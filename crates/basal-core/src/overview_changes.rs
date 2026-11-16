//! In-process invalidations for the operator's flow overview. SQLite callbacks
//! only mark a flag; successful commits wake readers after releasing the writer.

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

#[derive(Default)]
pub struct OverviewChanges {
    dirty: Arc<AtomicBool>,
    outbox_dirty: Arc<AtomicBool>,
    outbox: Mutex<(bool, Option<Waker>)>,
    subscribers: AtomicUsize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    generation: u64,
    next_id: u64,
    waiting: BTreeMap<u64, Waker>,
}

impl OverviewChanges {
    #[cfg(any(test, feature = "test-support"))]
    pub fn waiting_subscriptions_for_test(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .waiting
            .len()
    }

    pub(crate) fn install(&self, connection: &rusqlite::Connection) {
        let dirty = self.dirty.clone();
        let outbox_dirty = self.outbox_dirty.clone();
        connection.update_hook(Some(move |_, database: &str, table: &str, _| {
            if database == "main" && matches!(table, "outbox" | "template_event_outbox") {
                outbox_dirty.store(true, Ordering::Relaxed);
            }
            if database == "main"
                && matches!(
                    table,
                    "flows"
                        | "installs"
                        | "install_cards"
                        | "package_versions"
                        | "template_instances"
                        | "run_history"
                )
            {
                dirty.store(true, Ordering::Relaxed);
            }
        }));
    }

    // Called under the writer connection's mutex, before it can serve another
    // transaction. A failed transaction consumes its flag but never publishes it.
    pub(crate) fn take_dirty(&self) -> bool {
        self.subscribers.load(Ordering::Acquire) != 0 && self.dirty.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn take_outbox_dirty(&self) -> bool {
        self.outbox_dirty.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn publish_outbox(&self) {
        let wake = {
            let mut state = self.outbox.lock().unwrap_or_else(|p| p.into_inner());
            state.0 = true;
            state.1.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    /// A retained permit covers commits between the publisher's query and wait.
    /// Only the bus publisher consumes this signal, independently of overview watches.
    pub async fn outbox_changed(&self) {
        poll_fn(|cx| {
            let mut state = self.outbox.lock().unwrap_or_else(|p| p.into_inner());
            if std::mem::take(&mut state.0) {
                Poll::Ready(())
            } else {
                state.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }

    pub fn subscribe(self: &Arc<Self>) -> OverviewSubscription {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let id = state.next_id;
        state.next_id += 1;
        if self.subscribers.fetch_add(1, Ordering::AcqRel) == 0 {
            // No older reader needs pre-subscription changes. The first watch's
            // initial read includes them, even if their writer is still running.
            self.dirty.store(false, Ordering::Relaxed);
        }
        OverviewSubscription {
            changes: self.clone(),
            id,
            seen: state.generation,
        }
    }

    /// Wake subscribers only; callers may hold runtime locks, so no reader or
    /// observer callback executes here. With no watch, no mutex is acquired.
    pub(crate) fn publish(&self) {
        if self.subscribers.load(Ordering::Acquire) == 0 {
            return;
        }
        let waiting = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.generation += 1;
            std::mem::take(&mut state.waiting)
        };
        for waker in waiting.into_values() {
            waker.wake();
        }
    }
}

pub struct OverviewSubscription {
    changes: Arc<OverviewChanges>,
    id: u64,
    seen: u64,
}

impl OverviewSubscription {
    /// Cover changes already pending before opening the next read snapshot.
    pub fn acknowledge(&mut self) {
        self.seen = self
            .changes
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation;
    }

    /// Generations retained between polls prevent a commit during a read or a
    /// slow emit from being lost. Cancelling this future keeps the subscription.
    pub async fn changed(&mut self) {
        poll_fn(|cx| {
            let mut state = self.changes.state.lock().unwrap_or_else(|p| p.into_inner());
            if self.seen != state.generation {
                self.seen = state.generation;
                state.waiting.remove(&self.id);
                Poll::Ready(())
            } else {
                state.waiting.insert(self.id, cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

impl Drop for OverviewSubscription {
    fn drop(&mut self) {
        let mut state = self.changes.state.lock().unwrap_or_else(|p| p.into_inner());
        state.waiting.remove(&self.id);
        self.changes.subscribers.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
#[path = "overview_changes_tests.rs"]
mod tests;

#[cfg(test)]
mod outbox_tests {
    use super::*;
    use std::future::Future;
    use std::task::Context;

    #[test]
    fn outbox_signal_is_retained_coalesced_and_separate_from_overview() {
        let changes = Arc::new(OverviewChanges::default());
        let mut overview = changes.subscribe();
        changes.publish_outbox();
        changes.publish_outbox();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            Box::pin(changes.outbox_changed())
                .as_mut()
                .poll(&mut cx)
                .is_ready()
        );
        assert!(
            Box::pin(changes.outbox_changed())
                .as_mut()
                .poll(&mut cx)
                .is_pending()
        );
        assert!(
            Box::pin(overview.changed())
                .as_mut()
                .poll(&mut cx)
                .is_pending()
        );
        changes.publish();
        assert!(
            Box::pin(overview.changed())
                .as_mut()
                .poll(&mut cx)
                .is_ready()
        );
        assert!(
            Box::pin(changes.outbox_changed())
                .as_mut()
                .poll(&mut cx)
                .is_pending()
        );
    }

    #[test]
    fn only_committed_outbox_writes_wake_the_publisher() {
        let dir = crate::test_support::timestamp_scratch("outbox-changes", "commits");
        let store =
            crate::Store::open(dir.join("store.db"), crate::Durability::throwaway()).unwrap();
        let ready = || {
            Box::pin(store.overview_changes().outbox_changed())
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        };
        let insert = |tx: &rusqlite::Transaction<'_>| -> crate::Result<()> {
            tx.execute(
                "INSERT INTO outbox(at,kind,flow_id,body) VALUES (1,'flow.owner_notice','','{}')",
                [],
            )?;
            Ok(())
        };
        assert!(!ready());
        let result: crate::Result<()> = store.write(|tx| {
            insert(tx)?;
            Err(crate::CoreError::Invalid("rollback".into()))
        });
        assert!(result.is_err());
        assert!(!ready());
        store
            .write(|tx| {
                tx.execute(
                    "INSERT INTO meta(key,value) VALUES ('unrelated','value')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(!ready(), "rollback must not leak into a later commit");
        store.write(insert).unwrap();
        assert!(ready(), "a durable outbox commit wakes without a timer");
        assert!(!ready());
    }
}
