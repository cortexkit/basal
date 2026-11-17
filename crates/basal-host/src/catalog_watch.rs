//! One process-wide copy of the daemon catalog, kept current by `catalog.watch`.
//!
//! Without it every install validation, codemode admission and dispatch
//! freshness check read the whole `catalog.list` (hundreds of kilobytes on a
//! full fleet). The daemon pushes a generation number on `catalog.watch`
//! whenever its catalog changes; basal reads the catalog once per pushed
//! generation and every request shares that typed snapshot.
//!
//! The cache serves a snapshot only while a watch is open and the snapshot is
//! at least as new as the newest generation the watch announced. In every
//! other state (no watch yet, a watch that ended or could not be opened, a
//! push whose read has not landed or failed) callers read the catalog
//! themselves, exactly as before the cache existed. While a watch is open the
//! only work at idle is a parked read on the subscription: no timer, no
//! polling. Retrying a watch that could not be opened sleeps with backoff.

use crate::catalog_snapshot::CatalogSnapshot;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subc_client_rs::consumer::SubcConsumer;

/// First wait before reopening a watch that ended or could not be opened.
const RETRY_MIN: Duration = Duration::from_secs(1);
/// Longest wait between attempts. A daemon without `catalog.watch` refuses
/// every attempt, so this bounds the cost of retrying against it.
const RETRY_MAX: Duration = Duration::from_secs(300);

#[derive(Default)]
struct State {
    /// A subscription is open and has delivered at least one generation.
    watching: bool,
    /// The newest generation the open subscription announced.
    announced: u64,
    /// The newest catalog read while the subscription was open.
    snapshot: Option<Arc<CatalogSnapshot>>,
    /// Routing identity survives watch gaps; unlike declarations it grants no authority.
    core_module: Option<&'static str>,
}

#[derive(Default)]
pub(crate) struct SharedCatalog {
    state: Mutex<State>,
    stopped: AtomicBool,
    stop: tokio::sync::Notify,
}

impl SharedCatalog {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The shared snapshot, when one is open and no older than the newest
    /// generation the watch announced. `None` means: read the catalog.
    pub(crate) fn current(&self) -> Option<Arc<CatalogSnapshot>> {
        let state = self.state();
        state
            .snapshot
            .as_ref()
            .filter(|snapshot| state.watching && snapshot.generation >= state.announced)
            .cloned()
    }

    pub(crate) fn core_module(&self) -> &'static str {
        // Until the first catalog read or watch snapshot arrives, use the
        // legacy id. If only the renamed module is registered, its route miss
        // defers work until the catalog identifies the new routing target.
        self.state()
            .core_module
            .unwrap_or(crate::core_identity::LEGACY_CORE_MODULE)
    }

    /// Whether `seen` is the snapshot the cache is serving now, so a lookup
    /// that missed in it may be worth one fresh read.
    pub(crate) fn is_serving(&self, seen: &Arc<CatalogSnapshot>) -> bool {
        self.current()
            .is_some_and(|current| Arc::ptr_eq(&current, seen))
    }

    /// Remember routing identity from each read, including without a watch.
    /// Share declarations only while a watch is open: without one nothing
    /// would say when they go stale.
    /// `replace` installs a read of the same generation too (a route miss
    /// refresh); otherwise only a newer generation replaces the copy.
    pub(crate) fn offer(&self, snapshot: &Arc<CatalogSnapshot>, replace: bool) {
        let mut state = self.state();
        state.core_module = Some(crate::core_identity::routed_module(
            snapshot,
            crate::core_identity::LEGACY_CORE_MODULE,
        ));
        if !state.watching || snapshot.generation < state.announced {
            return;
        }
        let newer = state.snapshot.as_ref().is_none_or(|held| {
            snapshot.generation > held.generation
                || (replace && snapshot.generation == held.generation)
        });
        if newer {
            state.snapshot = Some(snapshot.clone());
        }
    }

    /// Record a pushed generation and say whether the held copy is older.
    /// The first push of a new subscription always discards the held copy: a
    /// restarted daemon may number its generations from the start again.
    fn announce(&self, generation: u64, first: bool) -> bool {
        let mut state = self.state();
        if first {
            state.snapshot = None;
            state.announced = generation;
        } else {
            state.announced = state.announced.max(generation);
        }
        state.watching = true;
        state
            .snapshot
            .as_ref()
            .is_none_or(|held| held.generation < state.announced)
    }

    /// The subscription ended: nothing announces staleness any more, so stop
    /// serving the copy until a new watch has delivered and been read again.
    fn end_watch(&self) {
        let mut state = self.state();
        state.watching = false;
        state.snapshot = None;
        state.announced = 0;
    }

    /// Ends the follower; called when the transport shuts down.
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.stop.notify_waiters();
        self.end_watch();
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

/// Follows `catalog.watch` until the transport stops. Each pushed generation
/// newer than the held copy costs one `catalog.list`; `changed` then runs
/// with that generation, so subscribers that react to a catalog change see
/// the new copy.
pub(crate) async fn follow(
    consumer: Arc<SubcConsumer>,
    shared: Arc<SharedCatalog>,
    changed: impl Fn(u64),
) {
    let mut retry = RETRY_MIN;
    while !shared.stopped() {
        let stopping = shared.stop.notified();
        tokio::pin!(stopping);
        stopping.as_mut().enable();
        if shared.stopped() {
            return;
        }
        let opened = tokio::select! {
            () = &mut stopping => return,
            opened = consumer.catalog_watch() => opened,
        };
        match opened {
            Ok(mut subscription) => {
                let mut first = true;
                loop {
                    let next = tokio::select! {
                        () = &mut stopping => {
                            let _ = subscription.unsubscribe();
                            return;
                        }
                        next = subscription.next() => next,
                    };
                    let push = match next {
                        Ok(Some(push)) => push,
                        Ok(None) => break,
                        Err(error) => {
                            tracing::debug!(%error, "catalog watch ended");
                            break;
                        }
                    };
                    retry = RETRY_MIN;
                    if shared.announce(push.generation, first) {
                        match consumer.catalog_list().await {
                            Ok(list) => {
                                let snapshot = CatalogSnapshot::new(list);
                                shared.offer(&snapshot, false);
                            }
                            // Requests read for themselves until a later
                            // push (or a request's own read) refreshes it.
                            Err(error) => {
                                tracing::warn!(%error, "catalog read after a watch push failed")
                            }
                        }
                    }
                    first = false;
                    changed(push.generation);
                }
                shared.end_watch();
            }
            Err(error) => tracing::debug!(%error, "catalog watch could not be opened"),
        }
        tokio::select! {
            () = &mut stopping => return,
            () = tokio::time::sleep(retry) => {}
        }
        retry = (retry * 2).min(RETRY_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(generation: u64) -> Arc<CatalogSnapshot> {
        CatalogSnapshot::from_value(serde_json::json!({"generation":generation,"modules":[]}))
            .unwrap()
    }

    #[test]
    fn serves_only_an_open_watch_copy_at_least_as_new_as_its_announcement() {
        let shared = SharedCatalog::default();
        shared.offer(&snapshot(3), false);
        assert!(shared.current().is_none(), "no watch, no shared copy");
        assert!(shared.announce(3, true));
        assert!(shared.current().is_none(), "announced but not read yet");
        shared.offer(&snapshot(3), false);
        assert_eq!(shared.current().unwrap().generation, 3);
        assert!(shared.announce(4, false), "a newer push needs a read");
        assert!(
            shared.current().is_none(),
            "never serve a copy older than announced"
        );
        shared.offer(&snapshot(4), false);
        assert!(!shared.announce(4, false), "a repeated push needs no read");
        assert_eq!(shared.current().unwrap().generation, 4);
        shared.end_watch();
        assert!(shared.current().is_none(), "an ended watch stops serving");
        assert!(
            shared.announce(1, true),
            "a new watch reads even an older number"
        );
    }

    #[test]
    fn core_routing_identity_survives_watch_gaps_and_updates_without_a_watch() {
        let shared = SharedCatalog::default();
        assert_eq!(shared.core_module(), "prefrontal-core");
        let renamed = CatalogSnapshot::from_value(serde_json::json!({
            "generation":3,"modules":[{"module_id":"prefrontal"}]
        }))
        .unwrap();
        shared.offer(&renamed, false);
        assert!(shared.current().is_none());
        assert_eq!(shared.core_module(), "prefrontal");
        assert!(shared.announce(4, true));
        assert_eq!(shared.core_module(), "prefrontal");
        shared.end_watch();
        assert_eq!(shared.core_module(), "prefrontal");
        let legacy = CatalogSnapshot::from_value(serde_json::json!({
            "generation":1,"modules":[{"module_id":"prefrontal-core"}]
        }))
        .unwrap();
        shared.offer(&legacy, false);
        assert_eq!(shared.core_module(), "prefrontal-core");
    }
}
