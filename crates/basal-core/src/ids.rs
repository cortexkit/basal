//! Hashes and identities: code hashes, idempotency keys, payload hashes and
//! the runtime fingerprint.

use basal_proto::{JsonText, PROTOCOL_VERSION, Settlement, Welcome};

use crate::schema::JOURNAL_FORMAT;

/// How argument digests are computed: BLAKE3 over the exact bytes the worker
/// sent, never over a re-serialised copy. Part of the runtime fingerprint, so
/// a change to the digest would stop old runs from replaying rather than make
/// every recorded call look divergent.
pub const ARGS_DIGEST_FORMAT: &str = "blake3-exact-bytes-v1";

/// Lowercase hex, as core writes a code hash.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// BLAKE3 of the exact script and manifest bytes. The script's length comes
/// first so that moving bytes between the two cannot produce the same hash.
pub fn code_hash(script: &str, manifest: &str) -> [u8; 32] {
    // prefrontal-core recomputes this hash when it creates the consent card
    // and stores it on its own install record, so the framing is a contract
    // between the two: a domain tag, then each part prefixed with its length
    // as a little-endian u64, so bytes cannot move from one part to the
    // other without changing the hash.
    let mut h = blake3::Hasher::new();
    h.update(CODE_HASH_TAG);
    h.update(&(script.len() as u64).to_le_bytes());
    h.update(script.as_bytes());
    h.update(&(manifest.len() as u64).to_le_bytes());
    h.update(manifest.as_bytes());
    *h.finalize().as_bytes()
}

/// The domain tag that opens every code hash, so a code hash can never equal
/// a BLAKE3 hash computed for any other purpose over the same bytes.
pub const CODE_HASH_TAG: &[u8] = b"basal-code-hash-v1\0";

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

    /// The same vectors are pinned in prefrontal-core, which recomputes the
    /// hash for the consent card; a change here must change there too.
    #[test]
    fn code_hash_matches_the_shared_vectors() {
        let hex = |h: [u8; 32]| h.iter().map(|b| format!("{b:02x}")).collect::<String>();
        for (script, manifest, expected) in [
            (
                "// package script bytes\n",
                r#"{"format":1,"id":"dark-wake","version":4,"purpose":"Nudge this agent when its session goes quiet.","trigger":{"schedule":{"interval":"15m"}},"sinks":[{"agent":"$self","digest_max":"wake"}],"status":["$self"],"facts":{"targets":["$self"]}}"#,
                "443a9c90ca55ba548bfb2fc5b496916ac46e96605be0b2be9469de40bd09743c",
            ),
            (
                "",
                "",
                "b5377f42c91b9631929ca590092769366de868dd1fc3c217d2abfc6f48382a78",
            ),
            (
                "a",
                "b",
                "cbca786f5bfade88b7953752a4144f71d49f585751311684635ed1169a956473",
            ),
            (
                "return 1;",
                "{\"id\":\"flow-x\",\"version\":1}",
                "7f9dcaf2bf757a09c91732726dc294f1178187e822c195c83ef66902ecb24841",
            ),
        ] {
            assert_eq!(
                hex(code_hash(script, manifest)),
                expected,
                "{script:?} {manifest:?}"
            );
        }
    }
}
