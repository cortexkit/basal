//! A deterministic Broca with the service's request and response JSON formats.
//! Admitted runs, results and usage can be synced to a file, preserving the
//! simulated remote service's state when the caller process is killed.
//!
//! `run.result` follows Broca's contract: `final_message` only on a completed
//! run, its text the final message's text parts joined with nothing between
//! them (`""` when it has none), `error` only on an `error` run, `reason`
//! only on a paused one, and `unknown_run` for a run the session does not
//! have. It answers for archived sessions too, which can no longer be
//! watched.

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Run {
    route: Route,
    bytes: Vec<u8>,
    run_id: String,
    submission_id: String,
    pending: bool,
    /// What `run.status` answers. Token usage is reported only here, never
    /// by `run.result`.
    status: RunStatusResponse,
    /// What `run.result` answers.
    result: RunResultResponse,
    /// `run.result` refuses the run as `unknown_run`.
    forgotten: bool,
    /// `session.subscribe` is refused, as for an archived session.
    archived: bool,
    /// The finish is not delivered to the session's subscription.
    drop_finish: bool,
    /// A subscription is open on the session.
    watched: bool,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    runs: BTreeMap<String, Run>,
    sends: Vec<(Route, Vec<u8>)>,
    watches: Vec<Route>,
    wakes: usize,
    status_calls: usize,
    result_calls: usize,
    pending_next: bool,
    cut_next_reply: bool,
}
#[derive(Clone)]
pub struct FakeBroca {
    state: Arc<Mutex<State>>,
    path: Option<PathBuf>,
}
impl Default for FakeBroca {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            path: None,
        }
    }
}
fn state_name(reason: RunFinishReason) -> &'static str {
    match reason {
        RunFinishReason::Completed => "completed",
        RunFinishReason::MaxSteps => "max_steps",
        RunFinishReason::Cancelled => "cancelled",
        RunFinishReason::Interrupted => "interrupted",
        RunFinishReason::Error => "error",
        RunFinishReason::TransformUnavailable => "transform_unavailable",
    }
}
impl FakeBroca {
    pub fn persistent(path: impl AsRef<Path>) -> Result<Self, BrocaError> {
        let path = path.as_ref().to_owned();
        let state = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| BrocaError::Store(e.to_string()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(BrocaError::Store(e.to_string())),
        };
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            path: Some(path),
        })
    }
    fn save(&self, state: &State) -> Result<(), BrocaError> {
        if let Some(path) = &self.path {
            use std::io::Write;
            let bytes = serde_json::to_vec(state).map_err(|e| BrocaError::Store(e.to_string()))?;
            let tmp = path.with_extension("tmp");
            let mut file =
                std::fs::File::create(&tmp).map_err(|e| BrocaError::Store(e.to_string()))?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|e| BrocaError::Store(e.to_string()))?;
            std::fs::rename(&tmp, path).map_err(|e| BrocaError::Store(e.to_string()))?;
        }
        Ok(())
    }
    fn with_run(&self, send_id: &str, f: impl FnOnce(&mut Run)) -> Result<(), BrocaError> {
        let mut s = lock(&self.state);
        let run = s
            .runs
            .get_mut(send_id)
            .ok_or_else(|| BrocaError::Invalid("unknown send id".into()))?;
        f(run);
        self.save(&s)
    }
    pub fn pending_next(&self) {
        lock(&self.state).pending_next = true;
    }
    pub fn cut_next_reply(&self) {
        lock(&self.state).cut_next_reply = true;
    }
    pub fn sends(&self) -> Vec<(Route, Vec<u8>)> {
        lock(&self.state).sends.clone()
    }
    /// The session route of every subscription opened, in opening order.
    pub fn watches(&self) -> Vec<Route> {
        lock(&self.state).watches.clone()
    }
    /// How many run finishes reached an open subscription, each of which
    /// would have woken the host.
    pub fn wakes(&self) -> usize {
        lock(&self.state).wakes
    }
    /// How many times `run.status` and `run.result` were called, in that
    /// order.
    pub fn calls(&self) -> (usize, usize) {
        let s = lock(&self.state);
        (s.status_calls, s.result_calls)
    }
    pub fn keys(&self) -> Vec<String> {
        lock(&self.state).runs.keys().cloned().collect()
    }
    /// The session is archived: it can no longer be subscribed to, while
    /// `run.result` and `run.status` still answer.
    pub fn archive(&self, send_id: &str) -> Result<(), BrocaError> {
        self.with_run(send_id, |r| {
            r.archived = true;
            r.watched = false;
        })
    }
    /// The run's finish never reaches the subscription, as when the
    /// connection drops at that moment.
    pub fn drop_finish(&self, send_id: &str) -> Result<(), BrocaError> {
        self.with_run(send_id, |r| r.drop_finish = true)
    }
    /// `run.result` refuses the run as `unknown_run`, which Broca should
    /// never do for a run it accepted.
    pub fn forget(&self, send_id: &str) -> Result<(), BrocaError> {
        self.with_run(send_id, |r| r.forgotten = true)
    }
    /// Ends the run. A completed run's final message is `text`; an `error`
    /// run carries a permanent provider error. Usage is reported only
    /// through `run.status`, as Broca does.
    pub fn finish(
        &self,
        send_id: &str,
        text: &str,
        reason: RunFinishReason,
        usage: Option<Usage>,
    ) -> Result<(), BrocaError> {
        let metadata = Terminal {
            usage,
            ..Terminal::default()
        };
        let status = match reason {
            RunFinishReason::Completed => RunStatusResponse::Completed { metadata },
            RunFinishReason::MaxSteps => RunStatusResponse::MaxSteps { metadata },
            RunFinishReason::Cancelled => RunStatusResponse::Cancelled { metadata },
            RunFinishReason::Interrupted => RunStatusResponse::Interrupted { metadata },
            RunFinishReason::Error => RunStatusResponse::Error { metadata },
            RunFinishReason::TransformUnavailable => {
                RunStatusResponse::TransformUnavailable { metadata }
            }
        };
        let error = (reason == RunFinishReason::Error).then(|| ProviderError {
            class: "permanent".into(),
            message: "fake provider refused the request".into(),
            rest: serde_json::Map::from_iter([("status".to_owned(), json!(400))]),
        });
        self.set(send_id, state_name(reason), text, error, None, status)
    }
    /// Sets both answers at once: `run.result` reports `state` (with
    /// `text` as the final message when completed, and the given error and
    /// pause reason), `run.status` reports `status`. A terminal state
    /// delivers a finish to the session's subscription.
    pub fn set(
        &self,
        send_id: &str,
        state: &str,
        text: &str,
        error: Option<ProviderError>,
        reason: Option<String>,
        status: RunStatusResponse,
    ) -> Result<(), BrocaError> {
        let mut s = lock(&self.state);
        let run = s
            .runs
            .get_mut(send_id)
            .ok_or_else(|| BrocaError::Invalid("unknown send id".into()))?;
        let completed = state == "completed";
        run.result = RunResultResponse {
            run_id: run.run_id.clone(),
            state: state.into(),
            reason: (state == "paused").then_some(reason).flatten(),
            error: (state == "error").then_some(error).flatten(),
            final_message: completed.then(|| FinalMessage {
                ordinal: 1,
                mid: "m1".into(),
                text: text.into(),
            }),
        };
        run.status = status;
        let wakes = run.watched && !run.drop_finish && !["active", "paused"].contains(&state);
        if wakes {
            run.watched = false;
            s.wakes += 1;
        }
        self.save(&s)
    }
    /// Changes only what `run.status` answers, leaving `run.result` alone.
    pub fn set_status(&self, send_id: &str, status: RunStatusResponse) -> Result<(), BrocaError> {
        self.with_run(send_id, |r| r.status = status)
    }
}
fn refused(code: &str) -> BrocaError {
    BrocaError::Refused {
        code: code.into(),
        detail: format!("fake refused {code}"),
    }
}
impl Transport for FakeBroca {
    fn send(&self, route: &Route, params: &[u8]) -> Result<SendResult, BrocaError> {
        let params_decoded: SendParams =
            serde_json::from_slice(params).map_err(|e| BrocaError::Wire(e.to_string()))?;
        let key = params_decoded
            .send_id
            .ok_or_else(|| refused("invalid_params"))?;
        let mut s = lock(&self.state);
        s.sends.push((route.clone(), params.to_vec()));
        if let Some(old) = s.runs.get(&key) {
            if old.bytes != params || &old.route != route {
                return Err(refused("send_id_reuse"));
            }
        } else {
            let pending = s.pending_next;
            s.pending_next = false;
            let run_id = format!("run:{key}");
            let run = Run {
                route: route.clone(),
                bytes: params.to_vec(),
                run_id: run_id.clone(),
                submission_id: format!("submission:{key}"),
                pending,
                status: RunStatusResponse::Active,
                result: RunResultResponse {
                    run_id,
                    state: "active".into(),
                    reason: None,
                    error: None,
                    final_message: None,
                },
                forgotten: false,
                archived: false,
                drop_finish: false,
                watched: false,
            };
            s.runs.insert(key.clone(), run);
        }
        let r = s
            .runs
            .get(&key)
            .ok_or_else(|| BrocaError::Wire("fake run disappeared".into()))?;
        let result = if let Some((reason, _)) = r.status.terminal() {
            SendResult::Finished {
                run_id: r.run_id.clone(),
                reason,
            }
        } else if r.pending {
            SendResult::Pending {
                submission_id: r.submission_id.clone(),
            }
        } else {
            SendResult::Active {
                run_id: r.run_id.clone(),
            }
        };
        let cut = s.cut_next_reply;
        s.cut_next_reply = false;
        self.save(&s)?;
        if cut {
            return Err(BrocaError::Unavailable {
                proven_unsent: false,
                detail: "reply lost after admission".into(),
            });
        }
        Ok(result)
    }
    fn watch(&self, route: &Route) -> Result<(), BrocaError> {
        let mut s = lock(&self.state);
        let run = s
            .runs
            .values_mut()
            .find(|r| &r.route == route)
            .ok_or_else(|| refused("not_found"))?;
        if run.archived {
            return Err(refused("session_archived"));
        }
        if !run.watched {
            run.watched = true;
            s.watches.push(route.clone());
        }
        self.save(&s)
    }
    fn result(
        &self,
        route: &Route,
        params: &RunResultParams,
    ) -> Result<RunResultResponse, BrocaError> {
        let mut s = lock(&self.state);
        s.result_calls += 1;
        let result = s
            .runs
            .values()
            .find(|r| &r.route == route && r.run_id == params.run_id && !r.forgotten)
            .map(|r| r.result.clone());
        self.save(&s)?;
        result.ok_or_else(|| refused(UNKNOWN_RUN))
    }
    fn status(
        &self,
        route: &Route,
        params: &StatusParams,
    ) -> Result<RunStatusResponse, BrocaError> {
        let mut s = lock(&self.state);
        s.status_calls += 1;
        let status = s
            .runs
            .values()
            .find(|r| &r.route == route && params.run_id.as_ref() == Some(&r.run_id))
            .map(|r| r.status.clone())
            .unwrap_or(RunStatusResponse::Unknown);
        self.save(&s)?;
        Ok(status)
    }
    fn release(&self, route: &Route) {
        let mut s = lock(&self.state);
        if let Some(run) = s.runs.values_mut().find(|r| &r.route == route) {
            run.watched = false;
        }
        let _ = self.save(&s);
    }
}

/// Saved calls retained in memory while tests drop and reopen a host.
/// Process-kill tests use the core's SQLite implementation instead.
#[derive(Default)]
pub struct MemoryStore {
    calls: Mutex<BTreeMap<String, StoredCall>>,
}
impl StateStore for MemoryStore {
    fn load(&self) -> Result<Vec<StoredCall>, BrocaError> {
        Ok(lock(&self.calls).values().cloned().collect())
    }
    fn save(&self, call: &StoredCall) -> Result<(), BrocaError> {
        lock(&self.calls).insert(call.send_id.clone(), call.clone());
        Ok(())
    }
}
