//! One `ck-basal-worker` child process, spoken to over its stdio frames.
//!
//! The worker receives an empty environment, three pipes and no arguments.
//! Rust's descriptors are close-on-exec; the worker also closes every inherited
//! descriptor above stdio and confines itself before reading its first frame. The
//! parent disclaims its macOS privacy identity before completing the handshake,
//! so untrusted scripts cannot borrow the module's privacy grants.

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use basal_core::channel::ChannelError;
use basal_proto::{
    FrameError, PROTOCOL_VERSION, ParentMessage, Welcome, WorkerMessage, encode_parent_frame,
    read_worker_message,
};
use subc_os::privacy_identity::DisclaimedCommand;

/// How to execute a worker. Test binaries do not implement the trampoline;
/// production uses `ck-basal` itself, before any runtime or threads start.
#[derive(Debug, Clone)]
pub enum WorkerLaunch {
    Disclaimed { trampoline: PathBuf },
    Plain,
}

/// Why a worker could not be started or greeted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// The binary could not be executed.
    Exec(String),
    /// Its own macOS privacy identity could not be confirmed.
    Disclaim(String),
    /// It started but did not complete the handshake.
    Handshake(String),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exec(e) => write!(f, "cannot start the worker: {e}"),
            Self::Disclaim(e) => write!(f, "cannot disclaim the worker's privacy identity: {e}"),
            Self::Handshake(e) => write!(f, "the worker did not complete its handshake: {e}"),
        }
    }
}

impl std::error::Error for SpawnError {}

type Incoming = Result<WorkerMessage, ChannelError>;

/// A started and greeted worker process. Killed on drop.
pub struct WorkerProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<Incoming>,
    welcome: Welcome,
}

impl WorkerProcess {
    /// Starts the worker and completes the handshake within `timeout`.
    ///
    /// The timeout is long on purpose: macOS evaluates an ad-hoc signed
    /// binary on its first launch, and again after a few idle minutes, and
    /// that evaluation has been measured stalling `exec` for over a minute.
    pub fn start(
        binary: &Path,
        launch: &WorkerLaunch,
        timeout: Duration,
    ) -> Result<Self, SpawnError> {
        let (mut command, confirmation) = match launch {
            WorkerLaunch::Disclaimed { trampoline } => {
                let mut builder = DisclaimedCommand::new(trampoline, binary);
                builder
                    .env_clear()
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let (command, confirmation) = builder
                    .into_command()
                    .map_err(|e| SpawnError::Disclaim(e.to_string()))?;
                (command, Some(confirmation))
            }
            WorkerLaunch::Plain => {
                let mut command = Command::new(binary);
                command
                    .env_clear()
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                (command, None)
            }
        };
        let deadline = Instant::now() + timeout;
        let mut child = command
            .spawn()
            .map_err(|e| SpawnError::Exec(format!("{}: {e}", binary.display())))?;
        // The command owns the parent's acknowledgement writer. Retaining it
        // would prevent EOF even after a successful trampoline exec.
        drop(command);
        if let Some(confirmation) = confirmation
            && let Err(error) = confirmation.confirm(deadline)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SpawnError::Disclaim(error.to_string()));
        }
        let stdin = child.stdin.take();
        let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SpawnError::Exec(
                "the worker's pipes were not created".into(),
            ));
        };
        let pid = child.id();

        // Frames are read on their own thread so a receive can time out.
        let (tx, incoming) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let message = match read_worker_message(&mut stdout) {
                    Ok(m) => Ok(m),
                    Err(FrameError::Closed) => {
                        let _ = tx.send(Err(ChannelError::Closed));
                        return;
                    }
                    Err(FrameError::Decode(e)) => Err(ChannelError::Broken(e.to_string())),
                    Err(e) => {
                        // The stream cannot be framed any further.
                        let _ = tx.send(Err(ChannelError::Broken(e.to_string())));
                        return;
                    }
                };
                if tx.send(message).is_err() {
                    return;
                }
            }
        });
        // The worker writes diagnostics to stderr; they go to the module's
        // log, a line at a time, so a worker cannot fill the pipe and stall.
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut pending = Vec::new();
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        pending.extend_from_slice(&buf[..n]);
                        while let Some(i) = pending.iter().position(|b| *b == b'\n') {
                            let line: Vec<u8> = pending.drain(..=i).collect();
                            tracing::debug!(
                                target: "worker",
                                pid,
                                "{}",
                                String::from_utf8_lossy(&line).trim_end()
                            );
                        }
                        // A line longer than this is logged in pieces.
                        if pending.len() > 16 * 1024 {
                            tracing::debug!(target: "worker", pid, "{}", String::from_utf8_lossy(&pending));
                            pending.clear();
                        }
                    }
                }
            }
        });

        // From here on, a failure drops `child`, which is killed by the
        // guard below rather than left running ungreeted.
        let mut guard = KillOnDrop(Some(child));
        let mut stdin = stdin;
        let hello = encode_parent_frame(&ParentMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        })
        .map_err(|e| SpawnError::Handshake(e.to_string()))?;
        stdin
            .as_mut()
            .ok_or_else(|| SpawnError::Handshake("no stdin pipe".into()))?
            .write_all(&hello)
            .map_err(|e| SpawnError::Handshake(e.to_string()))?;
        let welcome = match incoming.recv_timeout(timeout) {
            Ok(Ok(WorkerMessage::Welcome(w))) => w,
            Ok(Ok(other)) => {
                return Err(SpawnError::Handshake(format!(
                    "expected Welcome, got {}",
                    message_name(&other)
                )));
            }
            Ok(Err(e)) => return Err(SpawnError::Handshake(e.to_string())),
            Err(RecvTimeoutError::Timeout) => {
                return Err(SpawnError::Handshake(format!(
                    "no Welcome within {} s",
                    timeout.as_secs()
                )));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(SpawnError::Handshake("the worker closed its output".into()));
            }
        };
        let Some(child) = guard.0.take() else {
            return Err(SpawnError::Exec("the worker process was lost".into()));
        };
        Ok(Self {
            child,
            stdin,
            incoming,
            welcome,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    pub fn send(&mut self, message: &ParentMessage) -> Result<(), ChannelError> {
        let frame =
            encode_parent_frame(message).map_err(|e| ChannelError::Broken(e.to_string()))?;
        let stdin = self.stdin.as_mut().ok_or(ChannelError::Closed)?;
        stdin
            .write_all(&frame)
            .and_then(|_| stdin.flush())
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::BrokenPipe => ChannelError::Closed,
                _ => ChannelError::Broken(e.to_string()),
            })
    }

    pub fn recv(&mut self, timeout: Duration) -> Result<WorkerMessage, ChannelError> {
        match self.incoming.recv_timeout(timeout) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => Err(ChannelError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(ChannelError::Closed),
        }
    }

    /// Whether the process has exited (crashed, or was killed from outside).
    pub fn has_exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    pub fn kill(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Kills a child that never became a [`WorkerProcess`].
struct KillOnDrop(Option<Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn message_name(message: &WorkerMessage) -> &'static str {
    match message {
        WorkerMessage::Welcome(_) => "Welcome",
        WorkerMessage::HostCall(_) => "HostCall",
        WorkerMessage::Blocked { .. } => "Blocked",
        WorkerMessage::Finished { .. } => "Finished",
        WorkerMessage::Refused(_) => "Refused",
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.kill();
    }
}
