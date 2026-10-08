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
}

impl WorkerProcess {
    /// Spawns the worker with an empty environment and only stdio, the way
    /// basal's parent does: the worker inherits nothing else.
    pub fn spawn(binary: &Path) -> io::Result<Self> {
        Self::spawn_with_args(binary, &[])
    }

    pub fn spawn_with_args(binary: &Path, args: &[&str]) -> io::Result<Self> {
        let binary = crate::dev_binary(binary);
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
        // Retain Child ownership so a timed-out wait can still kill and reap it.
        // std::process has no timed wait that leaves the Child available.
        let mut backoff = crate::backoff::Backoff::new();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => backoff.sleep(deadline),
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

    /// Resident set size in KiB, sampled without starting a child on macOS.
    pub fn rss_kib(&self) -> Option<u64> {
        rss_kib(self.child.id())
    }
}

/// Read process memory directly so sampling does not add fork/exec load to
/// the activation whose memory is being measured.
pub fn rss_kib(pid: u32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_taskinfo>();
        // SAFETY: the buffer is exactly the size required by PROC_PIDTASKINFO;
        // it is read only if the kernel reports the full initialized struct.
        let read = unsafe {
            libc::proc_pidinfo(
                pid.try_into().ok()?,
                libc::PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast(),
                size.try_into().ok()?,
            )
        };
        if usize::try_from(read).ok()? != size {
            return None;
        }
        Some(unsafe { info.assume_init() }.pti_resident_size / 1024)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let out = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
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

/// Probe the operating system without waiting for an exit. A successful
/// process-exit wait can hide a missing reap in the cleanup path.
pub fn assert_reaped(pid: u32) {
    let mut status = 0;
    // SAFETY: waitpid writes only to this status and the caller's child pid.
    let result = unsafe {
        libc::waitpid(
            pid.try_into().expect("child pid"),
            &mut status,
            libc::WNOHANG,
        )
    };
    assert_eq!(result, -1, "worker {pid} was not reaped");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

/// Capture a subprocess with a deadline, draining both pipes concurrently.
/// Timeout kills the process group, so inherited pipes cannot keep a reader
/// alive after the parent has been reaped.
pub fn output_until(command: &mut Command, timeout: Duration) -> io::Result<std::process::Output> {
    use std::os::unix::process::CommandExt;
    // SAFETY: setpgid is async-signal-safe and touches no Rust-owned memory.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            result => {
                timed_out = result.is_ok();
                // SAFETY: the process group was created for this unreaped child.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                break child.wait();
            }
        }
    };
    let stdout = out
        .join()
        .map_err(|_| io::Error::other("stdout reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| io::Error::other("stderr reader panicked"))??;
    if timed_out {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "subprocess deadline expired; process group killed and reaped",
        ));
    }
    Ok(std::process::Output {
        status: status?,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subprocess_deadline_reaps_descendants_holding_output_pipes() {
        let error = output_until(
            Command::new("/bin/sh").args(["-c", "sleep 600 & wait"]),
            Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
