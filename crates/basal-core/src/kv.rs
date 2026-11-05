//! A flow's own durable state: `kv.get`, `kv.set` and `kv.delete`.
//!
//! The runtime serves these itself. Each call's journal row, its effect and
//! its outcome commit in one transaction, so a crash leaves all three or
//! none, and recovery never applies a committed write twice. Writes are
//! eager: they stay even if the run later fails or stops in
//! `needs_reconcile`, so a flow that posted something and recorded it does
//! not lose the record. Keys belong to the flow id, so every version of a
//! flow sees the same state.
//!
//! Every limit is checked before the write and refused as a journaled
//! rejection with code `kv_limit`, which the script can catch.

use basal_host::HostOutcome;
use basal_proto::{CallKind, JsonText, Primitive};

/// Size limits on a flow's `kv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvLimits {
    /// The longest key, in UTF-8 bytes.
    pub max_key_bytes: usize,
    /// The largest value, as JSON text.
    pub max_value_bytes: usize,
    /// The most bytes (keys plus values) one flow may hold.
    pub max_flow_bytes: usize,
}

impl Default for KvLimits {
    fn default() -> Self {
        Self {
            max_key_bytes: 256,
            max_value_bytes: 64 * 1024,
            max_flow_bytes: 1024 * 1024,
        }
    }
}

/// Whether the runtime serves `kind` itself as a local effect.
pub fn is_kv(kind: &CallKind) -> bool {
    matches!(
        kind,
        CallKind::Primitive(Primitive::KvGet | Primitive::KvSet | Primitive::KvDelete)
    )
}

/// A rejection outcome with a code the script can branch on.
pub fn rejection(code: &str, message: &str) -> HostOutcome {
    HostOutcome::rejected(
        basal_host::rejection_text(code, message).unwrap_or_else(|_| JsonText::null()),
    )
}
