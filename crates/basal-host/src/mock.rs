//! A deterministic mock of basal's hosts that records every effect.
//!
//! Behaviour is chosen by the call, so a test script states what it expects:
//!
//! | Call | Class | Behaviour |
//! |---|---|---|
//! | `ops.call('mock', 'echo', args)` | query | fulfils with `args` |
//! | `ops.call('mock', 'fail', args)` | query | rejects with `{message, code: "denied"}` |
//! | `ops.call('mock', 'send', args)` | mutation, honours keys | one effect per idempotency key; a repeat returns the first reply |
//! | `ops.call('mock', 'post', args)` | mutation, ignores keys | one effect per send |
//! | `ops.call('mock', 'long', args)` | mutation, honours keys | accepted as long-running; completes when the test says so |
//! | `llm(request)` | mutation, honours keys | as `long` |
//! | `sink.digest`, `sink.status` | mutation, honours keys | as `send` |
//! | `facts`, `classify`, any other op | query | fixed data, or `args` |
//!
//! Arguments may carry `delay_ms` (real time to wait before answering),
//! `gate` (a name: the call waits until the test opens that gate) and, on a
//! long call, `complete_after_ms` (the mock completes it by itself after that
//! long, fulfilling with `{done: args}`).
//!
//! The mock keeps every effect with the idempotency key it arrived with, so a
//! test can count effects per key and prove a crash never repeated one. With
//! [`MockHost::persistent`] its state survives the test process being killed:
//! the state file is rewritten and synced before any reply is returned, the
//! way a remote system's effect outlives the caller that lost its reply.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use serde_json::{Value, json};

use crate::{
    CallClass, CallRequest, Completion, CompletionSink, Dispatched, Host, HostOutcome,
    TransportError,
};

/// One effect the mock applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub key: String,
    pub module: String,
    pub op: String,
    pub args: String,
}

/// An injected transport failure for the next sends of one op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub proven_unsent: bool,
    /// Whether the effect happens before the failure is reported, as when a
    /// mutation commits remotely and its reply is lost.
    pub effect_applied: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct LongCall {
    run_id: String,
    position: u64,
    key: String,
    outcome: Option<(Settlement, String)>,
    acknowledged: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct State {
    effects: Vec<Effect>,
    /// The reply to each keyed mutation, returned again on a repeat.
    replies: BTreeMap<String, Value>,
    /// Every send that reached the mock: (key, module, op).
    sends: Vec<(String, String, String)>,
    long: BTreeMap<String, LongCall>,
    clock_ms: f64,
    clock_step_ms: f64,
    random_state: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            effects: Vec::new(),
            replies: BTreeMap::new(),
            sends: Vec::new(),
            long: BTreeMap::new(),
            clock_ms: 1_767_225_600_000.0, // 2026-01-01T00:00:00Z
            clock_step_ms: 1.0,
            random_state: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

/// Test controls that are not part of the simulated remote state.
#[derive(Default)]
struct Controls {
    faults: HashMap<(String, String), Vec<Fault>>,
    classes: HashMap<(String, String), CallClass>,
    open_gates: HashSet<String>,
}

struct Shared {
    state: Mutex<State>,
    controls: Mutex<Controls>,
    gates: Condvar,
    sink: Mutex<Option<Arc<dyn CompletionSink>>>,
    file: Option<PathBuf>,
}

/// The mock host. Cloning shares the same state.
#[derive(Clone)]
pub struct MockHost {
    shared: Arc<Shared>,
}

impl Default for MockHost {
    fn default() -> Self {
        Self::new()
    }
}

fn text(value: &Value) -> JsonText {
    JsonText::new(value.to_string()).unwrap_or_else(|_| JsonText::null())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic in another test thread must not make the mock unusable.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn op_names(kind: &CallKind) -> (String, String) {
    match kind {
        CallKind::Op { module, op } => (module.clone(), op.clone()),
        CallKind::Primitive(p) => ("primitive".to_owned(), p.name().to_owned()),
    }
}

impl MockHost {
    pub fn new() -> Self {
        Self::build(State::default(), None)
    }

    /// A mock whose remote state lives in `path`, loaded if it exists and
    /// rewritten (and synced) after every change.
    pub fn persistent(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let state = match std::fs::read_to_string(&path) {
            Ok(data) => {
                let value: Value = serde_json::from_str(&data).map_err(std::io::Error::other)?;
                decode_state(&value).ok_or_else(|| std::io::Error::other("bad mock state"))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e),
        };
        Ok(Self::build(state, Some(path)))
    }

    fn build(state: State, file: Option<PathBuf>) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(state),
                controls: Mutex::new(Controls::default()),
                gates: Condvar::new(),
                sink: Mutex::new(None),
                file,
            }),
        }
    }

    /// Persists `state`. Callers hold the state lock while saving, so two
    /// threads can never write their snapshots out of order.
    fn save(&self, state: &State) {
        let Some(path) = &self.shared.file else {
            return;
        };
        let tmp = path.with_extension("tmp");
        let written = std::fs::File::create(&tmp).and_then(|mut f| {
            f.write_all(encode_state(state).to_string().as_bytes())?;
            f.sync_all()
        });
        if written.and_then(|_| std::fs::rename(&tmp, path)).is_err() {
            // The mock stands in for a remote system; if it cannot persist,
            // the test cannot trust anything it reports afterwards.
            eprintln!("mock host: failed to persist state to {}", path.display());
            std::process::abort();
        }
    }

    /// Declares how an op is classified, overriding the table above. This is
    /// the stand-in for the operator-approved list of ops whose idempotency
    /// claim basal trusts.
    pub fn set_class(&self, module: &str, op: &str, class: CallClass) {
        lock(&self.shared.controls)
            .classes
            .insert((module.to_owned(), op.to_owned()), class);
    }

    /// Makes the next sends of (module, op) fail, one fault per send.
    pub fn inject(&self, module: &str, op: &str, faults: &[Fault]) {
        lock(&self.shared.controls)
            .faults
            .entry((module.to_owned(), op.to_owned()))
            .or_default()
            .extend_from_slice(faults);
    }

    pub fn open_gate(&self, gate: &str) {
        lock(&self.shared.controls)
            .open_gates
            .insert(gate.to_owned());
        self.shared.gates.notify_all();
    }

    /// Effects applied under `key`.
    pub fn effect_count(&self, key: &str) -> usize {
        lock(&self.shared.state)
            .effects
            .iter()
            .filter(|e| e.key == key)
            .count()
    }

    pub fn effects(&self) -> Vec<Effect> {
        lock(&self.shared.state).effects.clone()
    }

    /// Effects per idempotency key.
    pub fn effect_counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for e in &lock(&self.shared.state).effects {
            *counts.entry(e.key.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Sends that reached the mock under `key`, effects or not.
    pub fn send_count(&self, key: &str) -> usize {
        lock(&self.shared.state)
            .sends
            .iter()
            .filter(|(k, _, _)| k == key)
            .count()
    }

    pub fn total_sends(&self) -> usize {
        lock(&self.shared.state).sends.len()
    }

    /// Handles of long-running calls not yet completed.
    pub fn pending_long(&self) -> Vec<String> {
        lock(&self.shared.state)
            .long
            .iter()
            .filter(|(_, c)| c.outcome.is_none())
            .map(|(h, _)| h.clone())
            .collect()
    }

    /// The handle of the long-running call at (run, position), if accepted.
    pub fn handle_for(&self, run_id: &str, position: u64) -> Option<String> {
        lock(&self.shared.state)
            .long
            .iter()
            .find(|(_, c)| c.run_id == run_id && c.position == position)
            .map(|(h, _)| h.clone())
    }

    /// Completes a long-running call and delivers the completion to the
    /// attached sink. Returns false if the handle is unknown.
    pub fn complete(&self, handle: &str, outcome: HostOutcome) -> bool {
        {
            let mut state = lock(&self.shared.state);
            let Some(call) = state.long.get_mut(handle) else {
                return false;
            };
            if call.outcome.is_none() {
                call.outcome = Some((outcome.settlement, outcome.value.as_str().to_owned()));
                call.acknowledged = false;
            }
            self.save(&state);
            drop(state);
        }
        self.redeliver();
        true
    }

    /// Completes every pending long-running call with `{done: true}`.
    pub fn complete_all(&self) {
        for handle in self.pending_long() {
            self.complete(
                &handle,
                HostOutcome::fulfilled(text(&json!({"done": true}))),
            );
        }
    }

    /// Delivers every completed, unacknowledged long-running call to the
    /// sink again. A sink error leaves it for the next attempt.
    pub fn redeliver(&self) {
        let Some(sink) = lock(&self.shared.sink).clone() else {
            return;
        };
        let due: Vec<Completion> = lock(&self.shared.state)
            .long
            .iter()
            .filter(|(_, c)| !c.acknowledged)
            .filter_map(|(handle, c)| {
                let (settlement, value) = c.outcome.clone()?;
                Some(Completion {
                    run_id: c.run_id.clone(),
                    position: c.position,
                    handle: handle.clone(),
                    outcome: HostOutcome {
                        settlement,
                        value: JsonText::new(value).ok()?,
                    },
                })
            })
            .collect();
        for completion in due {
            if sink.complete(&completion).is_ok() {
                let mut state = lock(&self.shared.state);
                if let Some(c) = state.long.get_mut(&completion.handle) {
                    c.acknowledged = true;
                }
                self.save(&state);
                drop(state);
            }
        }
    }

    fn wait_gate(&self, gate: &str) {
        let mut controls = lock(&self.shared.controls);
        while !controls.open_gates.contains(gate) {
            controls = self
                .shared
                .gates
                .wait(controls)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    fn take_fault(&self, module: &str, op: &str) -> Option<Fault> {
        let mut controls = lock(&self.shared.controls);
        let queue = controls
            .faults
            .get_mut(&(module.to_owned(), op.to_owned()))?;
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    /// Applies a keyed mutation once and returns its reply.
    fn mutate(
        &self,
        state: &mut State,
        request: &CallRequest,
        module: &str,
        op: &str,
        honours_keys: bool,
        reply: Value,
    ) -> Value {
        if honours_keys && let Some(first) = state.replies.get(&request.idempotency_key) {
            return first.clone();
        }
        state.effects.push(Effect {
            key: request.idempotency_key.clone(),
            module: module.to_owned(),
            op: op.to_owned(),
            args: request.args.as_str().to_owned(),
        });
        state
            .replies
            .insert(request.idempotency_key.clone(), reply.clone());
        reply
    }

    fn default_class(kind: &CallKind) -> CallClass {
        let keyed = CallClass::Mutation {
            honours_idempotency_keys: true,
        };
        match kind {
            CallKind::Op { module, op } if module == "mock" => match op.as_str() {
                "send" | "long" => keyed,
                "post" => CallClass::Mutation {
                    honours_idempotency_keys: false,
                },
                _ => CallClass::Query,
            },
            CallKind::Op { .. } => CallClass::Query,
            CallKind::Primitive(p) => match p {
                Primitive::Llm | Primitive::SinkDigest | Primitive::SinkStatus => keyed,
                Primitive::KvSet | Primitive::KvDelete | Primitive::Sh => CallClass::Mutation {
                    honours_idempotency_keys: false,
                },
                _ => CallClass::Query,
            },
        }
    }
}

impl Host for MockHost {
    fn classify(&self, kind: &CallKind) -> CallClass {
        let names = op_names(kind);
        if let Some(class) = lock(&self.shared.controls).classes.get(&names) {
            return *class;
        }
        Self::default_class(kind)
    }

    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        let args: Value = serde_json::from_str(request.args.as_str()).unwrap_or(Value::Null);
        if let Some(ms) = args.get("delay_ms").and_then(Value::as_u64) {
            thread::sleep(Duration::from_millis(ms.min(60_000)));
        }
        if let Some(gate) = args.get("gate").and_then(Value::as_str) {
            self.wait_gate(gate);
        }
        let (module, op) = op_names(&request.kind);
        let class = self.classify(&request.kind);
        let fault = self.take_fault(&module, &op);
        if let Some(f) = fault
            && !f.effect_applied
        {
            return Err(TransportError::Unavailable {
                proven_unsent: f.proven_unsent,
                detail: "injected".into(),
            });
        }

        let mut state = lock(&self.shared.state);
        state
            .sends
            .push((request.idempotency_key.clone(), module.clone(), op.clone()));
        let honours = matches!(
            class,
            CallClass::Mutation {
                honours_idempotency_keys: true
            }
        );
        let long = matches!(&request.kind, CallKind::Primitive(Primitive::Llm))
            || matches!(&request.kind, CallKind::Op { module, op } if module == "mock" && op == "long");
        let result = if long {
            let handle = format!("h-{}", request.idempotency_key);
            let reply = self.mutate(
                &mut state,
                request,
                &module,
                &op,
                honours,
                json!({"handle": handle}),
            );
            let handle = reply
                .get("handle")
                .and_then(Value::as_str)
                .unwrap_or(&handle)
                .to_owned();
            state.long.entry(handle.clone()).or_insert(LongCall {
                run_id: request.run_id.clone(),
                position: request.position,
                key: request.idempotency_key.clone(),
                outcome: None,
                acknowledged: false,
            });
            if let Some(ms) = args.get("complete_after_ms").and_then(Value::as_u64) {
                let mock = self.clone();
                let handle = handle.clone();
                let done = text(&json!({"done": args}));
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(ms.min(60_000)));
                    mock.complete(&handle, HostOutcome::fulfilled(done));
                });
            }
            Dispatched::Accepted { handle }
        } else {
            match (&request.kind, class) {
                (CallKind::Op { module: m, op: o }, _) if m == "mock" && o == "fail" => {
                    let message = args
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("failed");
                    Dispatched::Completed(HostOutcome::rejected(text(
                        &json!({"message": message, "code": "denied"}),
                    )))
                }
                (_, CallClass::Mutation { .. }) => {
                    let reply = self.mutate(
                        &mut state,
                        request,
                        &module,
                        &op,
                        honours,
                        json!({"applied": args, "op": op}),
                    );
                    Dispatched::Completed(HostOutcome::fulfilled(text(&reply)))
                }
                (CallKind::Primitive(Primitive::Facts), _) => {
                    Dispatched::Completed(HostOutcome::fulfilled(text(
                        &json!({"agent": args.get("agent"), "activity": {"state": "idle"}}),
                    )))
                }
                (CallKind::Primitive(Primitive::Classify), _) => {
                    let label = args
                        .get("labels")
                        .and_then(|l| l.get(0))
                        .cloned()
                        .unwrap_or(Value::Null);
                    Dispatched::Completed(HostOutcome::fulfilled(text(&label)))
                }
                _ => Dispatched::Completed(HostOutcome::fulfilled(request.args.clone())),
            }
        };
        self.save(&state);
        drop(state);
        if fault.is_some() {
            return Err(TransportError::Unavailable {
                proven_unsent: false,
                detail: "injected after the effect".into(),
            });
        }
        Ok(result)
    }

    fn now_ms(&self) -> f64 {
        let mut state = lock(&self.shared.state);
        let now = state.clock_ms;
        state.clock_ms += state.clock_step_ms;
        self.save(&state);
        drop(state);
        now
    }

    fn random(&self) -> f64 {
        let mut state = lock(&self.shared.state);
        // xorshift64*: deterministic, and plenty for a mock.
        let mut x = state.random_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        state.random_state = x;
        self.save(&state);
        drop(state);
        let bits = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        bits as f64 / (1u64 << 53) as f64
    }

    fn attach(&self, sink: Arc<dyn CompletionSink>) {
        *lock(&self.shared.sink) = Some(sink);
        self.redeliver();
    }
}

impl MockHost {
    /// Sets the clock the next read returns.
    pub fn set_clock(&self, ms: f64) {
        let mut state = lock(&self.shared.state);
        state.clock_ms = ms;
        self.save(&state);
        drop(state);
    }

    /// Detaches the sink, as a host does when the runtime that attached it
    /// goes away.
    pub fn detach(&self) {
        *lock(&self.shared.sink) = None;
    }
}

fn settlement_code(s: Settlement) -> &'static str {
    match s {
        Settlement::Fulfilled => "fulfilled",
        Settlement::Rejected => "rejected",
    }
}

fn encode_state(state: &State) -> Value {
    json!({
        "effects": state.effects.iter().map(|e| json!([e.key, e.module, e.op, e.args])).collect::<Vec<_>>(),
        "replies": state.replies,
        "sends": state.sends.iter().map(|(k, m, o)| json!([k, m, o])).collect::<Vec<_>>(),
        "long": state.long.iter().map(|(h, c)| json!({
            "handle": h,
            "run_id": c.run_id,
            "position": c.position,
            "key": c.key,
            "outcome": c.outcome.as_ref().map(|(s, v)| json!([settlement_code(*s), v])),
            "acknowledged": c.acknowledged,
        })).collect::<Vec<_>>(),
        "clock_ms": state.clock_ms,
        "clock_step_ms": state.clock_step_ms,
        "random_state": state.random_state,
    })
}

fn decode_state(v: &Value) -> Option<State> {
    let strings = |item: &Value, n: usize| -> Option<Vec<String>> {
        let items = item.as_array()?;
        if items.len() != n {
            return None;
        }
        items
            .iter()
            .map(|s| s.as_str().map(str::to_owned))
            .collect()
    };
    let mut state = State::default();
    for e in v.get("effects")?.as_array()? {
        let s = strings(e, 4)?;
        state.effects.push(Effect {
            key: s[0].clone(),
            module: s[1].clone(),
            op: s[2].clone(),
            args: s[3].clone(),
        });
    }
    for (k, reply) in v.get("replies")?.as_object()? {
        state.replies.insert(k.clone(), reply.clone());
    }
    for s in v.get("sends")?.as_array()? {
        let s = strings(s, 3)?;
        state.sends.push((s[0].clone(), s[1].clone(), s[2].clone()));
    }
    for c in v.get("long")?.as_array()? {
        let outcome = match c.get("outcome")? {
            Value::Null => None,
            o => {
                let s = strings(o, 2)?;
                let settlement = match s[0].as_str() {
                    "fulfilled" => Settlement::Fulfilled,
                    "rejected" => Settlement::Rejected,
                    _ => return None,
                };
                Some((settlement, s[1].clone()))
            }
        };
        state.long.insert(
            c.get("handle")?.as_str()?.to_owned(),
            LongCall {
                run_id: c.get("run_id")?.as_str()?.to_owned(),
                position: c.get("position")?.as_u64()?,
                key: c.get("key")?.as_str()?.to_owned(),
                outcome,
                acknowledged: c.get("acknowledged")?.as_bool()?,
            },
        );
    }
    state.clock_ms = v.get("clock_ms")?.as_f64()?;
    state.clock_step_ms = v.get("clock_step_ms")?.as_f64()?;
    state.random_state = v.get("random_state")?.as_u64()?;
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: &str, key: &str) -> CallRequest {
        CallRequest {
            flow_id: "f".into(),
            run_id: "r".into(),
            position: 0,
            kind: CallKind::Op {
                module: "mock".into(),
                op: op.into(),
            },
            args: JsonText::new("{\"x\":1}").unwrap_or_else(|_| JsonText::null()),
            idempotency_key: key.into(),
            attempt: 1,
        }
    }

    #[test]
    fn keyed_mutations_apply_once_and_unkeyed_ones_every_time() {
        let mock = MockHost::new();
        for _ in 0..3 {
            assert!(mock.dispatch(&request("send", "k1")).is_ok());
            assert!(mock.dispatch(&request("post", "k2")).is_ok());
        }
        assert_eq!(mock.effect_count("k1"), 1);
        assert_eq!(mock.effect_count("k2"), 3);
        assert_eq!(mock.send_count("k1"), 3);
    }

    #[test]
    fn a_fault_after_the_effect_reports_unavailable_but_keeps_the_effect() {
        let mock = MockHost::new();
        mock.inject(
            "mock",
            "post",
            &[Fault {
                proven_unsent: false,
                effect_applied: true,
            }],
        );
        assert!(matches!(
            mock.dispatch(&request("post", "k")),
            Err(TransportError::Unavailable {
                proven_unsent: false,
                ..
            })
        ));
        assert_eq!(mock.effect_count("k"), 1);
    }

    #[test]
    fn persistent_state_survives_a_reload() {
        let dir =
            std::env::temp_dir().join(format!("basal-mock-{}-{}", std::process::id(), line!()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mock.json");
        let _ = std::fs::remove_file(&path);
        {
            let mock = MockHost::persistent(&path).expect("open");
            assert!(mock.dispatch(&request("send", "k1")).is_ok());
            assert!(mock.dispatch(&request("long", "k2")).is_ok());
            let _ = mock.now_ms();
        }
        let mock = MockHost::persistent(&path).expect("reopen");
        assert_eq!(mock.effect_count("k1"), 1);
        assert_eq!(mock.pending_long(), vec!["h-k2".to_owned()]);
        assert!(mock.dispatch(&request("send", "k1")).is_ok());
        assert_eq!(mock.effect_count("k1"), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
