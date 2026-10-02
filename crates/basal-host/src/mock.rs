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
//! | `llm(request)` | mutation, honours keys | as `long`, Broca-shaped (below) |
//! | `classify(text, labels)` | query | completes now with a label and separate usage metadata |
//! | `sink.digest`, `sink.status` | mutation, honours keys | as `send` |
//! | `facts`, any other op | query | fixed data, or `args` |
//!
//! Broca-shaped: the request is basal's envelope, whose `send_id` Broca
//! deduplicates on. The mock records every send's exact bytes per
//! `send_id`; a second send under the same `send_id` with different bytes
//! is refused with code `send_id_reuse`, as Broca refuses it. Usage is
//! reported with the outcome: the script's request may carry `usage` to set
//! it, otherwise input is a quarter of the envelope's bytes and output is
//! the smaller of the clamped `max_output` and 16. [`MockHost::complete_all`]
//! attaches it to an `llm` completion.
//!
//! Arguments may carry `delay_ms` (real time to wait before answering),
//! `gate` (a name: the call waits until the test opens that gate),
//! `rendezvous` (`{name, count}`: the call waits until `count` calls naming
//! the same rendezvous are inside the mock at once, or until
//! [`RENDEZVOUS_TIMEOUT`] passes; the peak number seen together is kept, so
//! a test can prove calls ran concurrently without timing them) and, on a
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
use std::time::{Duration, Instant};

use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use serde_json::{Value, json};

use crate::{
    CallClass, CallRequest, Completion, CompletionSink, Dispatched, Host, HostOutcome,
    InstallStatus, TransportError,
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
    /// The usage an `llm` call reports when it completes.
    usage: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
struct State {
    effects: Vec<Effect>,
    /// The reply to each keyed mutation, returned again on a repeat.
    replies: BTreeMap<String, Value>,
    /// Every send that reached the mock: (key, module, op).
    sends: Vec<(String, String, String)>,
    long: BTreeMap<String, LongCall>,
    /// Every Broca send: (send_id, exact request bytes).
    broca_sends: Vec<(String, String)>,
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
            broca_sends: Vec::new(),
            clock_ms: 1_767_225_600_000.0, // 2026-01-01T00:00:00Z
            clock_step_ms: 1.0,
            random_state: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

/// How long a call waits at a rendezvous for the others before answering
/// anyway. Bounded, so calls that never meet (because they were sent one
/// after another) still finish, with a peak below the count.
pub const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(10);

/// One named rendezvous.
#[derive(Default)]
struct Rendezvous {
    /// Calls currently waiting at it.
    inside: usize,
    /// The most calls that were inside at once.
    peak: usize,
    /// Set once `count` calls were inside together; later arrivals pass
    /// straight through.
    met: bool,
}

/// Test controls that are not part of the simulated remote state.
#[derive(Default)]
struct Controls {
    faults: HashMap<(String, String), Vec<Fault>>,
    classes: HashMap<(String, String), CallClass>,
    open_gates: HashSet<String>,
    rendezvous: HashMap<String, Rendezvous>,
    /// What core answers about each flow version; `None` is no answer (core
    /// unreachable). A version the test never set is unreachable too.
    installs: HashMap<(String, u32), Option<InstallStatus>>,
    /// Every install status question asked, in order.
    install_queries: Vec<(String, u32)>,
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

    /// Sets what core answers about `version` of `flow_id` from now on;
    /// `None` makes core unreachable for it.
    pub fn set_install_status(&self, flow_id: &str, version: u32, status: Option<InstallStatus>) {
        lock(&self.shared.controls)
            .installs
            .insert((flow_id.to_owned(), version), status);
    }

    /// Every install status question the runtime asked, in order.
    pub fn install_queries(&self) -> Vec<(String, u32)> {
        lock(&self.shared.controls).install_queries.clone()
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
                call.usage = outcome.usage.and_then(|u| serde_json::to_value(u).ok());
                call.outcome = Some((outcome.settlement, outcome.value.as_str().to_owned()));
                call.acknowledged = false;
            }
            self.save(&state);
            drop(state);
        }
        self.redeliver();
        true
    }

    /// Completes every pending long-running call with `{done: true}`, plus
    /// the reported `usage` for an `llm` call.
    pub fn complete_all(&self) {
        for handle in self.pending_long() {
            let usage = lock(&self.shared.state)
                .long
                .get(&handle)
                .and_then(|c| c.usage.clone());
            let mut outcome = HostOutcome::fulfilled(text(&json!({"done": true})));
            outcome.usage = usage.and_then(|u| serde_json::from_value(u).ok());
            self.complete(&handle, outcome);
        }
    }

    /// Every Broca send, as (send_id, exact request bytes), in send order.
    pub fn broca_sends(&self) -> Vec<(String, String)> {
        lock(&self.shared.state).broca_sends.clone()
    }

    /// The usage the mock reports for `llm` and `classify`: what the
    /// script's request asks for under `usage`, or a quarter of the
    /// envelope's bytes as input and at most 16 tokens of output.
    fn usage_for(envelope: &Value, bytes: usize) -> Value {
        if let Some(usage) = envelope.get("request").and_then(|r| r.get("usage"))
            && usage.is_object()
        {
            return usage.clone();
        }
        let max_output = envelope
            .get("max_output")
            .and_then(Value::as_u64)
            .unwrap_or(16);
        json!({
            "input_tokens": bytes.div_ceil(4),
            "cache_write_tokens": 0,
            "output_tokens": max_output.min(16),
            "cached_input_tokens": 0,
        })
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
                        usage: c.usage.clone().and_then(|u| serde_json::from_value(u).ok()),
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

    /// Waits at the named rendezvous until `count` calls are inside it at
    /// once, or until [`RENDEZVOUS_TIMEOUT`] passes.
    fn rendezvous(&self, name: &str, count: usize) {
        let deadline = Instant::now() + RENDEZVOUS_TIMEOUT;
        let mut controls = lock(&self.shared.controls);
        {
            let r = controls.rendezvous.entry(name.to_owned()).or_default();
            r.inside += 1;
            r.peak = r.peak.max(r.inside);
            if r.inside >= count {
                r.met = true;
            }
        }
        self.shared.gates.notify_all();
        loop {
            let met = controls.rendezvous.get(name).is_some_and(|r| r.met);
            let now = Instant::now();
            if met || now >= deadline {
                break;
            }
            controls = self
                .shared
                .gates
                .wait_timeout(controls, deadline - now)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|p| p.into_inner().0);
        }
        if let Some(r) = controls.rendezvous.get_mut(name) {
            r.inside = r.inside.saturating_sub(1);
        }
    }

    /// The most calls that were inside the named rendezvous at once.
    pub fn peak_concurrency(&self, name: &str) -> usize {
        lock(&self.shared.controls)
            .rendezvous
            .get(name)
            .map_or(0, |r| r.peak)
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
    fn install_status(&self, flow_id: &str, version: u32) -> Result<InstallStatus, TransportError> {
        let mut controls = lock(&self.shared.controls);
        controls.install_queries.push((flow_id.to_owned(), version));
        match controls.installs.get(&(flow_id.to_owned(), version)) {
            Some(Some(status)) => Ok(status.clone()),
            _ => Err(TransportError::Unavailable {
                proven_unsent: true,
                detail: format!("the mock's core is unreachable for {flow_id} v{version}"),
            }),
        }
    }

    fn classify(&self, kind: &CallKind) -> CallClass {
        let names = op_names(kind);
        if let Some(class) = lock(&self.shared.controls).classes.get(&names) {
            return *class;
        }
        Self::default_class(kind)
    }

    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        let envelope: Value = serde_json::from_str(request.args.as_str()).unwrap_or(Value::Null);
        let broca = matches!(
            &request.kind,
            CallKind::Primitive(Primitive::Llm | Primitive::Classify)
        );
        // A Broca call's own options sit inside the envelope basal built.
        let args = if broca {
            envelope.get("request").cloned().unwrap_or(Value::Null)
        } else {
            envelope.clone()
        };
        if let Some(ms) = args.get("delay_ms").and_then(Value::as_u64) {
            thread::sleep(Duration::from_millis(ms.min(60_000)));
        }
        if let Some(gate) = args.get("gate").and_then(Value::as_str) {
            self.wait_gate(gate);
        }
        if let Some(r) = args.get("rendezvous")
            && let (Some(name), Some(count)) = (
                r.get("name").and_then(Value::as_str),
                r.get("count").and_then(Value::as_u64),
            )
        {
            self.rendezvous(name, usize::try_from(count).unwrap_or(usize::MAX));
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
        if broca {
            let send_id = envelope
                .get("send_id")
                .and_then(Value::as_str)
                .unwrap_or(&request.idempotency_key)
                .to_owned();
            let reused = state
                .broca_sends
                .iter()
                .any(|(id, bytes)| *id == send_id && bytes != request.args.as_str());
            state
                .broca_sends
                .push((send_id, request.args.as_str().to_owned()));
            if reused {
                self.save(&state);
                return Ok(Dispatched::Completed(HostOutcome::rejected(text(
                    &json!({"message": "send_id reused with a different request", "code": "send_id_reuse"}),
                ))));
            }
        }
        let usage = broca.then(|| Self::usage_for(&envelope, request.args.len()));
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
                usage: usage.clone(),
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
                    let mut outcome = HostOutcome::fulfilled(text(&label));
                    outcome.usage = usage.clone().and_then(|u| serde_json::from_value(u).ok());
                    Dispatched::Completed(outcome)
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
            "usage": c.usage,
        })).collect::<Vec<_>>(),
        "broca_sends": state.broca_sends.iter().map(|(id, b)| json!([id, b])).collect::<Vec<_>>(),
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
                usage: c.get("usage").filter(|u| !u.is_null()).cloned(),
            },
        );
    }
    for s in v.get("broca_sends")?.as_array()? {
        let s = strings(s, 2)?;
        state.broca_sends.push((s[0].clone(), s[1].clone()));
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
    fn calls_meeting_at_a_rendezvous_are_counted_together() {
        let mock = MockHost::new();
        std::thread::scope(|s| {
            for i in 0..3 {
                let mock = mock.clone();
                s.spawn(move || {
                    let mut r = request("echo", &format!("k{i}"));
                    r.args = JsonText::new("{\"rendezvous\":{\"name\":\"r\",\"count\":3}}")
                        .unwrap_or_else(|_| JsonText::null());
                    assert!(mock.dispatch(&r).is_ok());
                });
            }
        });
        assert_eq!(mock.peak_concurrency("r"), 3);
        assert_eq!(mock.peak_concurrency("other"), 0);
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
