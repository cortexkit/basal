//! A real `ck-basal-worker` child process, driven over its stdio frames.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use basal_proto::{
    FrameError, PROTOCOL_VERSION, ParentMessage, Welcome, WorkerMessage, encode_parent_frame,
    read_worker_message,
};

/// What went wrong talking to the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentError {
    /// No frame arrived in time.
    Timeout,
    /// The worker closed its stdout (usually because it exited).
    Closed,
    /// The worker sent bytes that are not a valid worker message.
    Malformed(String),
    Io(String),
    /// A valid message arrived where the protocol does not allow it.
    Protocol(String),
}

impl fmt::Display for ParentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "timed out waiting for the worker"),
            Self::Closed => write!(f, "the worker closed its output"),
            Self::Malformed(e) => write!(f, "malformed frame from the worker: {e}"),
            Self::Io(e) => write!(f, "i/o error: {e}"),
            Self::Protocol(e) => write!(f, "protocol violation: {e}"),
        }
    }
}

impl std::error::Error for ParentError {}

type Incoming = Result<WorkerMessage, ParentError>;

/// A spawned worker. Killed on drop.
pub struct WorkerProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<Incoming>,
    stderr: Arc<Mutex<Vec<u8>>>,
    pub spawned_at: Instant,
}

impl WorkerProcess {
    /// Spawns the worker with an empty environment and only stdio, the way
    /// basal's parent does: the worker inherits nothing else.
    pub fn spawn(binary: &Path) -> io::Result<Self> {
        Self::spawn_with_args(binary, &[])
    }

    pub fn spawn_with_args(binary: &Path, args: &[&str]) -> io::Result<Self> {
        let spawned_at = Instant::now();
        let mut child = Command::new(binary)
            .args(args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take();
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("no stdout pipe"))?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("no stderr pipe"))?;

        let (tx, incoming) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let message = match read_worker_message(&mut stdout) {
                    Ok(m) => Ok(m),
                    Err(FrameError::Closed) => {
                        let _ = tx.send(Err(ParentError::Closed));
                        return;
                    }
                    Err(FrameError::Decode(e)) => Err(ParentError::Malformed(e.to_string())),
                    Err(e) => {
                        let _ = tx.send(Err(ParentError::Malformed(e.to_string())));
                        return;
                    }
                };
                if tx.send(message).is_err() {
                    return;
                }
            }
        });

        let stderr = Arc::new(Mutex::new(Vec::new()));
        let sink = stderr.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stderr_pipe.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if let Ok(mut all) = sink.lock() {
                            all.extend_from_slice(&buf[..n]);
                        }
                    }
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            incoming,
            stderr,
            spawned_at,
        })
    }

    /// Spawns and completes the handshake.
    pub fn start(binary: &Path, timeout: Duration) -> Result<(Self, Welcome), ParentError> {
        let mut worker = Self::spawn(binary).map_err(|e| ParentError::Io(e.to_string()))?;
        let welcome = worker.handshake(timeout)?;
        Ok((worker, welcome))
    }

    pub fn handshake(&mut self, timeout: Duration) -> Result<Welcome, ParentError> {
        self.send(&ParentMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        })?;
        match self.recv(timeout)? {
            WorkerMessage::Welcome(w) => Ok(w),
            other => Err(ParentError::Protocol(format!(
                "expected Welcome, got {other:?}"
            ))),
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn send(&mut self, message: &ParentMessage) -> Result<(), ParentError> {
        let frame = encode_parent_frame(message).map_err(|e| ParentError::Io(e.to_string()))?;
        self.send_raw(&frame)
    }

    /// Writes bytes as they are, for tests that send malformed frames.
    pub fn send_raw(&mut self, bytes: &[u8]) -> Result<(), ParentError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| ParentError::Io("stdin already closed".into()))?;
        stdin
            .write_all(bytes)
            .and_then(|_| stdin.flush())
            .map_err(|e| ParentError::Io(e.to_string()))
    }

    /// Closes the worker's stdin, which it reads as the end of the channel.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    pub fn recv(&self, timeout: Duration) -> Result<WorkerMessage, ParentError> {
        match self.incoming.recv_timeout(timeout) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => Err(ParentError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(ParentError::Closed),
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Waits for the worker to exit on its own.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                _ => return None,
            }
        }
    }

    pub fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    /// Resident set size in KiB, read with `ps`.
    pub fn rss_kib(&self) -> Option<u64> {
        let out = Command::new("ps")
            .args(["-o", "rss=", "-p", &self.child.id().to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.kill();
    }
}
