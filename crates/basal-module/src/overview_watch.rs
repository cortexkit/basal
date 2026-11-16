//! The overview's watch uses the read's admission and projection. StreamData
//! carries app-read-watch-v1 envelopes; the SDK sends an empty StreamEnd when
//! the task returns Streamed, or its ordinary Error frame on a refusal.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use basal_proto::util::lock;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subc_client_rs::{HandlerOutcome, RequestCtx, RouteHandle};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::caller::Caller;
use crate::module::Module;
use crate::ops::OpError;

const MIN_INTERVAL: Duration = Duration::from_secs(3);
const RETRY_AFTER: Duration = Duration::from_secs(5);
const NOTICE_BOUND: usize = 196_608;
const MAX_WATCHES: usize = 8;

#[derive(Default)]
pub(crate) struct Registry {
    state: Mutex<RegistryState>,
    #[cfg(test)]
    pub(crate) evaluations: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) evaluated: tokio::sync::Notify,
    #[cfg(test)]
    pub(crate) parked: tokio::sync::Notify,
    #[cfg(test)]
    pub(crate) woken: tokio::sync::Notify,
    #[cfg(test)]
    pub(crate) starts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) window: Mutex<Option<(Instant, Instant)>>,
}

#[derive(Default)]
struct RegistryState {
    next: u64,
    stopping: bool,
    open: BTreeMap<u64, (RouteHandle, watch::Sender<bool>)>,
}

impl Registry {
    fn open(self: &Arc<Self>, route: RouteHandle) -> Result<Option<Slot>, HandlerOutcome> {
        let mut state = lock(&self.state);
        if state.stopping {
            return Ok(None);
        }
        if state.open.len() >= MAX_WATCHES {
            return Err(HandlerOutcome::Error {
                code: "watch_limit_reached".into(),
                message: "basal already serves 8 overview watches; close one first".into(),
            });
        }
        let id = state.next;
        state.next += 1;
        let (end, ended) = watch::channel(false);
        state.open.insert(id, (route, end));
        Ok(Some(Slot {
            registry: self.clone(),
            id,
            ended,
        }))
    }

    pub(crate) fn end_route(&self, route: &RouteHandle) {
        lock(&self.state).open.retain(|_, (bound, end)| {
            if bound == route {
                let _ = end.send(true);
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn stop(&self) {
        let mut state = lock(&self.state);
        state.stopping = true;
        for (_, end) in std::mem::take(&mut state.open).into_values() {
            let _ = end.send(true);
        }
    }
}

struct Slot {
    registry: Arc<Registry>,
    id: u64,
    ended: watch::Receiver<bool>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        lock(&self.registry.state).open.remove(&self.id);
    }
}

impl Slot {
    async fn stopped(&self, ctx: &RequestCtx) {
        let mut ended = self.ended.clone();
        tokio::select! {
            _ = ctx.cancelled() => {},
            _ = ended.wait_for(|end| *end) => {},
        }
    }
}

fn error(error: OpError) -> HandlerOutcome {
    super::serve::op_error_outcome(error)
}

fn transient(code: &str) -> bool {
    matches!(
        code,
        "store_error" | "storage_unavailable" | "agent_registry_unavailable" | "internal"
    )
}

fn comparison(mut answer: Value) -> Vec<u8> {
    // Snapshot time is not content: otherwise every wake would look different
    // even when the flows and registry decorations are unchanged.
    answer
        .as_object_mut()
        .expect("overview object")
        .remove("as_of_ms");
    serde_json::to_vec(&answer).expect("JSON value")
}

fn frame(answer: Value) -> Vec<u8> {
    let bytes = serde_json::to_vec(&answer).expect("JSON value");
    let digest = format!("{:x}", Sha256::digest(bytes));
    let snapshot = serde_json::to_vec(&json!({"frame":"snapshot","digest":digest,"body":answer}))
        .expect("JSON value");
    if snapshot.len() <= NOTICE_BOUND {
        snapshot
    } else {
        serde_json::to_vec(&json!({"frame":"hint","digest":digest})).expect("JSON value")
    }
}

pub(crate) async fn serve(
    registry: Arc<Registry>,
    module: Arc<Module>,
    caller: Caller,
    params: Value,
    ctx: RequestCtx,
) -> HandlerOutcome {
    if let Err(refusal) = module.validate_overview(&caller, params.clone()) {
        return error(refusal);
    }
    let slot = match registry.open(ctx.route_handle()) {
        Ok(Some(slot)) => slot,
        Ok(None) => return HandlerOutcome::Streamed,
        Err(refusal) => return refusal,
    };
    // Subscribe before the first read. Commits during a read or a parked emit
    // retain their generation and will be covered by the next evaluation.
    let mut changes = module.rt.store().overview_changes().subscribe();
    let mut baseline: Option<Vec<u8>> = None;
    let mut due = Instant::now();
    let mut retry = false;
    loop {
        if baseline.is_some() {
            if !retry {
                #[cfg(test)]
                registry.parked.notify_one();
                tokio::select! {
                    biased;
                    _ = slot.stopped(&ctx) => return HandlerOutcome::Streamed,
                    _ = changes.changed() => {
                        #[cfg(test)]
                        registry.woken.notify_one();
                    },
                }
            }
            tokio::select! {
                biased;
                _ = slot.stopped(&ctx) => return HandlerOutcome::Streamed,
                _ = tokio::time::sleep_until(due) => {},
            }
        }
        changes.acknowledge();
        #[cfg(test)]
        registry
            .starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let read = {
            let module = module.clone();
            let caller = caller.clone();
            let params = params.clone();
            tokio::task::spawn_blocking(move || module.handle(&caller, "flow.overview", params))
        };
        let answer = tokio::select! {
            biased;
            _ = slot.stopped(&ctx) => return HandlerOutcome::Streamed,
            result = read => result.unwrap_or_else(|e| Err(OpError::new("internal", format!("overview read thread ended: {e}")))),
        };
        #[cfg(test)]
        {
            registry
                .evaluations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            registry.evaluated.notify_one();
        }
        due = Instant::now() + MIN_INTERVAL;
        let answer = match answer {
            Ok(answer) => answer,
            Err(refusal) if baseline.is_some() && transient(&refusal.code) => {
                retry = true;
                due = Instant::now() + RETRY_AFTER;
                continue;
            }
            Err(refusal) => return error(refusal),
        };
        retry = false;
        let key = comparison(answer.clone());
        if baseline.as_ref() == Some(&key) {
            continue;
        }
        let emitted = tokio::select! {
            biased;
            _ = slot.stopped(&ctx) => return HandlerOutcome::Streamed,
            emitted = ctx.emit(frame(answer)) => emitted,
        };
        if emitted.is_err() {
            return HandlerOutcome::Streamed;
        }
        baseline = Some(key);
        // A slow consumer must not get back-to-back snapshots when changes
        // accumulated while its previous frame waited for transport capacity.
        let sent_at = Instant::now();
        due = sent_at + MIN_INTERVAL;
        #[cfg(test)]
        {
            *lock(&registry.window) = Some((sent_at, due));
        }
    }
}

#[cfg(test)]
#[path = "overview_watch_tests.rs"]
mod tests;
