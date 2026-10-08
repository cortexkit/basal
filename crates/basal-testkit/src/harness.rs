//! The journal cut harness and its helpers.
//!
//! A scenario runs a script through basal-core's runtime with a real worker
//! and the mock host. The harness first runs it uncut, recording every
//! [`Boundary`] it passes (with its occurrence count, since some boundaries
//! repeat). Then, for each recorded boundary, it runs the scenario again in a
//! fresh store and crashes at that boundary: the store is cut, the worker is
//! killed, and every dispatch thread fails its next write. A new runtime
//! reopens the store, recovers, and drives the run to its end. The final
//! state, result and effect counts must equal the uncut run's.
//!
//! The mock host is the outside world: it survives the crash, keeps every
//! effect it applied, and redelivers completions the crashed runtime never
//! acknowledged.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use basal_core::{
    Boundary, Config, CoreError, Durability, Hooks, InstallRequest, Run, RunState, Runtime, Step,
    Store, TriggerSpec,
};
use basal_host::MockCatalog;
use basal_host::mock::MockHost;
use basal_proto::JsonText;
use serde_json::{Value, json};

use crate::channel::{ProcessSource, worker_binary};

/// Bound parallel real-process crash scenarios on a shared test machine.
pub const CUT_PARALLEL: usize = 6;

/// The representative run: a race whose loser is long-running and still
/// outstanding long after the winner was released, a synchronous clock read,
/// a caught rejection, a call the manifest does not allow (refused in the
/// parent and caught), a local eager effect, two calls completing in
/// parallel, a random sample, an `llm` call that reserves tokens and
/// suspends the run, and a resume that reads the local effect back.
pub const REPRESENTATIVE: &str = r#"
const winner = await Promise.race([
    ops.call('mock', 'long', { tag: 'slow' }),
    ops.call('mock', 'echo', { tag: 'fast' }),
]);
const t1 = Date.now();
let caught = null;
try {
    await ops.call('mock', 'fail', { message: 'nope' });
} catch (e) {
    caught = e.data.code;
}
let denied = null;
try {
    await ops.call('mock', 'unlisted', {});
} catch (e) {
    denied = e.data.code;
}
await kv.set('seen', winner.tag);
const [a, b] = await Promise.all([
    ops.call('mock', 'send', { n: 1, delay_ms: 15 }),
    ops.call('mock', 'echo', { n: 2 }),
]);
const r = Math.random();
const answer = await llm({ prompt: 'summarise' });
const seen = await kv.get('seen');
const t2 = Date.now();
return {
    winner: winner.tag,
    caught,
    denied,
    a: a.applied.n,
    b: b.n,
    r: r >= 0 && r < 1,
    answer: answer.done,
    seen,
    monotonic: t2 >= t1,
};
"#;

/// The flow id every harness run belongs to.
pub const TEST_FLOW: &str = "flow-test";

/// The manifest harness flows run under unless a test says otherwise: the
/// mock host's ops except `unlisted`, one digest sink, a status target,
/// facts and a model grant large enough never to refuse.
pub fn test_manifest() -> Value {
    json!({
        "id": TEST_FLOW,
        "version": 1,
        "purpose": "Exercise the runtime against the mock host.",
        "trigger": { "events": [ { "module": "plexus", "name": "pull_request_review", "version": 1 } ] },
        "sinks": [ { "agent": "ALF", "digest_max": "wake" } ],
        "status": [ "ALF" ],
        "ops": [
            { "module": "mock", "op": "echo" },
            { "module": "mock", "op": "fail" },
            { "module": "mock", "op": "send" },
            { "module": "mock", "op": "long" },
            { "module": "mock", "op": "post" }
        ],
        "facts": { "targets": [ "ALF" ], "text": false },
        "llm": { "iq": 0, "token_cap": { "tokens": 1_000_000, "window": "1d" }, "max_output": 2000 }
    })
}

/// Installs and approves `script` under `manifest` (its `id` must be the
/// flow's; its `version` is chosen here) and returns a spec that admits a
/// trigger under it. The same script and manifest as the approved version
/// reuse it; anything else becomes the next version, so a restarted test
/// parent finds the version its run was admitted under.
pub fn approve_spec(
    rt: &Runtime,
    script: &str,
    manifest: &Value,
    trigger_id: &str,
    trigger: JsonText,
) -> Result<TriggerSpec, String> {
    let flow_id = manifest
        .get("id")
        .and_then(Value::as_str)
        .ok_or("the manifest has no id")?
        .to_owned();
    let with_version = |v: u32| -> Result<String, String> {
        let mut m = manifest.clone();
        m["version"] = json!(v);
        serde_json::to_string(&m).map_err(|e| e.to_string())
    };
    let approved = rt
        .store()
        .read(|c| basal_core::install::approved(c, &flow_id))
        .map_err(|e| e.to_string())?;
    let spec = |manifest: String| TriggerSpec {
        flow_id: flow_id.clone(),
        trigger_id: trigger_id.to_owned(),
        trigger: trigger.clone(),
        script: script.to_owned(),
        manifest,
    };
    if let Some(a) = &approved {
        let same = with_version(a.version)?;
        if a.script == script && a.manifest == same {
            return Ok(spec(same));
        }
    }
    let version = approved.map_or(1, |a| a.version + 1);
    let text = with_version(version)?;
    let installed = rt
        .install(&InstallRequest {
            script: script.to_owned(),
            manifest: text.clone(),
            author: "ALF".into(),
            loop_override: false,
        })
        .map_err(|e| format!("install: {e}"))?;
    rt.approve(&flow_id, version, &installed.code_hash, "test-approval")
        .map_err(|e| format!("approve: {e}"))?;
    Ok(spec(text))
}

/// Waits for a fixture condition until its deadline. Callers must release any
/// blocked host calls and stop their workers before reporting a timeout.
pub fn wait_until(
    deadline: Instant,
    description: &str,
    mut ready: impl FnMut() -> bool,
) -> Result<(), String> {
    loop {
        if ready() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {description}"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A unique scratch directory.
pub fn scratch(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "basal-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}

/// A boundary as the harness names it: its debug form and which
/// occurrence of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Point {
    pub boundary: String,
    pub occurrence: usize,
}

impl Point {
    pub fn parse(text: &str) -> Option<Self> {
        let (boundary, n) = text.rsplit_once('#')?;
        Some(Self {
            boundary: boundary.to_owned(),
            occurrence: n.parse().ok()?,
        })
    }

    pub fn render(&self) -> String {
        format!("{}#{}", self.boundary, self.occurrence)
    }
}

/// Something a test does at a boundary instead of crashing.
pub type Action = Box<dyn Fn(&str, &Boundary) + Send + Sync>;

/// A boundary's name for cutting. The delivery order is left out of an
/// order commit's name: which of two calls completing in parallel gets the
/// lower order depends on timing, and the cut should still land at the
/// same call's release.
pub fn point_name(boundary: &Boundary) -> String {
    match boundary {
        Boundary::OrderCommitted { position, .. } => {
            format!("OrderCommitted {{ position: {position} }}")
        }
        other => format!("{other:?}"),
    }
}

/// Boundaries that can occur a different number of times in two runs of the
/// same script: how often the worker reports itself blocked, and in how
/// many replies the parent reports long-running calls, depend on when
/// outcomes arrive. A cut named by one of these may not be reached in
/// another run; every other boundary of a run is always reached again.
pub fn is_repeat_prone(point: &Point) -> bool {
    point.boundary == "BlockedReceived" || point.boundary == "LongRunningSent"
}

/// Records every boundary passed, and crashes (or runs an action) at one.
#[derive(Default)]
pub struct Probe {
    seen: Mutex<HashMap<String, usize>>,
    pub log: Mutex<Vec<Point>>,
    pub target: Option<Point>,
    pub fired: AtomicBool,
    /// Instead of crashing, run this at the target and continue.
    pub action: Option<Action>,
}

impl Probe {
    pub fn recording() -> Self {
        Self::default()
    }

    pub fn crash_at(target: Point) -> Self {
        Self {
            target: Some(target),
            ..Self::default()
        }
    }

    pub fn act_at(target: Point, action: impl Fn(&str, &Boundary) + Send + Sync + 'static) -> Self {
        Self {
            target: Some(target),
            action: Some(Box::new(action)),
            ..Self::default()
        }
    }

    pub fn points(&self) -> Vec<Point> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

impl Hooks for Probe {
    fn at(&self, run_id: &str, boundary: &Boundary) -> Step {
        let name = point_name(boundary);
        let point = {
            let Ok(mut seen) = self.seen.lock() else {
                return Step::Continue;
            };
            let n = seen.entry(name.clone()).or_insert(0);
            *n += 1;
            Point {
                boundary: name,
                occurrence: *n,
            }
        };
        if let Ok(mut log) = self.log.lock() {
            log.push(point.clone());
        }
        if self.target.as_ref() == Some(&point) && !self.fired.swap(true, Ordering::SeqCst) {
            if let Some(action) = &self.action {
                action(run_id, boundary);
                return Step::Continue;
            }
            return Step::Crash;
        }
        Step::Continue
    }
}

/// Hooks from a closure, for tests that act at a specific boundary.
pub struct FnHooks<F>(pub F);

impl<F: Fn(&str, &Boundary) -> Step + Send + Sync> Hooks for FnHooks<F> {
    fn at(&self, run_id: &str, boundary: &Boundary) -> Step {
        (self.0)(run_id, boundary)
    }
}

/// What a run ended as, in terms that do not depend on the store's random
/// identity (run ids and idempotency keys differ between stores).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub state: String,
    pub result: Option<Value>,
    pub error_kind: Option<String>,
    /// Remote effects by (module, op, arguments).
    pub effects: BTreeMap<String, usize>,
    /// Remote effects per idempotency key, as (position, count).
    pub effects_per_call: BTreeMap<u64, usize>,
    /// The call audit: each position's recorded outcome. Exactly one row
    /// per journaled call when [`Summary::audit_complete`] holds.
    pub audit: BTreeMap<u64, String>,
    /// Audit rows counted, which must equal `calls`.
    pub audit_rows: usize,
    /// The flow's `kv`, as key to `value@revision`: a write applied twice
    /// shows as a higher revision.
    pub kv: BTreeMap<String, String>,
    /// The flow's token ledger: reservations still open and settled
    /// usage summed by field.
    pub tokens: BTreeMap<String, u64>,
    /// Broca sends whose bytes differ from an earlier send under the same
    /// `send_id`.
    pub broca_reuse: usize,
    pub calls: usize,
    pub open_obligations: usize,
}

impl Summary {
    /// Every journaled call has exactly one audit row, and no audit row
    /// names a call that is not journaled.
    pub fn audit_complete(&self) -> bool {
        self.audit_rows == self.calls && self.audit.keys().copied().eq(0..self.calls as u64)
    }
}

impl Summary {
    /// The summary as JSON, for comparing runs across processes.
    pub fn to_json(&self) -> Value {
        let positions = |m: &BTreeMap<u64, usize>| -> BTreeMap<String, usize> {
            m.iter().map(|(k, v)| (k.to_string(), *v)).collect()
        };
        json!({
            "state": self.state,
            "result": self.result,
            "error_kind": self.error_kind,
            "effects": self.effects,
            "effects_per_call": positions(&self.effects_per_call),
            "audit": self.audit.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<BTreeMap<_, _>>(),
            "audit_rows": self.audit_rows,
            "kv": self.kv,
            "tokens": self.tokens,
            "broca_reuse": self.broca_reuse,
            "calls": self.calls,
            "open_obligations": self.open_obligations,
        })
    }
}

pub fn summarize(rt: &Runtime, mock: &MockHost, run_id: &str) -> Result<Summary, CoreError> {
    let run = rt.run(run_id)?;
    let calls = rt.calls(run_id)?;
    let positions: HashMap<String, u64> = calls
        .iter()
        .map(|c| (c.idempotency_key.clone(), c.position))
        .collect();
    let mut effects = BTreeMap::new();
    let mut per_call = BTreeMap::new();
    for e in mock.effects() {
        // A model call's envelope names its send id and run, which differ
        // between stores; the script's own request inside it does not.
        let args = match (e.module.as_str(), e.op.as_str()) {
            ("primitive", "llm" | "classify") => serde_json::from_str::<Value>(&e.args)
                .ok()
                .and_then(|v| v.get("request").map(Value::to_string))
                .unwrap_or_else(|| e.args.clone()),
            _ => e.args.clone(),
        };
        *effects
            .entry(format!("{}.{} {}", e.module, e.op, args))
            .or_insert(0) += 1;
        if let Some(p) = positions.get(&e.key) {
            *per_call.entry(*p).or_insert(0) += 1;
        }
    }
    let (audit, audit_rows, kv, tokens) = rt.store().read(|c| {
        let audit: BTreeMap<u64, String> = basal_core::audit::rows(c, run_id)?
            .into_iter()
            .map(|r| (r.position, r.outcome))
            .collect();
        let rows: i64 = c.query_row(
            "SELECT COUNT(*) FROM call_audit WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        let mut stmt =
            c.prepare("SELECT key, value, revision FROM kv WHERE flow_id = ?1 ORDER BY key")?;
        let kv = stmt
            .query_map([&run.flow_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    format!("{}@{}", r.get::<_, String>(1)?, r.get::<_, i64>(2)?),
                ))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        let (open, input, cache_write, output, cached, unreported): (i64, i64, i64, i64, i64, i64) =
            c.query_row(
                "SELECT COALESCE(SUM(state = 'reserved'), 0), COALESCE(SUM(input_tokens), 0), \
                 COALESCE(SUM(cache_write_tokens), 0), COALESCE(SUM(output_tokens), 0), \
                 COALESCE(SUM(cached_input_tokens), 0), COALESCE(SUM(unreported_tokens), 0) \
                 FROM token_ledger WHERE run_id = ?1",
                [run_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )?;
        let tokens: BTreeMap<String, u64> = [
            ("open", open),
            ("input", input),
            ("cache_write", cache_write),
            ("output", output),
            ("cached_input", cached),
            ("unreported", unreported),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), u64::try_from(v).unwrap_or(u64::MAX)))
        .collect();
        Ok((audit, rows as usize, kv, tokens))
    })?;
    let mut first: HashMap<String, String> = HashMap::new();
    let mut broca_reuse = 0;
    for (send_id, bytes) in mock.broca_sends() {
        match first.get(&send_id) {
            Some(b) if *b != bytes => broca_reuse += 1,
            Some(_) => {}
            None => {
                first.insert(send_id, bytes);
            }
        }
    }
    let open = rt.health()?.open_obligations.len();
    Ok(Summary {
        state: run.state.as_str().to_owned(),
        result: run
            .result
            .as_deref()
            .and_then(|r| serde_json::from_str(r).ok()),
        error_kind: run.error_kind,
        effects,
        effects_per_call: per_call,
        audit,
        audit_rows,
        kv,
        tokens,
        broca_reuse,
        calls: calls.len(),
        open_obligations: open,
    })
}

/// One store, one mock and one worker source that outlive individual
/// runtimes, as the disk and the outside world outlive a process.
pub struct World {
    pub dir: PathBuf,
    pub mock: MockHost,
    pub selector: Arc<basal_host::selector::FakeSelector>,
    pub catalog: MockCatalog,
    pub source: Arc<ProcessSource>,
    pub durability: Durability,
}

impl World {
    pub fn new(tag: &str) -> Self {
        Self {
            dir: scratch(tag),
            mock: MockHost::new(),
            selector: Arc::new(basal_host::selector::FakeSelector::default()),
            catalog: MockCatalog::standard(),
            source: Arc::new(ProcessSource::new(worker_binary())),
            // The harness checks logic, not power loss; skipping F_FULLFSYNC
            // keeps hundreds of cuts fast. Durability is measured separately.
            durability: Durability { fullfsync: false },
        }
    }

    pub fn store_path(&self) -> PathBuf {
        self.dir.join("basal.db")
    }

    /// Opens the store and a runtime over it, recovering orphaned runs as a
    /// starting process does.
    pub fn runtime(&self, hooks: Arc<dyn Hooks>, config: Config) -> Result<Runtime, CoreError> {
        let store = Arc::new(Store::open(self.store_path(), self.durability)?);
        let rt = Runtime::new(
            store,
            Arc::new(self.mock.clone()),
            Arc::new(self.catalog.clone()),
            hooks,
            Some(self.source.clone()),
            Config {
                selector: self.selector.clone(),
                ..config
            },
        );
        rt.recover()?;
        Ok(rt)
    }

    /// A spec for `script` under the test manifest, approved first.
    pub fn spec(&self, rt: &Runtime, script: &str) -> Result<TriggerSpec, String> {
        self.spec_with(rt, script, &test_manifest())
    }

    /// A spec for `script` under `manifest`, approved first.
    pub fn spec_with(
        &self,
        rt: &Runtime,
        script: &str,
        manifest: &Value,
    ) -> Result<TriggerSpec, String> {
        approve_spec(
            rt,
            script,
            manifest,
            "trigger-1",
            JsonText::new(json!({"kind": "test"}).to_string()).unwrap_or_else(|_| JsonText::null()),
        )
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Drives a run to a terminal state or `needs_reconcile`. Whenever it
/// suspends, every long-running call the mock holds is completed: the
/// outside world answers only once the run is waiting, which keeps a race
/// against a long call deterministic.
pub fn drive(
    rt: &Runtime,
    mock: &MockHost,
    run_id: &str,
    timeout: Duration,
) -> Result<Run, CoreError> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let run = rt.run_to_rest(run_id, left)?;
        if run.state != RunState::Suspended || Instant::now() >= deadline {
            return Ok(run);
        }
        mock.complete_all();
        rt.wait_for(
            run_id,
            deadline.saturating_duration_since(Instant::now()),
            |s| s != RunState::Suspended,
        )?;
    }
}

/// The outcome of one cut scenario.
#[derive(Debug)]
pub struct CutRun {
    /// Whether each requested cut was reached.
    pub fired: Vec<bool>,
    pub summary: Summary,
    /// Boundaries passed by each phase, the uncut first phase included.
    pub phases: Vec<Vec<Point>>,
}

/// Runs `script` and crashes at each of `cuts` in turn (the first in the
/// first runtime, the second in the recovery after it, and so on), then
/// recovers and drives the run to its end.
pub fn run_with_cuts(
    world: &World,
    script: &str,
    cuts: &[Point],
    config: &Config,
) -> Result<CutRun, String> {
    run_with_cuts_under(world, script, &test_manifest(), cuts, config)
}

/// [`run_with_cuts`] under a manifest of the test's choosing.
pub fn run_with_cuts_under(
    world: &World,
    script: &str,
    manifest: &Value,
    cuts: &[Point],
    config: &Config,
) -> Result<CutRun, String> {
    let mut fired = Vec::new();
    let mut phases = Vec::new();
    let mut run_id = None;
    for phase in 0..=cuts.len() {
        let probe = Arc::new(match cuts.get(phase) {
            Some(p) => Probe::crash_at(p.clone()),
            None => Probe::recording(),
        });
        let rt = match world.runtime(probe.clone(), config.clone()) {
            Ok(rt) => rt,
            // Attaching to the host redelivers completions, so a cut can
            // land while the runtime is still opening.
            Err(CoreError::Cut) if probe.fired() => {
                fired.push(true);
                phases.push(probe.points());
                world.mock.detach();
                continue;
            }
            Err(e) => return Err(format!("phase {phase}: opening: {e}")),
        };
        let id = match &run_id {
            Some(id) => id,
            None => {
                let spec = world.spec_with(&rt, script, manifest)?;
                let admitted = rt.admit(&spec).map_err(|e| format!("admit: {e}"))?;
                run_id.insert(admitted.run_id().unwrap_or_default().to_owned())
            }
        };
        let outcome = drive(&rt, &world.mock, id, Duration::from_secs(120));
        rt.quiesce();
        phases.push(probe.points());
        let crashed = probe.fired();
        if phase < cuts.len() {
            fired.push(crashed);
        }
        if crashed {
            // A crash left the runtime unusable; the next phase recovers.
            world.mock.detach();
            drop(rt);
            continue;
        }
        let run = outcome.map_err(|e| format!("phase {phase}: driving: {e}"))?;
        let summary = summarize(&rt, &world.mock, &run.run_id).map_err(|e| e.to_string())?;
        // Any cuts not reached are reported as not fired.
        while fired.len() < cuts.len() {
            fired.push(false);
        }
        return Ok(CutRun {
            fired,
            summary,
            phases,
        });
    }
    Err("every phase crashed".into())
}
