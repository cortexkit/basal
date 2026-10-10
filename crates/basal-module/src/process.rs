//! One `ck-basal-worker` child process, spoken to over its stdio frames.
//!
//! The worker receives an empty environment and three pipes. On Linux its
//! sole argument selects whether Landlock is required or optional.
//! Rust's descriptors are close-on-exec; the worker also closes every inherited
//! descriptor above stdio and confines itself before reading its first frame.
//!
//! **Disclaimed launch.** On macOS, a child normally shares its parent's
//! "responsible process", the identity privacy (TCC) grants such as Files &
//! Folders are checked against. So a worker launched plainly could use any
//! grant `ck-basal` holds. Production instead launches each worker
//! *disclaimed*: it becomes its own responsible process and holds no grants.
//! `subc-os` does this through a **trampoline**: `ck-basal` re-executes itself
//! in a hidden mode, single-threaded, which sets the disclaim attribute and then
//! replaces itself with the worker (keeping the same pid and pipes). The parent
//! waits for the trampoline's confirmation before the handshake, and a failed
//! confirmation kills the child rather than falling back to a plain launch.

use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use basal_core::channel::{ChannelError, WorkerReceiver};
use basal_proto::{
    FrameError, PROTOCOL_VERSION, ParentMessage, Welcome, WorkerMessage, encode_parent_frame,
    read_worker_message,
};
#[cfg(target_os = "macos")]
use subc_os::privacy_identity::DisclaimedCommand;

/// Linux workers require Landlock unless the caller explicitly permits a
/// seccomp-only worker on kernels where Landlock is unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LandlockPolicy {
    #[default]
    Required,
    Optional,
}

impl LandlockPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
        }
    }
}

/// How to execute a worker. `Disclaimed` runs it through a trampoline
/// executable (see the module comment); only `ck-basal` implements that hidden
/// mode, handling it first thing in `main`, so test binaries use `Plain`.
#[derive(Debug, Clone)]
pub enum WorkerLaunch {
    #[cfg(target_os = "macos")]
    Disclaimed {
        trampoline: PathBuf,
    },
    Plain,
}

/// Why a worker could not be started or greeted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// The binary could not be executed.
    Exec(String),
    /// The trampoline did not confirm that the worker was launched as its own
    /// macOS responsible process.
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

trait FrameSender: Send {
    fn send(&self, message: Incoming) -> Result<(), ()>;
    #[cfg(test)]
    fn try_send(&self, message: Incoming) -> Result<(), BufferError>;
}

#[cfg(test)]
#[derive(Debug)]
enum BufferError {
    Full,
    Disconnected,
}

impl FrameSender for mpsc::Sender<Incoming> {
    fn send(&self, message: Incoming) -> Result<(), ()> {
        self.send(message).map_err(|_| ())
    }
    #[cfg(test)]
    fn try_send(&self, message: Incoming) -> Result<(), BufferError> {
        self.send(message).map_err(|_| BufferError::Disconnected)
    }
}

impl FrameSender for mpsc::SyncSender<Incoming> {
    fn send(&self, message: Incoming) -> Result<(), ()> {
        self.send(message).map_err(|_| ())
    }
    #[cfg(test)]
    fn try_send(&self, message: Incoming) -> Result<(), BufferError> {
        self.try_send(message).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => BufferError::Full,
            mpsc::TrySendError::Disconnected(_) => BufferError::Disconnected,
        })
    }
}

fn frame_channel() -> (impl FrameSender, Receiver<Incoming>) {
    // Two queued frames plus the one being decoded bound read-ahead even when
    // a worker floods stdout while its activation is not receiving.
    mpsc::sync_channel(2)
}

fn recv_handshake(
    incoming: &Receiver<Incoming>,
    deadline: Instant,
) -> Result<Incoming, RecvTimeoutError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(RecvTimeoutError::Timeout)?;
    incoming.recv_timeout(remaining)
}

/// A started and greeted worker process. Killed on drop.
pub struct WorkerProcess {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    /// Shared so that a receiver split off for another thread reads the same
    /// frames (see [`WorkerProcess::receiver`]).
    incoming: Arc<Mutex<Receiver<Incoming>>>,
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
        Self::start_with_policy(binary, launch, timeout, LandlockPolicy::Required)
    }

    /// Starts a worker with the caller's explicit Linux Landlock policy.
    pub fn start_with_policy(
        binary: &Path,
        launch: &WorkerLaunch,
        timeout: Duration,
        landlock: LandlockPolicy,
    ) -> Result<Self, SpawnError> {
        Self::start_for_profile(binary, launch, timeout, landlock, false)
    }

    pub(crate) fn start_for_profile(
        binary: &Path,
        launch: &WorkerLaunch,
        timeout: Duration,
        landlock: LandlockPolicy,
        codemode: bool,
    ) -> Result<Self, SpawnError> {
        #[cfg(target_os = "macos")]
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
        #[cfg(not(target_os = "macos"))]
        let mut command = match launch {
            WorkerLaunch::Plain => {
                let mut command = Command::new(binary);
                command
                    .env_clear()
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                command
            }
        };
        #[cfg(target_os = "linux")]
        command.arg(format!("--landlock={}", landlock.as_str()));
        #[cfg(not(target_os = "linux"))]
        let _ = landlock;
        codemode_address_space(&mut command, codemode);
        let deadline = Instant::now() + timeout;
        let mut child = command
            .spawn()
            .map_err(|e| SpawnError::Exec(format!("{}: {e}", binary.display())))?;
        // The command holds this process's copy of the trampoline's confirmation
        // pipe. The confirmation reads that pipe to end-of-file, which never comes
        // while a copy stays open here, so drop it before confirming.
        drop(command);
        #[cfg(target_os = "macos")]
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
        let (tx, incoming) = frame_channel();
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
        let welcome = match recv_handshake(&incoming, deadline) {
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
            child: Arc::new(Mutex::new(child)),
            stdin,
            incoming: Arc::new(Mutex::new(incoming)),
            welcome,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.lock().unwrap_or_else(|p| p.into_inner()).id()
    }

    pub(crate) fn killer(&self) -> WorkerKiller {
        WorkerKiller(self.child.clone())
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
        let incoming = self.incoming.lock().unwrap_or_else(|p| p.into_inner());
        match incoming.recv_timeout(timeout) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => Err(ChannelError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(ChannelError::Closed),
        }
    }

    /// A receive side another thread can block on while this process keeps
    /// sending. A kill closes the worker's stdout, and the frame reader then
    /// reports `Closed`, which ends that thread's wait.
    pub fn receiver(&self) -> Box<dyn WorkerReceiver> {
        Box::new(SharedIncoming(self.incoming.clone()))
    }

    /// Whether the process has exited (crashed, or was killed from outside).
    pub fn has_exited(&mut self) -> bool {
        self.exit_status().is_some()
    }

    /// An observed exit, retaining its status so the pool can distinguish a
    /// confinement violation from other deaths. An observation error also
    /// retires the worker: its liveness can no longer be established.
    pub fn exit_status(&mut self) -> Option<std::io::Result<ExitStatus>> {
        self.child
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_wait()
            .transpose()
    }

    pub fn kill(&mut self) {
        self.stdin = None;
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct SharedIncoming(Arc<Mutex<Receiver<Incoming>>>);

impl WorkerReceiver for SharedIncoming {
    fn recv(&mut self) -> Result<WorkerMessage, ChannelError> {
        let incoming = self.0.lock().unwrap_or_else(|p| p.into_inner());
        incoming.recv().unwrap_or(Err(ChannelError::Closed))
    }
}

/// The receiver of a lease whose process is already gone.
pub(crate) struct Ended;

impl WorkerReceiver for Ended {
    fn recv(&mut self) -> Result<WorkerMessage, ChannelError> {
        Err(ChannelError::Closed)
    }
}

fn codemode_address_space(command: &mut Command, codemode: bool) {
    #[cfg(target_os = "linux")]
    if codemode {
        use std::os::unix::process::CommandExt;
        // Set the child's address-space cap before exec: the Linux sandbox
        // later denies changing process limits. Flow workers keep the parent's limits.
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: basal_proto::CODEMODE_ADDRESS_SPACE_BYTES as libc::rlim_t,
                    rlim_max: basal_proto::CODEMODE_ADDRESS_SPACE_BYTES as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (command, codemode);
}

#[cfg(all(test, target_os = "macos"))]
mod codemode_limits {
    use super::*;

    #[test]
    fn macos_codemode_leaves_address_space_limit_inherited() {
        fn limit(codemode: bool) -> Vec<u8> {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "ulimit -v"]);
            codemode_address_space(&mut command, codemode);
            let output = command.output().unwrap();
            assert!(output.status.success());
            output.stdout
        }
        // macOS workers inherit the same limit as ordinary launches. The
        // Linux-only cap must not leak into the privacy-identity trampoline.
        assert_eq!(limit(true), limit(false));
    }
}

#[derive(Clone)]
pub(crate) struct WorkerKiller(Arc<Mutex<Child>>);

impl WorkerKiller {
    fn signal_with(&self, signal: impl FnOnce(u32) -> bool) -> bool {
        let mut child = self.0.lock().unwrap_or_else(|p| p.into_inner());
        // A cached exit status means the pid may already belong to somebody
        // else. Exclude every reaper until after the signal is issued.
        if !matches!(child.try_wait(), Ok(None)) {
            return false;
        }
        signal(child.id())
    }

    pub(crate) fn kill(&self) -> bool {
        self.signal_with(|pid| {
            let Ok(pid) = libc::pid_t::try_from(pid) else {
                return false;
            };
            // SAFETY: the child mutex excludes reaping while its pid is signalled.
            unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
        })
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
        WorkerMessage::Console { .. } => "Console",
        WorkerMessage::Warning { .. } => "Warning",
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod buffer_tests {
    use super::*;

    #[test]
    fn incoming_frames_have_bounded_capacity() {
        let (tx, incoming) = frame_channel();
        tx.try_send(Err(ChannelError::Closed)).unwrap();
        tx.try_send(Err(ChannelError::Closed)).unwrap();
        assert!(matches!(
            tx.try_send(Err(ChannelError::Closed)),
            Err(BufferError::Full)
        ));
        assert!(matches!(
            incoming.recv().unwrap(),
            Err(ChannelError::Closed)
        ));
        tx.try_send(Err(ChannelError::Closed)).unwrap();
        drop(incoming);
        assert!(tx.send(Err(ChannelError::Closed)).is_err());
    }

    #[test]
    fn a_reaped_worker_handle_never_signals_a_recycled_pid() {
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let killer = WorkerKiller(Arc::new(Mutex::new(child)));
        let mut signalled = false;
        killer.signal_with(|_| {
            signalled = true;
            true
        });
        assert!(!signalled, "a reaped pid no longer identifies the worker");
    }

    #[test]
    fn expired_launch_budget_cannot_accept_a_queued_welcome() {
        let (tx, incoming) = mpsc::channel();
        tx.send(Err(ChannelError::Closed)).unwrap();
        assert!(matches!(
            recv_handshake(&incoming, Instant::now()),
            Err(RecvTimeoutError::Timeout)
        ));
    }
}
