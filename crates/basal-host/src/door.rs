//! Engram's session door, as basal uses it to move an agent's flows.
//!
//! The door (`session-door`, an internal service of the `engram` module) is
//! the only path an agent's data takes between computers. Basal uses four
//! of its ops: `session.admit` hands it a stream's bytes, `session.cover`
//! checks they are recorded, `cut.read` lists a sealed cut's items, and
//! `session.read` reads an item's bytes back on the receiving computer.
//! Requests and replies are JSON; bytes travel as lowercase hex.

use std::sync::Arc;

use serde_json::Value;

use crate::transport::{Transport, WireError};

pub const MODULE: &str = "engram";
pub const SERVICE: &str = "session-door";

/// The most bytes one `session.admit` or `session.read` carries.
pub const CHUNK: usize = 1024 * 1024;

pub trait Door: Send + Sync {
    /// Sends one door op and returns its result.
    fn call(&self, op: &str, params: Value) -> Result<Value, WireError>;
}

/// The door over basal's daemon connection.
pub struct TransportDoor(pub Arc<dyn Transport>);

impl Door for TransportDoor {
    fn call(&self, op: &str, params: Value) -> Result<Value, WireError> {
        self.0.internal_service(MODULE, SERVICE, op, params)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeDoor;

#[cfg(any(test, feature = "test-support"))]
mod fake {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use super::Door;
    use crate::transport::WireError;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(text: &str) -> Option<Vec<u8>> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
            .collect()
    }

    fn refused(code: &str, detail: Option<Value>) -> WireError {
        match detail {
            Some(detail) => WireError::RefusedDetails {
                code: code.into(),
                message: code.into(),
                detail,
            },
            None => WireError::Refused {
                code: code.into(),
                message: code.into(),
            },
        }
    }

    #[derive(Default)]
    struct State {
        streams: BTreeMap<String, Vec<u8>>,
        /// Sealed cuts: their items as `cut.read` lists them.
        cuts: BTreeMap<String, Vec<Value>>,
        /// Ops refused with `unavailable` the next time each is called.
        fail_next: Vec<String>,
        calls: Vec<String>,
    }

    /// An in-memory door for tests. One fake can stand for the door on both
    /// computers: what one admits, the other reads once the cut is sealed
    /// with [`FakeDoor::seal`], as Entorhinal would.
    #[derive(Default)]
    pub struct FakeDoor {
        state: Mutex<State>,
    }

    impl FakeDoor {
        pub fn new() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self::default())
        }

        /// Seals `cut_id` over the given items, as `cut.seal` would.
        pub fn seal(&self, cut_id: &str, items: Vec<Value>) {
            self.state.lock().unwrap().cuts.insert(cut_id.into(), items);
        }

        /// The bytes admitted for `session_ref`.
        pub fn stream(&self, session_ref: &str) -> Vec<u8> {
            self.state
                .lock()
                .unwrap()
                .streams
                .get(session_ref)
                .cloned()
                .unwrap_or_default()
        }

        /// Replaces the bytes held for `session_ref`, to damage a stream.
        pub fn replace(&self, session_ref: &str, bytes: Vec<u8>) {
            self.state
                .lock()
                .unwrap()
                .streams
                .insert(session_ref.into(), bytes);
        }

        /// Refuses the next call of `op` with `unavailable`.
        pub fn fail_next(&self, op: &str) {
            self.state.lock().unwrap().fail_next.push(op.into());
        }

        /// The ops called so far, in order.
        pub fn calls(&self) -> Vec<String> {
            self.state.lock().unwrap().calls.clone()
        }
    }

    impl Door for FakeDoor {
        fn call(&self, op: &str, params: Value) -> Result<Value, WireError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(op.into());
            if let Some(at) = state.fail_next.iter().position(|o| o == op) {
                state.fail_next.remove(at);
                return Err(refused("unavailable", None));
            }
            let session = params["sessionRef"].as_str().unwrap_or_default().to_owned();
            match op {
                "session.admit" => {
                    let from = params["fromOffset"].as_u64().unwrap_or(0);
                    let bytes = unhex(params["bytes"].as_str().unwrap_or_default())
                        .ok_or_else(|| refused("invalid_request", None))?;
                    let stream = state.streams.entry(session).or_default();
                    if from != stream.len() as u64 {
                        return Err(refused(
                            "offset_mismatch",
                            Some(json!({"expectedOffset": stream.len()})),
                        ));
                    }
                    stream.extend(bytes);
                    Ok(json!({"durableOffset": stream.len()}))
                }
                "session.cover" => {
                    let end = state.streams.get(&session).map_or(0, Vec::len);
                    Ok(json!({"durableOffset": end, "cloudOffset": end}))
                }
                "cut.read" => {
                    let cut = params["cutId"].as_str().unwrap_or_default();
                    let items = state
                        .cuts
                        .get(cut)
                        .cloned()
                        .ok_or_else(|| refused("not_found", None))?;
                    Ok(json!({"items": items, "author": "fake"}))
                }
                "session.read" => {
                    let stream = state.streams.get(&session).cloned().unwrap_or_default();
                    let from = params["fromOffset"].as_u64().unwrap_or(0) as usize;
                    let limit = params["limit"].as_u64().unwrap_or(0) as usize;
                    let end = (from + limit).min(stream.len());
                    let digest = params["cutId"]
                        .as_str()
                        .and_then(|cut| state.cuts.get(cut))
                        .and_then(|items| {
                            items
                                .iter()
                                .find(|i| i["sessionRef"] == session.as_str())
                                .map(|i| i["digest"].clone())
                        })
                        .unwrap_or_else(|| json!(hex(&Sha256::digest(&stream))));
                    Ok(json!({
                        "fromOffset": from,
                        "endOffset": end,
                        "streamEnd": stream.len(),
                        "digest": digest,
                        "bytes": hex(stream.get(from..end).unwrap_or_default()),
                    }))
                }
                other => Err(refused("unknown_op", Some(json!({"op": other})))),
            }
        }
    }
}
