//! Engram's session door, as basal uses it to move an agent's flows.
//!
//! The door (`session-door`, an internal service of the `engram` module) is
//! the only path an agent's data takes between computers. Basal uses three
//! of its ops: `session.admit` hands it a stream's bytes, `session.cover`
//! checks they are recorded, and `session.read` reads an item's bytes back
//! with its sealed length and digest on the receiving computer.
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
        /// The items listed in each sealed cut.
        cuts: BTreeMap<String, Vec<Value>>,
        /// Op and code for one-shot refusals.
        refusals: Vec<(String, String)>,
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
            self.refuse_next(op, "unavailable");
        }

        /// Refuses the next call of `op` with `code`.
        pub fn refuse_next(&self, op: &str, code: &str) {
            self.state
                .lock()
                .unwrap()
                .refusals
                .push((op.into(), code.into()));
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
            // Calls through this stand-in come from Basal. Only Entorhinal
            // may list a cut; tests seal cuts directly on its behalf.
            if op == "cut.read" {
                return Err(refused("caller_forbidden", None));
            }
            if let Some(at) = state.refusals.iter().position(|(o, _)| o == op) {
                let (_, code) = state.refusals.remove(at);
                return Err(refused(&code, None));
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
                "session.read" => {
                    let bytes = state.streams.get(&session).map_or(&[][..], Vec::as_slice);
                    let (stream_end, digest) = if let Some(cut) = params["cutId"].as_str() {
                        let items = state
                            .cuts
                            .get(cut)
                            .ok_or_else(|| refused("cut_import_pending", None))?;
                        let stream = params["stream"]
                            .as_str()
                            .ok_or_else(|| refused("invalid_request", None))?;
                        let item = items
                            .iter()
                            .find(|i| i["sessionRef"] == session.as_str() && i["stream"] == stream)
                            .ok_or_else(|| refused("cut_item_missing", None))?;
                        let end = item["endOffset"]
                            .as_u64()
                            .and_then(|n| usize::try_from(n).ok())
                            .ok_or_else(|| refused("verify_failed", None))?;
                        (end, item["digest"].clone())
                    } else {
                        (bytes.len(), json!(hex(&Sha256::digest(bytes))))
                    };
                    let from = params["fromOffset"].as_u64().unwrap_or(0) as usize;
                    let limit = params["limit"].as_u64().unwrap_or(0) as usize;
                    let end = from.saturating_add(limit).min(stream_end);
                    let page = bytes
                        .get(from..end)
                        .ok_or_else(|| refused("cut_range_not_local", None))?;
                    Ok(json!({
                        "fromOffset": from,
                        "endOffset": end,
                        "streamEnd": stream_end,
                        "digest": digest,
                        "bytes": hex(page),
                    }))
                }
                other => Err(refused("unknown_op", Some(json!({"op": other})))),
            }
        }
    }
}
