//! The worker pool: greeted `ck-basal-worker` processes, bound to one flow
//! each for their whole life, kept warm so an activation never waits on a
//! spawn.
//!
//! Why workers are bound to a flow: a worker runs untrusted script, and an
//! engine compromised by one flow's payload keeps its process state after the
//! activation ends. If that process later ran another flow's activation, the
//! compromise would reach the other flow's data and its channel. So a fresh
//! worker is bound to the first flow it serves and is only ever reused for
//! that flow, until it is retired after an idle period or a number of
//! activations. A dry run's worker serves exactly one activation and is
//! retired, because a dry run executes code nobody has approved yet.
//!
//! Why workers are kept warm: a spawn costs about 10 ms, but on macOS the
//! first launch of a freshly signed binary, and a launch after a few idle
//! minutes, has been measured stalling for over a minute while the system
//! evaluates the signature. The pool keeps `warm_spares` greeted, unbound
//! workers ready, and replaces a killed, crashed or retired worker at once
//! in the background, so the stall is paid off the activation's path.
//!
//! A worker is handed out before its run is claimed, so a run's wall-clock
//! deadline (fixed at its first claim) never starts counting before the
//! worker answered its handshake.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use basal_core::Clock;
use basal_core::channel::{ChannelError, WorkerChannel};
use basal_proto::{ParentMessage, Welcome, WorkerMessage};

use crate::metrics::Metrics;
use crate::process::{SpawnError, WorkerProcess};

/// The pool's parameters. The defaults keep two spares (an install's dry
/// run uses up to ten single-use workers in a row), cap the pool at 16
/// processes (about 5 MiB each when idle), retire a worker after 256
/// activations or 10 idle minutes so no engine process lives indefinitely,
/// and give a handshake 180 s because a launch has been measured stalling
/// for over a minute while macOS assesses the binary.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// `ck-basal-worker`; by default the one beside the running binary.
    pub worker_binary: PathBuf,
    /// Greeted, unbound workers kept ready.
    pub warm_spares: usize,
    /// Live worker processes at most, spares, idle, busy and starting
    /// included. When it is reached, the longest-idle bound worker is
    /// retired to make room.
    pub max_workers: usize,
    /// Activations one worker serves before it is retired.
    pub max_activations: u32,
    /// How long a bound worker may sit idle before it is retired, on the
    /// runtime's clock.
    pub idle_retire: Duration,
    /// How long a new worker has to answer its handshake.
    pub handshake_timeout: Duration,
    /// How long an activation waits for a worker when the pool is at
    /// `max_workers` and every worker is busy.
    pub acquire_timeout: Duration,
}

impl PoolConfig {
    pub fn new(worker_binary: impl Into<PathBuf>) -> Self {
        Self {
            worker_binary: worker_binary.into(),
            warm_spares: 2,
            max_workers: 16,
            max_activations: 256,
            idle_retire: Duration::from_secs(10 * 60),
            handshake_timeout: Duration::from_secs(180),
            acquire_timeout: Duration::from_secs(300),
        }
    }

    /// The worker beside the running executable, which is how the module is
    /// installed: both binaries in one directory.
    pub fn beside_current_exe() -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let dir = exe
            .parent()
            .ok_or_else(|| std::io::Error::other("the executable has no directory"))?;
        Ok(Self::new(dir.join("ck-basal-worker")))
    }
}

/// Starts greeted workers. The pool's only way to make a process, so tests
/// can hold a spawn at a gate.
pub trait Spawn: Send + Sync {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError>;
}

/// Spawns the configured worker binary.
pub struct ProcessSpawner {
    binary: PathBuf,
    timeout: Duration,
}

impl ProcessSpawner {
    pub fn new(config: &PoolConfig) -> Self {
        Self {
            binary: config.worker_binary.clone(),
            timeout: config.handshake_timeout,
        }
    }
}

impl Spawn for ProcessSpawner {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError> {
        let process = WorkerProcess::start(&self.binary, self.timeout)?;
        // A worker that could not sandbox itself would run flow code with
        // file and network access; it is refused, not used.
        if process.welcome().confinement != basal_proto::Confinement::Seatbelt {
            return Err(SpawnError::Handshake(
                "the worker reports no OS sandbox; refusing it".into(),
            ));
        }
        Ok(process)
    }
}

/// What a worker serves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Binding {
    /// Activations of this flow only.
    Flow(String),
    /// One dry-run activation, then retirement.
    DryRun(String),
}

/// Where a handed-out worker came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// An idle worker already bound to the same flow.
    Bound,
    /// A warm, unbound spare.
    Spare,
    /// Spawned for this request because nothing was ready.
    Spawned,
}

/// One worker handed out, for tests and the findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handout {
    pub worker: u64,
    pub pid: u32,
    pub binding: Binding,
    pub source: Source,
}

#[derive(Debug)]
pub enum PoolError {
    Spawn(SpawnError),
    /// Every worker stayed busy for the whole acquire timeout.
    Exhausted,
    Stopped,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "{e}"),
            Self::Exhausted => f.write_str("no worker became free in time"),
            Self::Stopped => f.write_str("the pool is stopped"),
        }
    }
}

impl std::error::Error for PoolError {}

struct Idle {
    id: u64,
    process: WorkerProcess,
    activations: u32,
    idle_since_ms: i64,
}

struct Busy {
    pid: u32,
    run: Option<String>,
}

#[derive(Default)]
struct State {
    spares: VecDeque<(u64, WorkerProcess)>,
    bound: HashMap<String, Vec<Idle>>,
    busy: HashMap<u64, Busy>,
    /// Live processes: spares, bound idle, busy, and spawns in progress.
    live: usize,
    /// Background spawns of spares in progress.
    spawning: usize,
    /// Killed or crashed workers not yet replaced.
    owed_respawns: u64,
    handouts: Vec<Handout>,
    stopped: bool,
}

struct Shared {
    config: PoolConfig,
    spawner: Arc<dyn Spawn>,
    clock: Clock,
    metrics: Arc<Metrics>,
    state: Mutex<State>,
    cond: Condvar,
    next_id: AtomicU64,
}

/// The pool. Cloning shares it.
#[derive(Clone)]
pub struct Pool {
    shared: Arc<Shared>,
}

/// Counts for health and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStats {
    pub live: usize,
    pub spares: usize,
    pub spawning: usize,
    pub busy: usize,
    /// Idle bound workers per flow.
    pub bound: Vec<(String, usize)>,
}

impl Pool {
    pub fn new(
        config: PoolConfig,
        spawner: Arc<dyn Spawn>,
        clock: Clock,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                spawner,
                clock,
                metrics,
                state: Mutex::new(State::default()),
                cond: Condvar::new(),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    pub fn config(&self) -> &PoolConfig {
        &self.shared.config
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.lock()
    }

    /// Hands out a greeted worker for `binding`: an idle one already bound to
    /// the same flow, else a warm spare (bound from now on), else one spawned
    /// now. Never a worker bound to anything else.
    pub fn acquire(&self, binding: Binding) -> Result<Lease, PoolError> {
        let deadline = Instant::now() + self.shared.config.acquire_timeout;
        let taken = loop {
            let mut state = self.lock();
            if state.stopped {
                return Err(PoolError::Stopped);
            }
            if let Binding::Flow(flow) = &binding {
                let reuse = state.bound.get_mut(flow).and_then(Vec::pop);
                if let Some(mut idle) = reuse {
                    if idle.process.has_exited() {
                        self.shared.crashed(&mut state);
                        drop(idle);
                        continue;
                    }
                    break (idle.id, idle.process, idle.activations, Source::Bound);
                }
            }
            if let Some((id, mut process)) = state.spares.pop_front() {
                if process.has_exited() {
                    self.shared.crashed(&mut state);
                    drop(process);
                    continue;
                }
                break (id, process, 0, Source::Spare);
            }
            if state.live >= self.shared.config.max_workers {
                // Make room by retiring the bound worker idle longest.
                if !self.shared.retire_longest_idle(&mut state) {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(PoolError::Exhausted);
                    }
                    let _ = self
                        .shared
                        .cond
                        .wait_timeout(state, (deadline - now).min(Duration::from_secs(1)));
                    continue;
                }
            }
            state.live += 1;
            drop(state);
            Metrics::bump(&self.shared.metrics.spawned_on_demand);
            match self.shared.spawn_counted() {
                Ok(process) => {
                    let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
                    break (id, process, 0, Source::Spawned);
                }
                Err(e) => {
                    let mut state = self.lock();
                    state.live = state.live.saturating_sub(1);
                    self.shared.cond.notify_all();
                    return Err(PoolError::Spawn(e));
                }
            }
        };
        let (id, process, activations, source) = taken;
        {
            let mut state = self.lock();
            state.busy.insert(
                id,
                Busy {
                    pid: process.pid(),
                    run: None,
                },
            );
            state.handouts.push(Handout {
                worker: id,
                pid: process.pid(),
                binding: binding.clone(),
                source,
            });
        }
        // A spare was used: start its replacement now.
        self.replenish();
        Ok(Lease {
            pool: self.clone(),
            id,
            process: Some(process),
            binding,
            activations,
            killed: false,
        })
    }

    /// Starts background spawns until `warm_spares` spares exist or are on
    /// their way, within `max_workers`.
    pub fn replenish(&self) {
        let mut state = self.lock();
        if state.stopped {
            return;
        }
        let wanted = self.shared.config.warm_spares;
        while state.spares.len() + state.spawning < wanted
            && state.live < self.shared.config.max_workers
        {
            state.live += 1;
            state.spawning += 1;
            let shared = self.shared.clone();
            thread::spawn(move || shared.spawn_spare());
        }
    }

    /// Housekeeping, called by the engine's loop: retires bound workers idle
    /// past `idle_retire`, drops idle workers that died, and tops up spares.
    pub fn maintain(&self) {
        let now = self.shared.clock.now_ms();
        let idle_ms = i64::try_from(self.shared.config.idle_retire.as_millis()).unwrap_or(i64::MAX);
        let mut doomed = Vec::new();
        {
            let mut state = self.lock();
            let mut crashed = 0;
            let mut retired = 0;
            for workers in state.bound.values_mut() {
                let mut keep = Vec::with_capacity(workers.len());
                for mut idle in workers.drain(..) {
                    if idle.process.has_exited() {
                        crashed += 1;
                        doomed.push(idle.process);
                    } else if now.saturating_sub(idle.idle_since_ms) >= idle_ms {
                        retired += 1;
                        doomed.push(idle.process);
                    } else {
                        keep.push(idle);
                    }
                }
                *workers = keep;
            }
            state.bound.retain(|_, w| !w.is_empty());
            let mut spares = VecDeque::new();
            while let Some((id, mut process)) = state.spares.pop_front() {
                if process.has_exited() {
                    crashed += 1;
                    doomed.push(process);
                } else {
                    spares.push_back((id, process));
                }
            }
            state.spares = spares;
            for _ in 0..crashed {
                self.shared.crashed(&mut state);
            }
            for _ in 0..retired {
                self.shared.lost(&mut state, false);
                Metrics::bump(&self.shared.metrics.workers_retired);
            }
        }
        // Killed outside the lock: reaping can take a moment.
        drop(doomed);
        self.replenish();
    }

    /// Kills the worker serving `run_id`, from outside its activation, the
    /// way a crash or the operating system would. For tests and the harness.
    /// Returns whether one was found.
    pub fn kill_for_run(&self, run_id: &str) -> bool {
        let pid = self
            .lock()
            .busy
            .values()
            .find(|b| b.run.as_deref() == Some(run_id))
            .map(|b| b.pid);
        match pid.and_then(|p| libc::pid_t::try_from(p).ok()) {
            Some(pid) => {
                // SAFETY: kill(2) has no memory-safety preconditions; the pid
                // is a child this process spawned and has not reaped (its
                // lease holds the process, which reaps only when dropped).
                unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
            }
            None => false,
        }
    }

    /// Waits until at least `n` spares are ready, up to `timeout`.
    pub fn wait_for_spares(&self, n: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if state.spares.len() >= n {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .shared
                .cond
                .wait_timeout(state, deadline - now)
                .map(|(g, _)| g)
                .unwrap_or_else(|p| p.into_inner().0);
        }
    }

    /// Every worker handed out so far, in order.
    pub fn handouts(&self) -> Vec<Handout> {
        self.lock().handouts.clone()
    }

    pub fn stats(&self) -> PoolStats {
        let state = self.lock();
        let mut bound: Vec<(String, usize)> = state
            .bound
            .iter()
            .map(|(f, w)| (f.clone(), w.len()))
            .collect();
        bound.sort();
        PoolStats {
            live: state.live,
            spares: state.spares.len(),
            spawning: state.spawning,
            busy: state.busy.len(),
            bound,
        }
    }

    /// Stops handing out workers and kills every idle one.
    pub fn stop(&self) {
        let doomed: Vec<WorkerProcess> = {
            let mut state = self.lock();
            state.stopped = true;
            let mut doomed: Vec<WorkerProcess> = state.spares.drain(..).map(|(_, p)| p).collect();
            for (_, workers) in state.bound.drain() {
                doomed.extend(workers.into_iter().map(|i| i.process));
            }
            state.live = state.live.saturating_sub(doomed.len());
            doomed
        };
        drop(doomed);
        self.shared.cond.notify_all();
    }

    /// Takes a lease back. A worker that was killed, died, served a dry run
    /// or reached its activation count is ended and replaced; any other goes
    /// back to its flow's idle list.
    fn release(&self, lease: &mut Lease) {
        let Some(mut process) = lease.process.take() else {
            return;
        };
        let now = self.shared.clock.now_ms();
        let mut state = self.lock();
        state.busy.remove(&lease.id);
        let activations = lease.activations.saturating_add(1);
        let crashed = !lease.killed && process.has_exited();
        let keep = match (&lease.binding, lease.killed || crashed) {
            (_, true) => None,
            (Binding::DryRun(_), false) => None,
            (Binding::Flow(_), false) if activations >= self.shared.config.max_activations => None,
            (Binding::Flow(flow), false) if !state.stopped => Some(flow.clone()),
            (Binding::Flow(_), false) => None,
        };
        match keep {
            Some(flow) => {
                state.bound.entry(flow).or_default().push(Idle {
                    id: lease.id,
                    process,
                    activations,
                    idle_since_ms: now,
                });
            }
            None => {
                if lease.killed {
                    Metrics::bump(&self.shared.metrics.workers_killed);
                    self.shared.lost(&mut state, true);
                } else if crashed {
                    self.shared.crashed(&mut state);
                } else {
                    Metrics::bump(&self.shared.metrics.workers_retired);
                    self.shared.lost(&mut state, false);
                }
                drop(state);
                process.kill();
                self.shared.cond.notify_all();
                self.replenish();
                return;
            }
        }
        drop(state);
        self.shared.cond.notify_all();
    }

    fn set_run(&self, id: u64, run_id: &str) {
        if let Some(b) = self.lock().busy.get_mut(&id) {
            b.run = Some(run_id.to_owned());
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic on another thread must not make the pool unusable.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Accounts for a worker that is gone. One killed or crashed is owed a
    /// replacement, counted as a respawn when it arrives.
    fn lost(&self, state: &mut State, replace: bool) {
        state.live = state.live.saturating_sub(1);
        if replace {
            state.owed_respawns += 1;
        }
    }

    /// Accounts for a worker found dead that nobody killed.
    fn crashed(&self, state: &mut State) {
        Metrics::bump(&self.metrics.workers_crashed);
        self.lost(state, true);
    }

    /// Retires the bound worker idle longest, to make room. Returns whether
    /// there was one.
    fn retire_longest_idle(&self, state: &mut State) -> bool {
        let oldest = state
            .bound
            .iter()
            .flat_map(|(flow, workers)| {
                workers
                    .iter()
                    .enumerate()
                    .map(move |(i, w)| (w.idle_since_ms, flow.clone(), i))
            })
            .min();
        let Some((_, flow, i)) = oldest else {
            return false;
        };
        let Some(workers) = state.bound.get_mut(&flow) else {
            return false;
        };
        if i >= workers.len() {
            return false;
        }
        let idle = workers.remove(i);
        if workers.is_empty() {
            state.bound.remove(&flow);
        }
        state.live = state.live.saturating_sub(1);
        Metrics::bump(&self.metrics.workers_retired);
        drop(idle);
        true
    }

    fn spawn_counted(&self) -> Result<WorkerProcess, SpawnError> {
        match self.spawner.spawn() {
            Ok(p) => {
                Metrics::bump(&self.metrics.workers_spawned);
                Ok(p)
            }
            Err(e) => {
                Metrics::bump(&self.metrics.spawn_failures);
                tracing::warn!(target: "pool", "spawning a worker failed: {e}");
                Err(e)
            }
        }
    }

    /// One background spawn of a spare. `live` and `spawning` were counted
    /// by the caller.
    fn spawn_spare(self: Arc<Self>) {
        let spawned = self.spawn_counted();
        let mut state = self.lock();
        state.spawning = state.spawning.saturating_sub(1);
        match spawned {
            Ok(process) if !state.stopped => {
                if state.owed_respawns > 0 {
                    state.owed_respawns -= 1;
                    Metrics::bump(&self.metrics.workers_respawned);
                }
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                state.spares.push_back((id, process));
            }
            Ok(process) => {
                state.live = state.live.saturating_sub(1);
                drop(state);
                drop(process);
                self.cond.notify_all();
                return;
            }
            Err(_) => {
                state.live = state.live.saturating_sub(1);
            }
        }
        drop(state);
        self.cond.notify_all();
    }
}

/// The pool as the runtime's own worker source. The runtime asks a source
/// for a worker only when it resumes a run by itself, which the module never
/// lets it do (the engine drives every run). A worker handed out this way
/// knows nothing of the run it will serve, so it is single-use and can never
/// carry one flow's state into another's.
pub struct PoolSource {
    pub pool: Pool,
}

impl basal_core::WorkerSource for PoolSource {
    fn worker(&self) -> Result<Box<dyn WorkerChannel>, ChannelError> {
        self.pool
            .acquire(Binding::DryRun("runtime-source".into()))
            .map(|lease| Box::new(lease) as Box<dyn WorkerChannel>)
            .map_err(|e| ChannelError::Broken(e.to_string()))
    }
}

/// A worker handed out for one activation. It goes back to the pool (or is
/// ended) when dropped.
pub struct Lease {
    pool: Pool,
    id: u64,
    process: Option<WorkerProcess>,
    binding: Binding,
    activations: u32,
    killed: bool,
}

impl Lease {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(WorkerProcess::pid)
    }

    /// Names the run this worker is about to serve, so it can be found by
    /// run (see [`Pool::kill_for_run`]).
    pub fn serve_run(&self, run_id: &str) {
        self.pool.set_run(self.id, run_id);
    }

    /// Whether the activation ended with the worker killed.
    pub fn was_killed(&self) -> bool {
        self.killed
    }
}

impl WorkerChannel for Lease {
    fn welcome(&self) -> &Welcome {
        // A lease always holds its process until it is dropped; the only
        // way to lose it is release, which happens in drop.
        match &self.process {
            Some(p) => p.welcome(),
            None => unreachable_welcome(),
        }
    }

    fn send(&mut self, message: &ParentMessage) -> Result<(), ChannelError> {
        match self.process.as_mut() {
            Some(p) => p.send(message),
            None => Err(ChannelError::Closed),
        }
    }

    fn recv(&mut self, timeout: Duration) -> Result<WorkerMessage, ChannelError> {
        match self.process.as_mut() {
            Some(p) => p.recv(timeout),
            None => Err(ChannelError::Closed),
        }
    }

    fn kill(&mut self) {
        self.killed = true;
        if let Some(p) = self.process.as_mut() {
            p.kill();
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let pool = self.pool.clone();
        pool.release(self);
    }
}

/// A welcome for a lease whose process is gone. Never reached: the process
/// is taken only in drop. Kept total so no path can panic.
fn unreachable_welcome() -> &'static Welcome {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<Welcome> = OnceLock::new();
    EMPTY.get_or_init(|| Welcome {
        protocol_version: 0,
        engine: String::from("none"),
        prelude_hash: basal_proto::PreludeHash([0; 32]),
        confinement: basal_proto::Confinement::None,
    })
}
