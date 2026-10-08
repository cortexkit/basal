//! The worker's top-level loop: handshake, then activations one at a time.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::rc::Rc;

use basal_proto::{
    Confinement, PROTOCOL_VERSION, ParentMessage, Refusal, Welcome, WorkerMessage, WorkerState,
};

use crate::engine::{ENGINE, prelude_hash, run_activation_owned};
use crate::link::{Channel, Received};

/// Why the loop ended. The binary turns this into its exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeExit {
    /// The parent sent `Shutdown` or closed the channel between frames.
    Done,
    /// The channel broke (an I/O error, a truncated frame, or an oversized
    /// frame after which the stream cannot be framed).
    Broken(String),
}

impl ServeExit {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Done => 0,
            Self::Broken(_) => 3,
        }
    }
}

/// Serves one parent until it shuts the worker down or the channel breaks.
pub fn serve<R: Read + 'static, W: Write + 'static>(
    reader: R,
    writer: W,
    confinement: Confinement,
) -> ServeExit {
    let channel = Rc::new(RefCell::new(Channel::new(reader, writer)));
    let mut state = WorkerState::AwaitingHello;
    loop {
        let received = channel.borrow_mut().receive();
        let message = match received {
            Received::Message(m) => m,
            Received::Closed => return ServeExit::Done,
            Received::Broken(detail) => return ServeExit::Broken(detail),
        };
        let reply = match (state, message) {
            (_, ParentMessage::Shutdown) => return ServeExit::Done,
            (WorkerState::AwaitingHello, ParentMessage::Hello { protocol_version }) => {
                if protocol_version == PROTOCOL_VERSION {
                    state = WorkerState::Idle;
                    WorkerMessage::Welcome(Welcome {
                        protocol_version: PROTOCOL_VERSION,
                        engine: ENGINE.into(),
                        prelude_hash: prelude_hash(),
                        confinement,
                    })
                } else {
                    WorkerMessage::Refused(Refusal::VersionMismatch {
                        supported: PROTOCOL_VERSION,
                        requested: protocol_version,
                    })
                }
            }
            (WorkerState::Idle, ParentMessage::Activate(request)) => {
                let activation_id = request.activation_id;
                let result = run_activation_owned(*request, channel.clone());
                if channel.borrow().shutdown_requested() {
                    return ServeExit::Done;
                }
                WorkerMessage::Finished {
                    activation_id,
                    result,
                }
            }
            (state, other) => WorkerMessage::Refused(Refusal::UnexpectedFrame {
                received: other.kind(),
                state,
            }),
        };
        let sent = channel.borrow_mut().send(&reply);
        if let Err(e) = sent {
            return ServeExit::Broken(e.0);
        }
        if channel.borrow().is_broken() {
            return ServeExit::Broken("channel broke during an activation".into());
        }
    }
}
