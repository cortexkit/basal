//! The worker's side of the channel to its parent.

use std::fmt;
use std::io::{Read, Write};

use basal_proto::{
    FrameError, HostCall, MAX_FRAME_BYTES, Outcome, ParentMessage, Refusal, WorkerMessage,
    WorkerState, read_parent_message, write_host_call, write_worker_message,
};

/// The channel to the parent broke; the activation cannot continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError(pub String);

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The parent's answer when the script is blocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitReply {
    Deliver(Outcome),
    LongRunning(Vec<u64>),
}

/// What an activation needs from its parent. The IPC channel implements it;
/// tests inside this crate implement it in memory.
///
/// Implementations validate replies before returning them: a delivery always
/// names a position the activation is waiting on and carries a delivery
/// order greater than `last_order`.
pub trait HostLink {
    /// Sends console output without allocating a host-call position.
    fn console(&mut self, _line: &str) -> Result<(), LinkError> {
        Err(LinkError("console output unsupported by this link".into()))
    }
    /// Sends a new asynchronous call. Its outcome comes back later through
    /// [`HostLink::wait`].
    fn issue(&mut self, call: &HostCall) -> Result<(), LinkError>;

    /// Sends a synchronous call and waits for its outcome.
    fn issue_sync(
        &mut self,
        call: &HostCall,
        last_order: Option<u64>,
    ) -> Result<Outcome, LinkError>;

    /// Reports that the script is blocked on `awaiting` and waits for the
    /// parent's one reply.
    fn wait(&mut self, awaiting: &[u64], last_order: Option<u64>) -> Result<WaitReply, LinkError>;
}

/// How reading the next frame went.
#[derive(Debug)]
pub enum Received {
    Message(ParentMessage),
    /// The parent closed the channel at a frame boundary.
    Closed,
    /// The channel is unusable (an I/O error, a truncated frame, or an
    /// oversized one that could not be skipped).
    Broken(String),
}

/// Frames over the worker's stdin and stdout.
pub struct Channel<R: Read, W: Write> {
    reader: R,
    writer: W,
    broken: bool,
    shutdown: bool,
}

impl<R: Read, W: Write> Channel<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            broken: false,
            shutdown: false,
        }
    }

    /// Whether the channel has failed and the worker should exit.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown
    }

    pub fn send(&mut self, message: &WorkerMessage) -> Result<(), LinkError> {
        if self.broken {
            return Err(LinkError("channel already broken".into()));
        }
        write_worker_message(&mut self.writer, message).map_err(|e| {
            self.broken = true;
            LinkError(format!("writing to parent: {e}"))
        })
    }

    pub fn refuse(&mut self, refusal: Refusal) -> Result<(), LinkError> {
        self.send(&WorkerMessage::Refused(refusal))
    }

    /// Reads the next well-formed message. Malformed frames are refused and
    /// skipped; an oversized frame is refused and breaks the channel, because
    /// its payload was never read and the stream can no longer be framed.
    pub fn receive(&mut self) -> Received {
        loop {
            if self.broken {
                return Received::Broken("channel already broken".into());
            }
            match read_parent_message(&mut self.reader) {
                Ok(message) => return Received::Message(message),
                Err(FrameError::Decode(e)) => {
                    if let Err(e) = self.refuse(Refusal::Malformed {
                        detail: e.to_string(),
                    }) {
                        return Received::Broken(e.0);
                    }
                }
                Err(FrameError::Oversized { declared, max }) => {
                    let _ = self.refuse(Refusal::Oversized { declared, max });
                    self.broken = true;
                    return Received::Broken(format!(
                        "oversized frame of {declared} bytes (maximum {MAX_FRAME_BYTES})"
                    ));
                }
                Err(FrameError::Closed) => {
                    self.broken = true;
                    return Received::Closed;
                }
                Err(e) => {
                    self.broken = true;
                    return Received::Broken(e.to_string());
                }
            }
        }
    }

    /// Reads messages until `accept` takes one, refusing every other frame.
    fn receive_expected<T>(
        &mut self,
        state: WorkerState,
        mut accept: impl FnMut(ParentMessage) -> Result<T, Refusal>,
    ) -> Result<T, LinkError> {
        loop {
            match self.receive() {
                Received::Message(ParentMessage::Shutdown) => {
                    self.shutdown = true;
                    return Err(LinkError("parent requested shutdown".into()));
                }
                Received::Message(message) => match accept(message) {
                    Ok(value) => return Ok(value),
                    Err(refusal) => {
                        // A refusal of an unexpected frame names the state
                        // the worker is in, so the parent can see why.
                        let refusal = match refusal {
                            Refusal::UnexpectedFrame { received, .. } => {
                                Refusal::UnexpectedFrame { received, state }
                            }
                            other => other,
                        };
                        self.refuse(refusal)?;
                    }
                },
                Received::Closed => return Err(LinkError("parent closed the channel".into())),
                Received::Broken(detail) => return Err(LinkError(detail)),
            }
        }
    }
}

fn check_order(last: Option<u64>, received: u64) -> Result<(), Refusal> {
    match last {
        Some(last) if received <= last => Err(Refusal::DeliveryOrderRegression { last, received }),
        _ => Ok(()),
    }
}

fn unexpected(message: &ParentMessage) -> Refusal {
    Refusal::UnexpectedFrame {
        received: message.kind(),
        state: WorkerState::Idle,
    }
}

impl<R: Read, W: Write> HostLink for Channel<R, W> {
    fn console(&mut self, line: &str) -> Result<(), LinkError> {
        self.send(&WorkerMessage::Console { line: line.into() })
    }
    fn issue(&mut self, call: &HostCall) -> Result<(), LinkError> {
        if self.broken {
            return Err(LinkError("channel already broken".into()));
        }
        write_host_call(&mut self.writer, call).map_err(|e| {
            self.broken = true;
            LinkError(format!("writing to parent: {e}"))
        })
    }

    fn issue_sync(
        &mut self,
        call: &HostCall,
        last_order: Option<u64>,
    ) -> Result<Outcome, LinkError> {
        self.issue(call)?;
        let position = call.position;
        self.receive_expected(WorkerState::AwaitingSyncReply, |message| match message {
            ParentMessage::Deliver(outcome) if outcome.position == position => {
                check_order(last_order, outcome.delivery_order)?;
                Ok(outcome)
            }
            ParentMessage::Deliver(outcome) => Err(Refusal::UnknownPosition {
                position: outcome.position,
            }),
            other => Err(unexpected(&other)),
        })
    }

    fn wait(&mut self, awaiting: &[u64], last_order: Option<u64>) -> Result<WaitReply, LinkError> {
        self.send(&WorkerMessage::Blocked {
            awaiting: awaiting.to_vec(),
        })?;
        self.receive_expected(WorkerState::Blocked, |message| match message {
            ParentMessage::Deliver(outcome) => {
                if !awaiting.contains(&outcome.position) {
                    return Err(Refusal::UnknownPosition {
                        position: outcome.position,
                    });
                }
                check_order(last_order, outcome.delivery_order)?;
                Ok(WaitReply::Deliver(outcome))
            }
            ParentMessage::LongRunning { positions } => {
                if positions.is_empty() {
                    return Err(Refusal::Malformed {
                        detail: "a long-running report must name at least one position".into(),
                    });
                }
                if let Some(&position) = positions.iter().find(|p| !awaiting.contains(p)) {
                    return Err(Refusal::UnknownPosition { position });
                }
                Ok(WaitReply::LongRunning(positions))
            }
            other => Err(unexpected(&other)),
        })
    }
}
