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
//! workers ready, and replenishes that reserve in the background. A dead
//! bound worker is replaced on its flow's next activation, not by a new
//! worker pre-bound to an idle flow. Failed spawns back off to avoid repeatedly
//! launching a broken or unavailable binary.
//!
//! A worker is handed out before its run is claimed, so a run's wall-clock
//! deadline (fixed at its first claim) never starts counting before the
//! worker answered its handshake.

use std::collections::{HashMap, VecDeque};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use basal_core::Clock;
use basal_core::channel::{ChannelError, WorkerChannel, WorkerReceiver};
use basal_proto::{Confinement, LANDLOCK_ABI, ParentMessage, Welcome, WorkerMessage};

use crate::metrics::Metrics;
pub use crate::process::LandlockPolicy;
use crate::process::{SpawnError, WorkerKiller, WorkerLaunch, WorkerProcess};

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
    /// How workers are launched (see `crate::process`). On macOS the module
    /// re-executes itself to detach the worker from its parent's privacy grants;
    /// on Linux it launches the worker directly. Test binaries use direct launch
    /// because they do not implement the macOS re-execution mode.
    pub worker_launch: WorkerLaunch,
    /// Required by default. Optional permits seccomp-only Linux workers when
    /// Landlock is unavailable, but never permits an invalid Landlock report.
    pub landlock: LandlockPolicy,
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
    pub fn new(worker_binary: impl Into<PathBuf>, worker_launch: WorkerLaunch) -> Self {
        Self {
            worker_binary: worker_binary.into(),
            worker_launch,
            landlock: LandlockPolicy::Required,
            warm_spares: 2,
            max_workers: 16,
            max_activations: 256,
            idle_retire: Duration::from_secs(10 * 60),
            handshake_timeout: Duration::from_secs(180),
            acquire_timeout: Duration::from_secs(300),
        }
    }

    /// Locates the worker in this executable's directory by appending `-worker`
    /// to its basename, matching the deploy script's installation layout. It
    /// detaches workers from the parent's privacy grants through the running
    /// executable on macOS. Linux launches the worker directly with an explicit
    /// Landlock policy; the worker installs confinement before reading input.
    pub fn beside_current_exe() -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        #[cfg(target_os = "macos")]
        let launch = WorkerLaunch::Disclaimed {
            trampoline: exe.clone(),
        };
        #[cfg(not(target_os = "macos"))]
        let launch = WorkerLaunch::Plain;
        Ok(Self::new(sibling_worker(&exe)?, launch))
    }
}

// The file name, not the signing identifier, distinguishes development copies
// from the placed fleet. Deriving both names keeps their lookup layouts alike.
// A trailing `.exe` (any case) is kept at the end, so `ck-basal.exe` finds
// `ck-basal-worker.exe`.
fn sibling_worker(exe: &Path) -> std::io::Result<PathBuf> {
    let file_name = exe
        .file_name()
        .ok_or_else(|| std::io::Error::other("the executable has no file name"))?;
    let (stem, exe_suffix) = split_exe_suffix(file_name);
    let mut name = stem.to_os_string();
    name.push("-worker");
    if exe_suffix {
        name.push(".exe");
    }
    Ok(exe.with_file_name(name))
}

/// Splits a trailing `.exe`, compared case-insensitively, off a file name.
fn split_exe_suffix(name: &std::ffi::OsStr) -> (&std::ffi::OsStr, bool) {
    let bytes = name.as_encoded_bytes();
    let Some(split) = bytes.len().checked_sub(4) else {
        return (name, false);
    };
    if !bytes[split..].eq_ignore_ascii_case(b".exe") {
        return (name, false);
    }
    // SAFETY: the split falls immediately before an ASCII `.`, which the
    // standard library documents as a valid boundary of the encoding.
    (
        unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&bytes[..split]) },
        true,
    )
}

#[cfg(test)]
mod names {
    use super::*;

    #[test]
    fn worker_file_name_follows_parent_in_production_and_development() {
        assert_eq!(
            sibling_worker(Path::new("/placed/ck-basal")).unwrap(),
            Path::new("/placed/ck-basal-worker")
        );
        assert_eq!(
            sibling_worker(Path::new("/scratch/ckdev-basal")).unwrap(),
            Path::new("/scratch/ckdev-basal-worker")
        );
        assert_eq!(
            sibling_worker(Path::new("/placed/ck-basal.exe")).unwrap(),
            Path::new("/placed/ck-basal-worker.exe")
        );
        assert_eq!(
            sibling_worker(Path::new("/scratch/ckdev-basal.EXE")).unwrap(),
            Path::new("/scratch/ckdev-basal-worker.exe")
        );
        // Only a whole trailing `.exe` is an extension to keep at the end.
        assert_eq!(
            sibling_worker(Path::new("/placed/ck-basal.exe.old")).unwrap(),
            Path::new("/placed/ck-basal.exe.old-worker")
        );
        assert_eq!(
            sibling_worker(Path::new("/placed/exe")).unwrap(),
            Path::new("/placed/exe-worker")
        );
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
    launch: WorkerLaunch,
    timeout: Duration,
    landlock: LandlockPolicy,
    codemode: bool,
}

impl ProcessSpawner {
    pub fn new(config: &PoolConfig) -> Self {
        Self {
            binary: config.worker_binary.clone(),
            launch: config.worker_launch.clone(),
            timeout: config.handshake_timeout,
            landlock: config.landlock,
            codemode: false,
        }
    }

    /// Applies the flow worker's per-OS sandbox checks, plus the Linux
    /// address-space cap for codemode. No address-space cap is set on macOS.
    pub fn codemode(config: &PoolConfig) -> Self {
        Self {
            codemode: true,
            ..Self::new(config)
        }
    }
}

impl Spawn for ProcessSpawner {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError> {
        let process = WorkerProcess::start_for_profile(
            &self.binary,
            &self.launch,
            self.timeout,
            self.landlock,
            self.codemode,
        )?;
        // A worker that could not sandbox itself would run flow code with
        // file and network access; it is refused, not used.
        if !accepts_welcome(process.welcome(), self.landlock) {
            return Err(SpawnError::Handshake(
                "the worker's confinement does not satisfy the OS policy; refusing it".into(),
            ));
        }
        Ok(process)
    }
}

/// Require the applied Landlock ABI to equal min(kernel ABI, pinned worker ABI):
/// a smaller ABI would omit restrictions that both sides support.
fn accepts_welcome(welcome: &Welcome, landlock: LandlockPolicy) -> bool {
    match welcome.confinement {
        Confinement::Seatbelt => cfg!(target_os = "macos"),
        Confinement::Linux {
            seccomp,
            landlock: report,
        } => {
            cfg!(target_os = "linux")
                && seccomp
                && match report {
                    None => landlock == LandlockPolicy::Optional,
                    Some(report) => {
                        report.runtime_abi >= 1
                            && report.applied_abi == report.runtime_abi.min(LANDLOCK_ABI)
                    }
                }
        }
        Confinement::Windows {
            lpac,
            untrusted,
            no_thread_token,
            mitigations,
            handle_table,
        } => {
            #[cfg(windows)]
            {
                crate::windows::accepts_report(
                    lpac,
                    untrusted,
                    no_thread_token,
                    mitigations,
                    handle_table,
                )
            }
            #[cfg(not(windows))]
            {
                let _ = (lpac, untrusted, no_thread_token, mitigations, handle_table);
                false
            }
        }
        Confinement::None => false,
    }
}

/// What a worker serves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Binding {
    /// Activations of this flow only.
    Flow(String),
    /// One dry-run activation, then retirement.
    DryRun(String),
    /// One codemode run, never returned to a flow's idle list.
    Codemode,
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

/// One worker handed out, for tests and measurements.
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
    Backoff {
        retry_in: Duration,
    },
    /// Every worker stayed busy for the whole acquire timeout.
    Exhausted,
    Stopped,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "{e}"),
            Self::Backoff { retry_in } => write!(f, "worker spawns backed off for {retry_in:?}"),
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
    killer: WorkerKiller,
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
    /// Lost workers whose next successful spawn counts as a replacement.
    /// Bounded by the pool's maximum size, not an unbounded history of losses.
    owed_respawns: u64,
    handouts: VecDeque<Handout>,
    spawn_failures: u32,
    spawn_error: Option<String>,
    retry_at_ms: i64,
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
    notify: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
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
    pub spawn_error: Option<String>,
    pub spawn_retry_in_ms: i64,
}

const HANDOUT_HISTORY: usize = 1024;
const SPAWN_RETRY_MIN_MS: i64 = 250;
const SPAWN_RETRY_MAX_MS: i64 = 30_000;

impl State {
    fn record_handout(&mut self, handout: Handout) {
        self.handouts.push_back(handout);
        if self.handouts.len() > HANDOUT_HISTORY {
            self.handouts.pop_front();
        }
    }
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
                notify: Mutex::new(None),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.lock()
    }

    pub(crate) fn on_change(&self, notify: Arc<dyn Fn() + Send + Sync>) {
        *self.shared.notify.lock().unwrap_or_else(|p| p.into_inner()) = Some(notify);
    }

    /// Retirement and failed-spawn retry use the same clock as run timers.
    /// Dead idle children are discovered by the engine's slow fallback.
    pub(crate) fn next_maintenance_at(&self) -> Option<i64> {
        let state = self.lock();
        if state.stopped {
            return None;
        }
        let idle_ms = i64::try_from(self.shared.config.idle_retire.as_millis()).unwrap_or(i64::MAX);
        let retirement = state
            .bound
            .values()
            .flatten()
            .map(|w| w.idle_since_ms.saturating_add(idle_ms))
            .min();
        let retry = state
            .spawn_error
            .as_ref()
            .map(|_| state.retry_at_ms)
            .filter(|at| *at > self.shared.clock.now_ms());
        retirement.into_iter().chain(retry).min()
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
                        let status = idle.process.exit_status().unwrap_or_else(|| {
                            Err(std::io::Error::other("worker exit status unavailable"))
                        });
                        self.shared.crashed(&mut state, status);
                        drop(idle);
                        continue;
                    }
                    break (idle.id, idle.process, idle.activations, Source::Bound);
                }
            }
            if let Some((id, mut process)) = state.spares.pop_front() {
                if let Some(status) = process.exit_status() {
                    self.shared.crashed(&mut state, status);
                    drop(process);
                    continue;
                }
                break (id, process, 0, Source::Spare);
            }
            if state.live >= self.shared.config.max_workers {
                // Make room by retiring the bound worker idle longest.
                if let Some(idle) = self.shared.retire_longest_idle(&mut state) {
                    // Reaping must not block other leases or housekeeping.
                    drop(state);
                    drop(idle);
                    continue;
                } else {
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
            let retry_in = state.retry_at_ms.saturating_sub(self.shared.clock.now_ms());
            if state.spawn_error.is_some() && retry_in > 0 {
                return Err(PoolError::Backoff {
                    retry_in: Duration::from_millis(retry_in as u64),
                });
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
                    killer: process.killer(),
                    run: None,
                },
            );
            state.record_handout(Handout {
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
        if state.spawn_error.is_some() && self.shared.clock.now_ms() < state.retry_at_ms {
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
            let mut crashes = Vec::new();
            let mut retired = 0;
            for workers in state.bound.values_mut() {
                let mut keep = Vec::with_capacity(workers.len());
                for mut idle in workers.drain(..) {
                    if let Some(status) = idle.process.exit_status() {
                        crashes.push(status);
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
                if let Some(status) = process.exit_status() {
                    crashes.push(status);
                    doomed.push(process);
                } else {
                    spares.push_back((id, process));
                }
            }
            state.spares = spares;
            let crashed = crashes.len();
            for status in crashes {
                self.shared.crashed(&mut state, status);
            }
            for _ in 0..retired {
                self.shared.lost(&mut state, false);
                Metrics::bump(&self.shared.metrics.workers_retired);
            }
            if crashed + retired > 0 {
                self.shared.cond.notify_all();
                self.shared.changed();
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
        let killer = self
            .lock()
            .busy
            .values()
            .find(|b| b.run.as_deref() == Some(run_id))
            .map(|b| b.killer.clone());
        killer.is_some_and(|k| k.kill())
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

    /// The most recent 1024 handouts, in order. This diagnostic history must
    /// not grow with the lifetime of a production module.
    pub fn handouts(&self) -> Vec<Handout> {
        self.lock().handouts.iter().cloned().collect()
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
            spawn_error: state.spawn_error.clone(),
            spawn_retry_in_ms: if state.spawn_error.is_some() {
                state
                    .retry_at_ms
                    .saturating_sub(self.shared.clock.now_ms())
                    .max(0)
            } else {
                0
            },
        }
    }

    /// The operator's confinement policy and deaths attributed to a
    /// confinement fault, published under the name of this OS's fault:
    /// `sigsys_deaths` on Unix, `fatal_status_deaths` on Windows.
    pub fn worker_confinement(&self) -> serde_json::Value {
        let mut confinement = serde_json::json!({
            "os": std::env::consts::OS,
            "landlock": if cfg!(target_os = "linux") {
                Some(self.shared.config.landlock.as_str())
            } else {
                None
            },
        });
        confinement[CONFINEMENT_FAULT_DEATHS] = self
            .shared
            .metrics
            .sigsys_deaths
            .load(Ordering::Relaxed)
            .into();
        confinement
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
        self.shared.changed();
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
        let status = process.exit_status();
        // A channel failure can make drive request a kill after a confinement
        // fault has already ended the worker. Retain that death as a confinement crash.
        let crash_status = status.filter(|status| !lease.killed || is_confinement_fault(status));
        let crashed = crash_status.is_some();
        let keep = match (&lease.binding, lease.killed || crashed) {
            (_, true) => None,
            (Binding::DryRun(_) | Binding::Codemode, false) => None,
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
                if let Some(status) = crash_status {
                    self.shared.crashed(&mut state, status);
                } else if lease.killed {
                    Metrics::bump(&self.shared.metrics.workers_killed);
                    self.shared.lost(&mut state, true);
                } else {
                    Metrics::bump(&self.shared.metrics.workers_retired);
                    self.shared.lost(&mut state, false);
                }
                drop(state);
                process.kill();
                self.shared.cond.notify_all();
                self.shared.changed();
                self.replenish();
                return;
            }
        }
        drop(state);
        self.shared.cond.notify_all();
        self.shared.changed();
    }

    fn set_run(&self, id: u64, run_id: &str) {
        if let Some(b) = self.lock().busy.get_mut(&id) {
            b.run = Some(run_id.to_owned());
        }
    }
}

impl Shared {
    fn changed(&self) {
        let notify = self
            .notify
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(notify) = notify {
            notify();
        }
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic on another thread must not make the pool unusable.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Accounts for a worker that is gone. One killed or crashed is owed a
    /// replacement, counted as a respawn when it arrives.
    fn lost(&self, state: &mut State, replace: bool) {
        state.live = state.live.saturating_sub(1);
        if replace {
            state.owed_respawns = state
                .owed_respawns
                .saturating_add(1)
                .min(self.config.max_workers as u64);
        }
    }

    /// Accounts for a worker found dead that nobody killed.
    fn crashed(&self, state: &mut State, status: std::io::Result<ExitStatus>) {
        Metrics::bump(&self.metrics.workers_crashed);
        if is_confinement_fault(&status) {
            Metrics::bump(&self.metrics.sigsys_deaths);
        }
        self.lost(state, true);
    }

    /// Removes the bound worker idle longest; the caller reaps it outside
    /// the mutex, so releasing other workers never waits for this child.
    fn retire_longest_idle(&self, state: &mut State) -> Option<Idle> {
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
        let (_, flow, i) = oldest?;
        let workers = state.bound.get_mut(&flow)?;
        if i >= workers.len() {
            return None;
        }
        let idle = workers.remove(i);
        if workers.is_empty() {
            state.bound.remove(&flow);
        }
        state.live = state.live.saturating_sub(1);
        Metrics::bump(&self.metrics.workers_retired);
        Some(idle)
    }

    fn spawn_counted(&self) -> Result<WorkerProcess, SpawnError> {
        match self.spawner.spawn() {
            Ok(p) => {
                Metrics::bump(&self.metrics.workers_spawned);
                let mut state = self.lock();
                state.spawn_failures = 0;
                state.spawn_error = None;
                state.retry_at_ms = 0;
                if state.owed_respawns > 0 && !state.stopped {
                    state.owed_respawns -= 1;
                    Metrics::bump(&self.metrics.workers_respawned);
                }
                Ok(p)
            }
            Err(e) => {
                Metrics::bump(&self.metrics.spawn_failures);
                let mut state = self.lock();
                state.spawn_failures = state.spawn_failures.saturating_add(1);
                let delay = SPAWN_RETRY_MIN_MS
                    .saturating_mul(1i64 << state.spawn_failures.saturating_sub(1).min(7))
                    .min(SPAWN_RETRY_MAX_MS);
                state.retry_at_ms = self.clock.now_ms().saturating_add(delay);
                state.spawn_error = Some(e.to_string());
                self.changed();
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
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                state.spares.push_back((id, process));
            }
            Ok(process) => {
                state.live = state.live.saturating_sub(1);
                drop(state);
                drop(process);
                self.cond.notify_all();
                self.changed();
                return;
            }
            Err(_) => {
                state.live = state.live.saturating_sub(1);
            }
        }
        drop(state);
        self.cond.notify_all();
        self.changed();
    }
}

/// The health key the confinement-fault count is published under.
pub const CONFINEMENT_FAULT_DEATHS: &str = if cfg!(windows) {
    "fatal_status_deaths"
} else {
    "sigsys_deaths"
};

/// Whether a worker died of its confinement: SIGSYS on Unix (seccomp or the
/// macOS sandbox), a fatal access-violation or invalid-handle status on
/// Windows. A worker's own startup refusal (exit 70) and a kill are not.
fn is_confinement_fault(status: &std::io::Result<ExitStatus>) -> bool {
    #[cfg(unix)]
    {
        matches!(status, Ok(status) if status.signal() == Some(libc::SIGSYS))
    }
    #[cfg(windows)]
    {
        matches!(status, Ok(status) if status.code().is_some_and(|code| crate::windows::is_fault_exit(code as u32)))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = status;
        false
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
    /// Names the run this worker is about to serve, so it can be found by
    /// run (see [`Pool::kill_for_run`]).
    pub fn serve_run(&self, run_id: &str) {
        self.pool.set_run(self.id, run_id);
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

    fn receiver(&mut self) -> Box<dyn WorkerReceiver> {
        match self.process.as_ref() {
            Some(p) => p.receiver(),
            None => Box::new(crate::process::Ended),
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
        codemode_prelude_hash: basal_proto::PreludeHash([0; 32]),
        confinement: basal_proto::Confinement::None,
    })
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod bookkeeping_tests;

#[cfg(test)]
#[path = "acceptance_tests.rs"]
mod acceptance_tests;
