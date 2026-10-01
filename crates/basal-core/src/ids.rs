//! Hashes and identities: code hashes, idempotency keys, payload hashes and
//! the runtime fingerprint.

use basal_proto::{JsonText, PROTOCOL_VERSION, Settlement, Welcome};

use crate::schema::JOURNAL_FORMAT;

/// How argument digests are computed: BLAKE3 over the exact bytes the worker
/// sent, never over a re-serialised copy. Part of the runtime fingerprint, so
/// a change to the digest would stop old runs from replaying rather than make
/// every recorded call look divergent.
pub const ARGS_DIGEST_FORMAT: &str = "blake3-exact-bytes-v1";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// BLAKE3 of the exact script and manifest bytes. The script's length comes
/// first so that moving bytes between the two cannot produce the same hash.
pub fn code_hash(script: &str, manifest: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&(script.len() as u64).to_be_bytes());
    h.update(script.as_bytes());
    h.update(manifest.as_bytes());
    *h.finalize().as_bytes()
}

/// The idempotency key sent with a call: derived from (flow id, run id,
/// position) only, so every send of the same call carries the same key, and
/// no other call ever does (run ids are never reused).
pub fn idempotency_key(flow_id: &str, run_id: &str, position: u64) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"basal/idempotency/v1\0");
    h.update(&(flow_id.len() as u64).to_be_bytes());
    h.update(flow_id.as_bytes());
    h.update(&(run_id.len() as u64).to_be_bytes());
    h.update(run_id.as_bytes());
    h.update(&position.to_be_bytes());
    format!("bk1-{}", &h.finalize().to_hex()[..40])
}

/// Identifies an outcome's content, so a redelivered completion can be told
/// apart from a contradictory one.
pub fn payload_hash(settlement: Settlement, value: &JsonText) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(match settlement {
        Settlement::Fulfilled => b"F",
        Settlement::Rejected => b"R",
    });
    h.update(value.as_str().as_bytes());
    *h.finalize().as_bytes()
}

/// Everything that decides how a recorded run replays besides its code: the
/// engine and prelude the worker reported in its handshake, and the formats
/// of the parent-worker bridge (its IPC protocol version), the argument
/// digests and the journal. A run is replayed only under the fingerprint it
/// was recorded with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub engine: String,
    pub prelude_hash: [u8; 32],
    pub protocol_version: u32,
    pub args_digest: &'static str,
    pub journal_format: u32,
}

impl Fingerprint {
    pub fn of_worker(welcome: &Welcome) -> Self {
        Self {
            engine: welcome.engine.clone(),
            prelude_hash: welcome.prelude_hash.0,
            protocol_version: welcome.protocol_version.min(PROTOCOL_VERSION),
            args_digest: ARGS_DIGEST_FORMAT,
            journal_format: JOURNAL_FORMAT,
        }
    }

    /// The stored form. Compared as a whole string, so any field differing
    /// is a mismatch.
    pub fn canonical(&self) -> String {
        format!(
            "engine={};prelude={};protocol={};args_digest={};journal={}",
            self.engine,
            hex(&self.prelude_hash),
            self.protocol_version,
            self.args_digest,
            self.journal_format
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_differ_by_every_input_and_are_stable() {
        let k = idempotency_key("f", "r", 1);
        assert_eq!(k, idempotency_key("f", "r", 1));
        assert_ne!(k, idempotency_key("f", "r", 2));
        assert_ne!(k, idempotency_key("f", "r2", 1));
        assert_ne!(k, idempotency_key("g", "r", 1));
        // Length prefixes keep ("ab", "c") apart from ("a", "bc").
        assert_ne!(idempotency_key("ab", "c", 0), idempotency_key("a", "bc", 0));
    }

    #[test]
    fn code_hash_separates_script_from_manifest() {
        assert_ne!(code_hash("ab", "c"), code_hash("a", "bc"));
    }
}
