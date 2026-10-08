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
    pub activate: AtomicUsize,
    pub deliver: AtomicUsize,
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
    /// Bound a test's worker startup separately from its activation budget.
    pub handshake_timeout: Duration,
    pub counts: Arc<FrameCounts>,
    /// Replaces the engine string the worker reports, to stand in for a
    /// worker built with a different engine.
    pub engine_override: Option<String>,
}

impl ProcessSource {
    pub fn new(binary: impl AsRef<Path>) -> Self {
        Self {
            binary: binary.as_ref().to_path_buf(),
            handshake_timeout: Duration::from_secs(60),
            counts: Arc::new(FrameCounts::default()),
            engine_override: None,
        }
    }

    pub fn spawn(&self) -> Result<ProcessChannel, ChannelError> {
        let (process, mut welcome) =
            WorkerProcess::start(&self.binary, self.handshake_timeout).map_err(channel_error)?;
        if let Some(engine) = &self.engine_override {
            welcome.engine = engine.clone();
        }
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

/// A development-named copy of `BASAL_WORKER_BIN` if set, otherwise the
/// workspace's own debug build, built once per test process if needed.
pub fn worker_binary() -> PathBuf {
    static PATH: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    PATH.get_or_init(|| {
        if let Some(p) = std::env::var_os("BASAL_WORKER_BIN") {
            return Ok(crate::dev_binary(PathBuf::from(p)));
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        // Keep build chatter out of libtest's status lines, which mutation
        // reports parse. A failed build must never select a stale worker.
        let output = Command::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "basal-worker",
                "--bin",
                "ck-basal-worker",
                "--message-format=json",
            ])
            .output()
            .map_err(|e| format!("launch Cargo to build ck-basal-worker: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "building ck-basal-worker failed: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        // Cargo resolves relative target directories and profile layouts. Its
        // artifact message also prevents a successful build selecting old bytes.
        let binary = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find_map(|v| {
                (v["reason"] == "compiler-artifact" && v["target"]["name"] == "ck-basal-worker")
                    .then(|| v["executable"].as_str().map(PathBuf::from))
                    .flatten()
            })
            .ok_or("Cargo did not report a ck-basal-worker executable")?;
        Ok(crate::dev_binary(binary))
    })
    .as_ref()
    .unwrap_or_else(|e| panic!("{e}"))
    .clone()
}
