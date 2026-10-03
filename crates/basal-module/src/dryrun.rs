//! Dry runs: run a flow against the fires its schedule
//! would have produced over a window, or against a synthetic trigger, and
//! report every call and sink write it made.
//!
//! A dry run never touches the flow's real state. It installs the version
//! into a scratch store of its own, created for the dry run and deleted
//! after it, so the runs, the journal and `kv` it writes are the scratch
//! store's. Its host is a capture host:
//! - in capture mode (the default, and the only mode before approval) no
//!   host call is executed. Each one is answered with a journaled rejection
//!   with code `captured`, so the script sees an error rather than an
//!   invented result. Running a flow's reads with basal's authority before
//!   anyone approved them, and handing the results to its author, would make
//!   basal a confused deputy.
//! - in live mode (the operator only) ops the catalog marks `query` are sent
//!   to the real host; everything else is still captured. The markers are
//!   each module's own claim, so this is a live query simulation, not a
//!   guarantee of no effects.
//!
//! `kv` is served by the scratch store like in a real run, so a flow that
//! reads back what it wrote behaves as it would. Clock reads return the
//! fire's due time.
//!
//! A captured call's rejection is a fiction the real run would not see, so
//! everything the script did after it saw one is marked partial: each call
//! issued after a captured rejection was delivered to the script, and the
//! run's own ending if it did not complete. A script that merely ignores a
//! captured call (never waits for it, or catches it and issues nothing
//! more) leaves the rest of its trace whole.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use basal_core::channel::{ChannelError, WorkerChannel};
use basal_core::manifest::{DigestAction, Manifest};
use basal_core::schedule::{self, fire_trigger_id};
use basal_core::{
    ActivationEnd, Admission, CallRow, Config, Durability, InstallRequest, NoHooks, RunState,
    Runtime, Store,
};
use basal_host::{
    CallClass, CallRequest, Catalog, CompletionSink, Dispatched, Host, HostOutcome, OpKind,
    TransportError,
};
use basal_proto::{
    CallKind, JsonText, ParentMessage, Primitive, Settlement, Welcome, WorkerMessage,
};
use jiff::Timestamp;
use serde_json::{Value, json};

use crate::metrics::Metrics;
use crate::pool::{Binding, Lease, Pool};

/// The rejection code of a captured call.
pub const CAPTURED: &str = "captured";

#[derive(Debug, Clone)]
pub struct DryRunConfig {
    /// Where scratch stores are created (and removed).
    pub scratch_root: PathBuf,
    /// The most fires one schedule dry run replays: the newest ones in the
    /// window.
    pub max_fires: usize,
    /// The window replayed when the request names none.
    pub default_window: Duration,
    /// The longest window a request may name.
    pub max_window: Duration,
    /// Activations one fire may take before the dry run gives up on it (a
    /// worker can break; the run is replayed on another).
    pub attempts_per_fire: u32,
    /// Calls listed per run in the summary; the rest are counted.
    pub max_calls_listed: usize,
    /// Bytes of a call's arguments shown in the summary.
    pub max_arg_bytes_shown: usize,
}

impl DryRunConfig {
    pub fn new(scratch_root: impl Into<PathBuf>) -> Self {
        Self {
            scratch_root: scratch_root.into(),
            max_fires: 10,
            default_window: Duration::from_secs(24 * 3600),
            max_window: Duration::from_secs(7 * 24 * 3600),
            attempts_per_fire: 3,
            max_calls_listed: 200,
            max_arg_bytes_shown: 2048,
        }
    }
}

/// Which mode a dry run runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Every host call captured.
    Capture,
    /// `query` ops live, everything else captured.
    Live,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Live => "live",
        }
    }
}

/// What a dry run runs the flow against.
#[derive(Debug, Clone, PartialEq)]
pub enum DryTrigger {
    /// The fires the flow's schedule would have produced over the window
    /// ending now. A flow with an events trigger has none: event replay
    /// needs the event plane.
    Schedule { window: Option<Duration> },
    /// One trigger with this payload.
    Synthetic(Value),
}

#[derive(Debug, Clone)]
pub struct DryRunRequest {
    pub script: String,
    pub manifest: String,
    pub author: String,
    pub loop_override: bool,
    pub mode: Mode,
    pub trigger: DryTrigger,
    /// The end of the window, and the time the fires are measured from.
    pub now_ms: i64,
}

#[derive(Debug)]
pub enum DryRunError {
    /// The request itself is wrong (a window too long, a manifest that does
    /// not parse).
    Invalid(String),
    /// The scratch store or a worker failed. The flow's real state is not
    /// involved either way.
    Failed(String),
}

impl std::fmt::Display for DryRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(e) => write!(f, "invalid dry run: {e}"),
            Self::Failed(e) => write!(f, "dry run failed: {e}"),
        }
    }
}

impl std::error::Error for DryRunError {}

fn failed(e: impl std::fmt::Display) -> DryRunError {
    DryRunError::Failed(e.to_string())
}

/// Runs dry runs.
pub struct DryRunner {
    catalog: Arc<dyn Catalog>,
    live_host: Arc<dyn Host>,
    pool: Pool,
    config: DryRunConfig,
    base: Config,
    metrics: Arc<Metrics>,
    counter: AtomicU64,
}

impl DryRunner {
    pub fn new(
        catalog: Arc<dyn Catalog>,
        live_host: Arc<dyn Host>,
        pool: Pool,
        config: DryRunConfig,
        base: Config,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            catalog,
            live_host,
            pool,
            config,
            base,
            metrics,
            counter: AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> &DryRunConfig {
        &self.config
    }

    /// Runs one dry run and returns its summary.
    pub fn run(&self, request: &DryRunRequest) -> Result<Value, DryRunError> {
        let manifest = Manifest::parse(&request.manifest)
            .map_err(|e| DryRunError::Invalid(format!("manifest: {e}")))?;
        let (fires, window) = self.fires(&manifest, request)?;
        Metrics::bump(&self.metrics.dry_runs);

        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let dir = self.config.scratch_root.join(format!(
            "dry-{}-{}-{n}",
            std::process::id(),
            request.now_ms
        ));
        std::fs::create_dir_all(&dir).map_err(failed)?;
        let outcome = self.run_in(&dir, &manifest, request, &fires);
        // The scratch store is the dry run's alone; nothing outlives it.
        let _ = std::fs::remove_dir_all(&dir);
        let runs = outcome?;
        let partial = runs
            .iter()
            .any(|r| r.get("partial").and_then(Value::as_bool) == Some(true));
        Ok(json!({
            "mode": request.mode.as_str(),
            "window": window,
            "partial": partial,
            "runs": runs,
        }))
    }

    /// The fires to replay, oldest first, and the summary's description of
    /// the window they cover.
    fn fires(
        &self,
        manifest: &Manifest,
        request: &DryRunRequest,
    ) -> Result<(Vec<(String, Value)>, Value), DryRunError> {
        match &request.trigger {
            DryTrigger::Synthetic(payload) => Ok((
                vec![("dry-run:synthetic".to_owned(), payload.clone())],
                json!({ "kind": "synthetic" }),
            )),
            DryTrigger::Schedule { window } => {
                let Some(spec) = &manifest.trigger.schedule else {
                    return Ok((
                        Vec::new(),
                        json!({
                            "kind": "events",
                            "covered": null,
                            "note": "event replay needs the event plane; no events were replayed",
                        }),
                    ));
                };
                let window = window.unwrap_or(self.config.default_window);
                if window > self.config.max_window {
                    return Err(DryRunError::Invalid(format!(
                        "the window may be at most {} s",
                        self.config.max_window.as_secs()
                    )));
                }
                let compiled = schedule::spec::validate(spec)
                    .map_err(|e| DryRunError::Invalid(e.to_string()))?;
                let window_ms = i64::try_from(window.as_millis())
                    .map_err(|_| DryRunError::Invalid("window too long".into()))?;
                let to = timestamp(request.now_ms)?;
                let from = timestamp(request.now_ms.saturating_sub(window_ms))?;
                // Walk the due times in (from, to], keeping the newest
                // `max_fires`. The walk is bounded so a minute-level pattern
                // over a long window cannot hold the op for long.
                const SCAN_LIMIT: u64 = 20_000;
                let mut kept: VecDeque<Timestamp> = VecDeque::new();
                let mut count: u64 = 0;
                let mut cursor = from;
                let mut lower_bound = false;
                loop {
                    let next = compiled
                        .next_due_after(from, cursor)
                        .map_err(|e| DryRunError::Invalid(e.to_string()))?;
                    let Some(due) = next.filter(|d| *d <= to) else {
                        break;
                    };
                    count += 1;
                    kept.push_back(due);
                    if kept.len() > self.config.max_fires {
                        kept.pop_front();
                    }
                    if count >= SCAN_LIMIT {
                        lower_bound = true;
                        break;
                    }
                    cursor = due;
                }
                let first_replayed = kept.front().map(|t| t.to_string());
                let fires = kept
                    .iter()
                    .map(|due| {
                        (
                            fire_trigger_id(*due),
                            json!({ "kind": "schedule", "due": due.to_string() }),
                        )
                    })
                    .collect();
                Ok((
                    fires,
                    json!({
                        "kind": "schedule",
                        "requested": { "from": from.to_string(), "to": to.to_string() },
                        // What the replayed fires actually cover: from the
                        // first replayed due time to the window's end.
                        "covered": first_replayed.map(|f| json!({ "from": f, "to": to.to_string() })),
                        "due_times_in_window": count,
                        "due_times_is_lower_bound": lower_bound,
                        "replayed": kept.len(),
                        "capped": count > kept.len() as u64,
                    }),
                ))
            }
        }
    }

    fn run_in(
        &self,
        dir: &std::path::Path,
        manifest: &Manifest,
        request: &DryRunRequest,
        fires: &[(String, Value)],
    ) -> Result<Vec<Value>, DryRunError> {
        let store =
            Store::open(dir.join("scratch.db"), Durability { fullfsync: false }).map_err(failed)?;
        let clock = basal_core::Clock::manual(request.now_ms);
        let host = Arc::new(CaptureHost {
            mode: request.mode,
            live: self.live_host.clone(),
            catalog: self.catalog.clone(),
            clock: clock.clone(),
            random_state: AtomicU64::new(0x9E37_79B9_7F4A_7C15),
            live_positions: Mutex::new(BTreeSet::new()),
        });
        let config = Config {
            owner: format!("basal-dry-run-{}", std::process::id()),
            clock: clock.clone(),
            auto_resume: false,
            selector: Arc::new(CapturedSelector),
            // No install gate: core has approved nothing for a dry run, and
            // its capture host never reaches core. Only module ops the
            // catalog marks as queries go out, in live mode; every sink
            // write, facts read and model call is captured (`runs_live`).
            install_gate: basal_core::InstallGate::Off,
            ..self.base.clone()
        };
        let rt = Runtime::new(
            Arc::new(store),
            host.clone(),
            self.catalog.clone(),
            Arc::new(NoHooks),
            None,
            config,
        );
        let installed = rt
            .install(&InstallRequest {
                script: request.script.clone(),
                manifest: request.manifest.clone(),
                author: request.author.clone(),
                loop_override: request.loop_override,
            })
            .map_err(|e| DryRunError::Invalid(format!("install into the scratch store: {e}")))?;
        rt.approve(
            &installed.flow_id,
            installed.version,
            &installed.code_hash,
            "dry-run",
        )
        .map_err(|e| failed(format!("approve in the scratch store: {e}")))?;

        let mut runs = Vec::new();
        for (trigger_id, payload) in fires {
            // Clock reads, token windows and deadlines all read the fire's
            // due time when it has one, else the request's time.
            let at = payload
                .get("due")
                .and_then(Value::as_str)
                .and_then(|d| d.parse::<Timestamp>().ok())
                .map_or(request.now_ms, |t| t.as_millisecond());
            clock.set(at);
            let text = JsonText::new(payload.to_string()).map_err(failed)?;
            let run_id = match rt
                .admit_trigger(&installed.flow_id, trigger_id, text)
                .map_err(failed)?
            {
                Admission::Admitted { run_id } => run_id,
                other => {
                    runs.push(json!({
                        "trigger_id": trigger_id,
                        "trigger": payload,
                        "admission": format!("{other:?}"),
                    }));
                    continue;
                }
            };
            let mut events = Vec::new();
            let mut ending = None;
            for _ in 0..self.config.attempts_per_fire {
                let mut lease = self
                    .pool
                    .acquire(Binding::DryRun(format!(
                        "{}:{trigger_id}",
                        installed.flow_id
                    )))
                    .map_err(failed)?;
                let end = {
                    let mut recording = Recording {
                        inner: &mut lease,
                        events: &mut events,
                    };
                    rt.activate(&run_id, &mut recording).map_err(failed)?
                };
                drop(lease);
                let again = matches!(end, ActivationEnd::Requeued);
                ending = Some(end);
                if !again {
                    break;
                }
            }
            rt.quiesce();
            let run = rt.run(&run_id).map_err(failed)?;
            let calls = rt.calls(&run_id).map_err(failed)?;
            runs.push(self.describe_run(manifest, &run, &calls, &events, ending.as_ref(), &host));
        }
        Ok(runs)
    }

    fn describe_run(
        &self,
        manifest: &Manifest,
        run: &basal_core::Run,
        calls: &[CallRow],
        events: &[Event],
        ending: Option<&ActivationEnd>,
        host: &CaptureHost,
    ) -> Value {
        let captured: BTreeSet<u64> = calls
            .iter()
            .filter(|c| outcome_code(c).as_deref() == Some(CAPTURED))
            .map(|c| c.position)
            .collect();
        // Which calls were issued after the script had seen a captured
        // rejection.
        let mut tainted = false;
        let mut partial_calls = BTreeSet::new();
        for event in events {
            match event {
                Event::Delivered(p) if captured.contains(p) => tainted = true,
                Event::Prefix(ps) if ps.iter().any(|p| captured.contains(p)) => tainted = true,
                Event::Issued(p) if tainted => {
                    partial_calls.insert(*p);
                }
                _ => {}
            }
        }
        let ended_well = run.state == RunState::Succeeded;
        let run_partial = !partial_calls.is_empty() || (tainted && !ended_well);
        let live = host.live_positions(&run.run_id);
        let listed: Vec<Value> = calls
            .iter()
            .take(self.config.max_calls_listed)
            .map(|c| {
                let mut entry = describe_call(c, &self.config);
                entry["action"] = json!(if live.contains(&c.position) {
                    "live"
                } else if captured.contains(&c.position) {
                    "captured"
                } else if c.kind.is_synchronous() || basal_core::kv::is_kv(&c.kind) {
                    "local"
                } else {
                    "refused"
                });
                entry["partial"] = json!(partial_calls.contains(&c.position));
                if let CallKind::Primitive(Primitive::SinkDigest) = c.kind {
                    entry["sink"] = effective_sink_action(manifest, &c.args);
                }
                entry
            })
            .collect();
        json!({
            "run_id": run.run_id,
            "trigger_id": run.trigger_id,
            "trigger": serde_json::from_str::<Value>(run.trigger.as_str()).unwrap_or(Value::Null),
            "state": run.state.as_str(),
            "result": run.result.as_deref().map(|r| serde_json::from_str::<Value>(r).unwrap_or(Value::Null)),
            "error": run.error_kind.as_ref().map(|k| json!({"kind": k, "detail": run.error_detail})),
            "ending": ending.map(|e| format!("{e:?}")),
            "partial": run_partial,
            "calls": listed,
            "calls_total": calls.len(),
        })
    }
}

fn timestamp(ms: i64) -> Result<Timestamp, DryRunError> {
    Timestamp::from_millisecond(ms).map_err(|e| DryRunError::Invalid(format!("time {ms}: {e}")))
}

/// The rejection code of a call's recorded outcome, if it was rejected.
fn outcome_code(call: &CallRow) -> Option<String> {
    let outcome = call.outcome.as_ref()?;
    if outcome.settlement != Settlement::Rejected {
        return None;
    }
    serde_json::from_str::<Value>(outcome.value.as_str())
        .ok()?
        .get("code")?
        .as_str()
        .map(str::to_owned)
}

fn describe_call(call: &CallRow, config: &DryRunConfig) -> Value {
    let (kind, module, op) = match &call.kind {
        CallKind::Op { module, op } => ("op".to_owned(), Some(module.clone()), Some(op.clone())),
        CallKind::Primitive(p) => (p.name().to_owned(), None, None),
    };
    let args = call.args.as_str();
    let shown = if args.len() <= config.max_arg_bytes_shown {
        serde_json::from_str::<Value>(args).unwrap_or(Value::Null)
    } else {
        let mut end = config.max_arg_bytes_shown;
        while !args.is_char_boundary(end) {
            end -= 1;
        }
        json!({ "truncated": &args[..end], "bytes": args.len() })
    };
    let outcome = call.outcome.as_ref().map(|o| {
        let value = serde_json::from_str::<Value>(o.value.as_str()).unwrap_or(Value::Null);
        match o.settlement {
            Settlement::Fulfilled => json!({ "fulfilled": true }),
            Settlement::Rejected => {
                json!({ "rejected": value.get("code").cloned().unwrap_or(Value::Null) })
            }
        }
    });
    json!({
        "position": call.position,
        "kind": kind,
        "module": module,
        "op": op,
        "args": shown,
        "outcome": outcome,
    })
}

/// The action a digest write would have taken: the less intrusive of what
/// the flow asked for and the manifest's cap for the recipient. An omitted
/// action means the approved digest_max, just as in a live journaled request. The
/// recipient's own delivery policy lives in core, which a dry run does not
/// reach, so it is not applied.
fn effective_sink_action(manifest: &Manifest, args: &JsonText) -> Value {
    let args: Value = serde_json::from_str(args.as_str()).unwrap_or(Value::Null);
    let agent = args.get("agent").and_then(Value::as_str).unwrap_or("");
    let requested = match args.get("action").and_then(Value::as_str) {
        Some(a) => DigestAction::parse(a),
        None => manifest.digest_cap(agent),
    };
    let cap = manifest.digest_cap(agent);
    let effective = match (requested, cap) {
        (Some(r), Some(c)) => Some(r.min(c)),
        _ => None,
    };
    json!({
        "agent": agent,
        "requested": requested,
        "cap": cap,
        "effective": effective,
        "policy": "not consulted: the recipient's policy is core's",
    })
}

/// What crossed a dry-run activation's channel, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    /// The recorded outcomes an activation's prefix carried (a replay after
    /// a broken worker): the script sees them before anything new.
    Prefix(Vec<u64>),
    /// An outcome delivered to the script.
    Delivered(u64),
    /// A call the script issued.
    Issued(u64),
}

/// A worker channel that notes the order of deliveries and calls.
struct Recording<'a> {
    inner: &'a mut Lease,
    events: &'a mut Vec<Event>,
}

impl WorkerChannel for Recording<'_> {
    fn welcome(&self) -> &Welcome {
        self.inner.welcome()
    }

    fn send(&mut self, message: &ParentMessage) -> Result<(), ChannelError> {
        match message {
            ParentMessage::Activate(request) => self.events.push(Event::Prefix(
                request
                    .prefix
                    .iter()
                    .filter(|c| c.outcome.is_some())
                    .map(|c| c.position)
                    .collect(),
            )),
            ParentMessage::Deliver(outcome) => self.events.push(Event::Delivered(outcome.position)),
            _ => {}
        }
        self.inner.send(message)
    }

    fn recv(&mut self, timeout: Duration) -> Result<WorkerMessage, ChannelError> {
        let message = self.inner.recv(timeout)?;
        if let WorkerMessage::HostCall(call) = &message {
            self.events.push(Event::Issued(call.position));
        }
        Ok(message)
    }

    fn kill(&mut self) {
        self.inner.kill();
    }
}

/// The dry run's host: captures every call, or in live mode sends `query`
/// ops to the real host.
struct CaptureHost {
    mode: Mode,
    live: Arc<dyn Host>,
    catalog: Arc<dyn Catalog>,
    clock: basal_core::Clock,
    random_state: AtomicU64,
    /// (run, position) of calls sent live.
    live_positions: Mutex<BTreeSet<(String, u64)>>,
}

impl CaptureHost {
    /// Whether a call goes to the real host: in live mode only, and only a
    /// catalogued query op or a built-in that only reads (the file and git
    /// reads, and `net.fetch` with `GET` or `HEAD`). A built-in's request
    /// carries the scope the manifest granted it, which the real host
    /// checks again before it acts, so a live read stays within the scope.
    fn runs_live(&self, request: &CallRequest) -> bool {
        if self.mode != Mode::Live {
            return false;
        }
        match &request.kind {
            CallKind::Op { module, op } => self
                .catalog
                .op(module, op)
                .is_some_and(|d| d.kind == Some(OpKind::Query) && !d.shell_capable),
            kind if basal_host::builtins::is_builtin(kind) => {
                serde_json::from_str::<Value>(request.args.as_str())
                    .ok()
                    .and_then(|envelope| basal_host::builtins::request_class(kind, &envelope))
                    == Some(CallClass::Query)
            }
            _ => false,
        }
    }

    fn live_positions(&self, run_id: &str) -> BTreeSet<u64> {
        self.live_positions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(r, _)| r == run_id)
            .map(|(_, p)| *p)
            .collect()
    }
}

fn captured(detail: &str) -> HostOutcome {
    HostOutcome::rejected(
        JsonText::new(
            json!({
                "message": "captured by a dry run: not executed",
                "code": CAPTURED,
                "detail": detail,
            })
            .to_string(),
        )
        .unwrap_or_else(|_| JsonText::null()),
    )
}

impl Host for CaptureHost {
    fn classify(&self, kind: &CallKind) -> CallClass {
        // A built-in is classed by the runtime from its arguments and never
        // asks this; a module op's class is the catalog's.
        let live_op = self.mode == Mode::Live
            && matches!(kind, CallKind::Op { module, op } if self
                .catalog
                .op(module, op)
                .is_some_and(|d| d.kind == Some(OpKind::Query) && !d.shell_capable));
        if live_op {
            CallClass::Query
        } else {
            // Captured calls are answered at once by this host, so the class
            // never decides a resend. A mutation that ignores idempotency
            // keys is the class the runtime never resends, so it is the safe
            // answer if that ever changes.
            CallClass::Mutation {
                honours_idempotency_keys: false,
            }
        }
    }

    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        if !self.runs_live(request) {
            return Ok(Dispatched::Completed(captured("capture mode")));
        }
        match self.live.dispatch(request)? {
            Dispatched::Completed(outcome) => {
                self.live_positions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert((request.run_id.clone(), request.position));
                Ok(Dispatched::Completed(outcome))
            }
            // A long-running query would complete through the real host's
            // completion sink, which belongs to the real runtime. The dry
            // run does not wait for it.
            Dispatched::Accepted { .. } => Ok(Dispatched::Completed(captured(
                "the op answered as long-running; a dry run does not wait",
            ))),
        }
    }

    fn now_ms(&self) -> f64 {
        self.clock.now_ms() as f64
    }

    fn random(&self) -> f64 {
        // xorshift64*: deterministic, so a dry run is repeatable.
        let mut x = self.random_state.load(Ordering::Relaxed);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.random_state.store(x, Ordering::Relaxed);
        let bits = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        bits as f64 / (1u64 << 53) as f64
    }

    fn attach(&self, _sink: Arc<dyn CompletionSink>) {
        // The scratch runtime's completion sink is not passed on to the
        // real host: the real host has one sink, the real runtime's, and
        // attaching another would take completions away from it.
    }
}

#[derive(Debug)]
struct CapturedSelector;
impl basal_host::selector::ModelSelector for CapturedSelector {
    fn select(
        &self,
        _: &basal_host::selector::SelectionRequest,
    ) -> Result<basal_host::selector::ModelSelection, basal_host::selector::SelectionError> {
        // Previewing a model write must not allocate a real routing decision.
        Err(basal_host::selector::SelectionError::Refused {
            code: CAPTURED.into(),
            detail: "model call captured without selecting or sending model work".into(),
        })
    }
    fn report_outcome(
        &self,
        _: &str,
        _: basal_host::selector::ModelOutcome,
    ) -> Result<(), basal_host::selector::SelectionError> {
        Ok(())
    }
}
