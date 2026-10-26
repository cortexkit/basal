//! A deterministic Broca with the service's request and response JSON formats.
//! Admitted runs, results and usage can be synced to a file, preserving the
//! simulated remote service's state when the caller process is killed.
//!
//! `run.result` follows Broca's contract: `final_message` only on a completed
//! run, its text the final message's text parts joined with nothing between
//! them (`""` when it has none), `error` only on an `error` run, `reason`
//! only on a paused one, and `unknown_run` for a run the session does not
//! have. It answers for archived sessions too. `run.event.get` answers a
//! published event only on the route of the session it belongs to, and
//! `absent` everywhere else, as Broca does.

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
    /// The session is archived. `run.result` and `run.status` still answer.
    archived: bool,
    /// `run.result` fails as unreachable, as when Broca is down.
    #[serde(default)]
    failing: bool,
    /// `send.status` answers `unknown_send` for this send.
    #[serde(default)]
    send_forgotten: bool,
}
/// A published run event's body and the session it belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Event {
    route: Route,
    jcs: String,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    runs: BTreeMap<String, Run>,
    sends: Vec<(Route, Vec<u8>)>,
    events: BTreeMap<String, Event>,
    event_gets: usize,
    send_statuses: usize,
    refuse_next_send: Option<String>,
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
    /// How many times `send.status` was called.
    pub fn send_statuses(&self) -> usize {
        lock(&self.state).send_statuses
    }
    /// A queued submission starts its run: `send.status` and a re-send now
    /// answer `active` with the run id.
    pub fn start(&self, send_id: &str) -> Result<(), BrocaError> {
        self.with_run(send_id, |r| r.pending = false)
    }
    /// `send.status` answers `unknown_send` for this send from now on.
    pub fn forget_send(&self, send_id: &str) {
        let _ = self.with_run(send_id, |r| r.send_forgotten = true);
    }
    /// `run.result` for this call fails as unreachable until `false`.
    pub fn set_failing(&self, send_id: &str, failing: bool) -> Result<(), BrocaError> {
        self.with_run(send_id, |r| r.failing = failing)
    }
    /// The next `session.send` is refused with `code`, whether or not Broca
    /// accepted the send before.
    pub fn refuse_next_send(&self, code: &str) {
        lock(&self.state).refuse_next_send = Some(code.into());
    }
    /// How many times `run.event.get` was called.
    pub fn event_gets(&self) -> usize {
        lock(&self.state).event_gets
    }
    /// Stores an event body as Broca would, readable with `run.event.get`
    /// only on `route`.
    pub fn insert_event(&self, route: &Route, event_key: &str, jcs: &str) {
        lock(&self.state).events.insert(
            event_key.into(),
            Event {
                route: route.clone(),
                jcs: jcs.into(),
            },
        );
    }
    /// Publishes `run.ended` for the call's run in its current state and
    /// returns the wake the event reader would commit for it.
    pub fn publish_ended(&self, send_id: &str) -> Result<RunWake, BrocaError> {
        let (route, run_id, state) = {
            let s = lock(&self.state);
            let run = s
                .runs
                .get(send_id)
                .ok_or_else(|| BrocaError::Invalid("unknown send id".into()))?;
            (
                run.route.clone(),
                run.run_id.clone(),
                run.result.state.clone(),
            )
        };
        let session_ref = fake_session_ref(&route);
        let seq = self.next_seq();
        let jcs = serde_json::to_string(&json!({
            "at_ms": 1, "run_id": run_id, "seq": seq, "session_ref": session_ref,
            "structured_output": "absent", "terminal": state, "v": 1,
        }))
        .map_err(|e| BrocaError::Wire(e.to_string()))?;
        Ok(self.publish(
            send_id,
            &route,
            WakeKind::Ended,
            Some(run_id),
            session_ref,
            seq,
            jcs,
        ))
    }
    /// Publishes `run.skipped` for the call's session and returns the wake
    /// the event reader would commit for it.
    pub fn publish_skipped(&self, send_id: &str) -> Result<RunWake, BrocaError> {
        let route = lock(&self.state)
            .runs
            .get(send_id)
            .map(|run| run.route.clone())
            .ok_or_else(|| BrocaError::Invalid("unknown send id".into()))?;
        let session_ref = fake_session_ref(&route);
        let seq = self.next_seq();
        let jcs = serde_json::to_string(&json!({
            "count": 1, "from_ms": 1, "from_seq": seq, "session_ref": session_ref,
            "to_ms": 1, "to_seq": seq,
        }))
        .map_err(|e| BrocaError::Wire(e.to_string()))?;
        Ok(self.publish(
            send_id,
            &route,
            WakeKind::Skipped,
            None,
            session_ref,
            seq,
            jcs,
        ))
    }
    fn next_seq(&self) -> u64 {
        lock(&self.state).events.len() as u64 + 1
    }
    #[allow(clippy::too_many_arguments)]
    fn publish(
        &self,
        send_id: &str,
        route: &Route,
        kind: WakeKind,
        run_id: Option<String>,
        session_ref: String,
        seq: u64,
        jcs: String,
    ) -> RunWake {
        use sha2::{Digest, Sha256};
        let digest = format!("{:x}", Sha256::digest(jcs.as_bytes()));
        let event_key = format!("{:x}", Sha256::digest(format!("{kind:?}:{seq}:{digest}")));
        self.insert_event(route, &event_key, &jcs);
        RunWake {
            send_id: send_id.into(),
            kind,
            event_key,
            digest,
            run_id,
            session_ref: Some(session_ref),
            seq: Some(seq.to_string()),
        }
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
        self.with_run(send_id, |r| r.archived = true)
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
    /// Ends the run with `state` and `error` as the provider's own error. Broca
    /// reports the run's state and this error under `error`.
    pub fn finish_with_error(
        &self,
        send_id: &str,
        state: &str,
        error: ProviderError,
        usage: Option<Usage>,
    ) -> Result<(), BrocaError> {
        let status = RunStatusResponse::Error {
            metadata: Terminal {
                usage,
                ..Terminal::default()
            },
        };
        self.set(send_id, state, "", Some(error), None, status)
    }
    /// Sets both answers at once: `run.result` reports `state` (with
    /// `text` as the final message when completed, and the given error and
    /// pause reason), `run.status` reports `status`. Publishing the run's
    /// events is the caller's choice (`publish_ended`).
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
        if let Some(code) = s.refuse_next_send.take() {
            self.save(&s)?;
            return Err(refused(&code));
        }
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
                failing: false,
                send_forgotten: false,
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
    fn send_status(&self, route: &Route, params: &SendStatusParams) -> Result<Value, BrocaError> {
        let mut s = lock(&self.state);
        s.send_statuses += 1;
        let reply = s
            .runs
            .get(&params.send_id)
            .filter(|r| &r.route == route && !r.send_forgotten)
            .map(|r| {
                let delivered = json!({"as": "turn", "ref": r.run_id});
                match r.status.terminal() {
                    Some((reason, _)) => json!({"state": "finished", "run_id": r.run_id,
                        "reason": state_name(reason), "delivered": delivered}),
                    None if r.pending => {
                        json!({"state": "pending", "submission_id": r.submission_id})
                    }
                    None => json!({"state": "active", "run_id": r.run_id, "delivered": delivered}),
                }
            });
        self.save(&s)?;
        reply.ok_or_else(|| refused(UNKNOWN_SEND))
    }
    fn event_get(
        &self,
        route: &Route,
        params: &RunEventGetParams,
    ) -> Result<RunEventReply, BrocaError> {
        let mut s = lock(&self.state);
        s.event_gets += 1;
        let reply = match s.events.get(&params.event_key) {
            Some(event) if &event.route == route => RunEventReply {
                state: "found".into(),
                event_key: Some(params.event_key.clone()),
                jcs: Some(event.jcs.clone()),
            },
            _ => RunEventReply {
                state: "absent".into(),
                event_key: None,
                jcs: None,
            },
        };
        self.save(&s)?;
        Ok(reply)
    }
    fn result(
        &self,
        route: &Route,
        params: &RunResultParams,
    ) -> Result<RunResultResponse, BrocaError> {
        let mut s = lock(&self.state);
        s.result_calls += 1;
        if s.runs.values().any(|r| &r.route == route && r.failing) {
            self.save(&s)?;
            return Err(BrocaError::Unavailable {
                proven_unsent: false,
                detail: "fake Broca unreachable".into(),
            });
        }
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
}
/// A stand-in for Broca's session_ref: 32 lowercase hex characters, fixed
/// per session.
fn fake_session_ref(route: &Route) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(&route.session))[..32].to_owned()
}

/// Saved calls retained in memory while tests drop and reopen a host.
/// Process-kill tests use the core's SQLite implementation instead.
#[derive(Default)]
pub struct MemoryStore {
    calls: Mutex<BTreeMap<String, StoredCall>>,
    wakes: Mutex<Vec<RunWake>>,
    deadlines: Mutex<BTreeMap<String, i64>>,
    ended: Mutex<std::collections::BTreeSet<String>>,
    session_refs: Mutex<BTreeMap<String, String>>,
}
impl MemoryStore {
    /// Commits a wake as the event reader would.
    pub fn add_wake(&self, wake: RunWake) {
        let mut wakes = lock(&self.wakes);
        if !wakes
            .iter()
            .any(|w| w.event_key == wake.event_key && w.send_id == wake.send_id)
        {
            wakes.push(wake);
        }
    }
    pub fn set_deadline(&self, run_id: &str, deadline: i64) {
        lock(&self.deadlines).insert(run_id.into(), deadline);
    }
    /// The basal run has ended (see `StateStore::run_ended`).
    pub fn end_run(&self, run_id: &str) {
        lock(&self.ended).insert(run_id.into());
    }
    /// The session_ref learned for a call, if any.
    pub fn session_ref(&self, send_id: &str) -> Option<String> {
        lock(&self.session_refs).get(send_id).cloned()
    }
}
impl StateStore for MemoryStore {
    fn deadline(&self, run_id: &str) -> Result<Option<i64>, BrocaError> {
        Ok(lock(&self.deadlines).get(run_id).copied())
    }
    fn run_ended(&self, run_id: &str) -> Result<bool, BrocaError> {
        Ok(lock(&self.ended).contains(run_id))
    }
    fn learn_session_ref(&self, send_id: &str, session_ref: &str) -> Result<(), BrocaError> {
        lock(&self.session_refs)
            .entry(send_id.into())
            .or_insert_with(|| session_ref.into());
        Ok(())
    }
    fn wakes(&self) -> Result<Vec<RunWake>, BrocaError> {
        Ok(lock(&self.wakes).clone())
    }
    fn clear_wake(&self, event_key: &str, send_id: &str) -> Result<(), BrocaError> {
        lock(&self.wakes).retain(|w| !(w.event_key == event_key && w.send_id == send_id));
        Ok(())
    }
    fn get(&self, send_id: &str) -> Result<Option<StoredCall>, BrocaError> {
        Ok(lock(&self.calls).get(send_id).cloned())
    }
    fn pending_ids(&self) -> Result<Vec<String>, BrocaError> {
        Ok(lock(&self.calls)
            .values()
            .filter(|call| !call.acknowledged && !call.deferred)
            .map(|call| call.send_id.clone())
            .collect())
    }
    fn load(&self) -> Result<Vec<StoredCall>, BrocaError> {
        Ok(lock(&self.calls).values().cloned().collect())
    }
    fn save(&self, call: &StoredCall) -> Result<(), BrocaError> {
        lock(&self.calls).insert(call.send_id.clone(), call.clone());
        Ok(())
    }
}
