//! Worker channels over real `ck-basal-worker` processes, for driving
//! basal-core's activation driver in tests and in the test parent.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use basal_core::channel::{ChannelError, WorkerChannel, WorkerSource};
use basal_proto::{MessageKind, ParentMessage, Welcome, WorkerMessage};

use crate::process::{ParentError, WorkerProcess};

/// Counters shared by every channel a source hands out.
#[derive(Debug, Default)]
pub struct FrameCounts {
    pub spawned: AtomicUsize,
    pub activate: AtomicUsize,
    pub deliver: AtomicUsize,
    pub long_running: AtomicUsize,
    /// The pid of the most recently spawned worker, while it is alive and
    /// owned by a channel (0 once the channel killed or dropped it).
    pub live_pid: AtomicU32,
}

/// A greeted worker process as a [`WorkerChannel`].
pub struct ProcessChannel {
    process: WorkerProcess,
    welcome: Welcome,
    counts: Arc<FrameCounts>,
}

impl ProcessChannel {
    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    /// Whether the worker process has exited within `timeout`.
    pub fn exited(&mut self, timeout: Duration) -> bool {
        self.process.wait_exit(timeout).is_some()
    }
}

fn channel_error(e: ParentError) -> ChannelError {
    match e {
        ParentError::Timeout => ChannelError::Timeout,
        ParentError::Closed => ChannelError::Closed,
        other => ChannelError::Broken(other.to_string()),
    }
}

impl WorkerChannel for ProcessChannel {
    fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    fn send(&mut self, message: &ParentMessage) -> Result<(), ChannelError> {
        let counter = match message.kind() {
            MessageKind::Activate => Some(&self.counts.activate),
            MessageKind::Deliver => Some(&self.counts.deliver),
            MessageKind::LongRunning => Some(&self.counts.long_running),
            _ => None,
        };
        self.process.send(message).map_err(channel_error)?;
        if let Some(c) = counter {
            c.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<WorkerMessage, ChannelError> {
        self.process.recv(timeout).map_err(channel_error)
    }

    fn kill(&mut self) {
        let _ = self.counts.live_pid.compare_exchange(
            self.process.pid(),
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        self.process.kill();
    }
}

impl Drop for ProcessChannel {
    fn drop(&mut self) {
        let _ = self.counts.live_pid.compare_exchange(
            self.process.pid(),
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

/// Spawns and greets a fresh worker process for every activation.
pub struct ProcessSource {
    binary: PathBuf,
    pub counts: Arc<FrameCounts>,
    /// Replaces the engine string the worker reports, to stand in for a
    /// worker built with a different engine.
    pub engine_override: Option<String>,
}

impl ProcessSource {
    pub fn new(binary: impl AsRef<Path>) -> Self {
        Self {
            binary: binary.as_ref().to_path_buf(),
            counts: Arc::new(FrameCounts::default()),
            engine_override: None,
        }
    }

    pub fn spawn(&self) -> Result<ProcessChannel, ChannelError> {
        let (process, mut welcome) =
            WorkerProcess::start(&self.binary, Duration::from_secs(120)).map_err(channel_error)?;
        if let Some(engine) = &self.engine_override {
            welcome.engine = engine.clone();
        }
        self.counts.spawned.fetch_add(1, Ordering::SeqCst);
        self.counts.live_pid.store(process.pid(), Ordering::SeqCst);
        Ok(ProcessChannel {
            process,
            welcome,
            counts: self.counts.clone(),
        })
    }
}

impl WorkerSource for ProcessSource {
    fn worker(&self) -> Result<Box<dyn WorkerChannel>, ChannelError> {
        Ok(Box::new(self.spawn()?))
    }
}

/// The worker binary for tests: `BASAL_WORKER_BIN` if set, otherwise the
/// workspace's own debug build, built once per test process if needed.
pub fn worker_binary() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        if let Some(p) = std::env::var_os("BASAL_WORKER_BIN") {
            return PathBuf::from(p);
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(&root)
            .args(["build", "-p", "basal-worker", "--bin", "ck-basal-worker"])
            .status();
        if !status.as_ref().is_ok_and(|s| s.success()) {
            eprintln!("building ck-basal-worker failed: {status:?}");
        }
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target"));
        target.join("debug").join("ck-basal-worker")
    })
    .clone()
}
