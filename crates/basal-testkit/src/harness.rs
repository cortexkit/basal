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
    Boundary, Config, CoreError, Durability, Hooks, Run, RunState, Runtime, Step, Store,
    TriggerSpec,
};
use basal_host::mock::MockHost;
use basal_proto::JsonText;
use serde_json::{Value, json};

use crate::channel::{ProcessSource, worker_binary};

/// The representative run: a race whose loser is long-running and still
/// outstanding long after the winner was released, a synchronous clock read,
/// a caught rejection, a local eager effect, two calls completing in
/// parallel, a random sample, an `llm` call that suspends the run, and a
/// resume that reads the local effect back.
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
    a: a.applied.n,
    b: b.n,
    r: r >= 0 && r < 1,
    answer: answer.done,
    seen,
    monotonic: t2 >= t1,
};
"#;

/// A unique scratch directory.
pub fn scratch(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "basal-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
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
    fn at(&self, _run_id: &str, boundary: &Boundary) -> Step {
        let name = format!("{boundary:?}");
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
                action(_run_id, boundary);
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
    /// Local effects per call position.
    pub local_effects: BTreeMap<u64, usize>,
    pub kv: BTreeMap<String, String>,
    pub calls: usize,
    pub open_obligations: usize,
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
            "local_effects": positions(&self.local_effects),
            "kv": self.kv,
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
        *effects
            .entry(format!("{}.{} {}", e.module, e.op, e.args))
            .or_insert(0) += 1;
        if let Some(p) = positions.get(&e.key) {
            *per_call.entry(*p).or_insert(0) += 1;
        }
    }
    let (local, kv) = rt.store().read(|c| {
        let mut stmt = c.prepare(
            "SELECT idempotency_key, COUNT(*) FROM local_effects GROUP BY idempotency_key",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut stmt = c.prepare("SELECT key, value FROM local_kv ORDER BY key")?;
        let kv = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        Ok((rows, kv))
    })?;
    let local_effects = local
        .into_iter()
        .map(|(k, n)| (positions.get(&k).copied().unwrap_or(u64::MAX), n as usize))
        .collect();
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
        local_effects,
        kv,
        calls: calls.len(),
        open_obligations: open,
    })
}

/// One store, one mock and one worker source that outlive individual
/// runtimes, as the disk and the outside world outlive a process.
pub struct World {
    pub dir: PathBuf,
    pub mock: MockHost,
    pub source: Arc<ProcessSource>,
    pub durability: Durability,
}

impl World {
    pub fn new(tag: &str) -> Self {
        Self {
            dir: scratch(tag),
            mock: MockHost::new(),
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
            hooks,
            Some(self.source.clone()),
            config,
        );
        rt.recover()?;
        Ok(rt)
    }

    pub fn spec(&self, script: &str) -> TriggerSpec {
        TriggerSpec {
            flow_id: "flow-test".into(),
            trigger_id: "trigger-1".into(),
            trigger: JsonText::new(json!({"kind": "test"}).to_string())
                .unwrap_or_else(|_| JsonText::null()),
            script: script.to_owned(),
            manifest: "{\"id\":\"flow-test\"}".into(),
        }
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
        rt.wait_for(run_id, Duration::from_secs(5), |s| s != RunState::Suspended)?;
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
                let admitted = rt
                    .admit(&world.spec(script))
                    .map_err(|e| format!("admit: {e}"))?;
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
