use basal_proto::{ArgsDigest, CallKind, HostCall, JsonText, Settlement};
use serde::{Deserialize, Serialize};

use super::{State, codec};
use crate::{model::StoredClass, run_log::Lsn};

/// Retains the call kind and exact argument digest used to check replay,
/// including calls served locally or refused before authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallIdentity {
    pub position: u64,
    #[serde(with = "codec::kind")]
    pub kind: CallKind,
    #[serde(with = "codec::class")]
    pub class: StoredClass,
    pub op: String,
    #[serde(with = "codec::json")]
    pub args: JsonText,
    #[serde(with = "codec::digest")]
    pub args_digest: ArgsDigest,
}

impl CallIdentity {
    pub fn new(call: HostCall, class: StoredClass) -> Self {
        let op = match &call.kind {
            CallKind::Op { op, .. } | CallKind::OpFor { op, .. } => op.clone(),
            CallKind::Primitive(p) => p.name().to_owned(),
            CallKind::Tool { name } => name.clone(),
        };
        Self {
            position: call.position,
            args_digest: ArgsDigest::of(&call.args),
            kind: call.kind,
            class,
            op,
            args: call.args,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchCount {
    pub window_start: i64,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micros: u64,
    pub window_start: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub call: CallIdentity,
    pub idempotency_key: Option<String>,
    #[serde(with = "codec::json")]
    pub frozen_request: JsonText,
    pub attempts: u64,
    pub dispatch_count_window: Option<DispatchCount>,
    pub audit_at: i64,
    pub reservation: Option<Reservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KvEffect {
    Set {
        key: String,
        #[serde(with = "codec::json")]
        value: JsonText,
    },
    Delete {
        key: String,
    },
}

/// Facts that make an outcome externally observable and therefore require sync.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrivalDurability {
    pub refusal_committed: bool,
    pub host_callback: bool,
    pub immediate_completion: bool,
    pub settles_reservation: bool,
}

/// Full provider measurements, separate from the conservative budget charge in
/// Usage. Missing measurements remain absent rather than becoming zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    pub input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
}
impl From<basal_host::TokenUsage> for UsageReport {
    fn from(value: basal_host::TokenUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            cache_write_tokens: value.cache_write_tokens,
            output_tokens: value.output_tokens,
            cached_input_tokens: value.cached_input_tokens,
        }
    }
}
impl From<UsageReport> for basal_host::TokenUsage {
    fn from(value: UsageReport) -> Self {
        Self {
            input_tokens: value.input_tokens,
            cache_write_tokens: value.cache_write_tokens,
            output_tokens: value.output_tokens,
            cached_input_tokens: value.cached_input_tokens,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arrival {
    pub position: u64,
    pub arrival_seq: u64,
    pub outcome: String,
    #[serde(with = "codec::settlement")]
    pub settlement: Settlement,
    #[serde(with = "codec::json")]
    pub value: JsonText,
    pub value_hash: [u8; 32],
    pub handle: Option<String>,
    pub usage: Option<Usage>,
    pub cost_micros: Option<u64>,
    pub kv_effect: Option<KvEffect>,
    pub durability: ArrivalDurability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sync {
    pub call: CallIdentity,
    pub delivery_order: u64,
    #[serde(with = "codec::json")]
    pub value: JsonText,
    pub clock_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deferred {
    pub position: u64,
    pub retry_not_before: i64,
    pub cause: String,
    pub audit_at: i64,
    /// May this be resent? An unsent proof permits even an unkeyed retry.
    pub proven_unsent: bool,
    /// Did this attempt use the flow's dispatch budget? True means no: refund
    /// its charge. A transport attempt sets false even when proven unsent.
    pub refund_dispatch: bool,
    /// Intent ordinal this refusal belongs to; absent before authorization or
    /// in older records. The appender fills it from the current durable intent.
    pub attempt: Option<u64>,
}

// The binary format used one byte for proven_unsent. Keep its low bit and use
// bit 1 for a retained budget charge and bit 2 for an appended intent ordinal.
// Old 0/1 bytes therefore decode with a refund and no ordinal, including
// deferrals nested in snapshots, without shifting any old field.
// Human-readable call detail keeps both policy fields explicit.
impl Serialize for Deferred {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            use serde::ser::SerializeStruct;
            let mut value = serializer.serialize_struct("Deferred", 7)?;
            value.serialize_field("position", &self.position)?;
            value.serialize_field("retry_not_before", &self.retry_not_before)?;
            value.serialize_field("cause", &self.cause)?;
            value.serialize_field("audit_at", &self.audit_at)?;
            value.serialize_field("proven_unsent", &self.proven_unsent)?;
            value.serialize_field("refund_dispatch", &self.refund_dispatch)?;
            value.serialize_field("attempt", &self.attempt)?;
            value.end()
        } else {
            use serde::ser::SerializeTuple;
            let flags = u8::from(self.proven_unsent)
                | if self.refund_dispatch { 0 } else { 2 }
                | if self.attempt.is_some() { 4 } else { 0 };
            let mut value =
                serializer.serialize_tuple(if self.attempt.is_some() { 6 } else { 5 })?;
            value.serialize_element(&self.position)?;
            value.serialize_element(&self.retry_not_before)?;
            value.serialize_element(&self.cause)?;
            value.serialize_element(&self.audit_at)?;
            value.serialize_element(&flags)?;
            if let Some(attempt) = self.attempt {
                value.serialize_element(&attempt)?;
            }
            value.end()
        }
    }
}
impl<'de> Deserialize<'de> for Deferred {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            #[derive(Deserialize)]
            struct Human {
                position: u64,
                retry_not_before: i64,
                cause: String,
                audit_at: i64,
                proven_unsent: bool,
                #[serde(default = "refund_by_default")]
                refund_dispatch: bool,
                #[serde(default)]
                attempt: Option<u64>,
            }
            let value = Human::deserialize(deserializer)?;
            Ok(Self {
                position: value.position,
                retry_not_before: value.retry_not_before,
                cause: value.cause,
                audit_at: value.audit_at,
                proven_unsent: value.proven_unsent,
                refund_dispatch: value.refund_dispatch,
                attempt: value.attempt,
            })
        } else {
            struct Binary;
            impl<'de> serde::de::Visitor<'de> for Binary {
                type Value = Deferred;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("a binary deferral")
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<Deferred, A::Error> {
                    use serde::de::Error;
                    let position = seq
                        .next_element()?
                        .ok_or_else(|| Error::missing_field("position"))?;
                    let retry_not_before = seq
                        .next_element()?
                        .ok_or_else(|| Error::missing_field("retry_not_before"))?;
                    let cause = seq
                        .next_element()?
                        .ok_or_else(|| Error::missing_field("cause"))?;
                    let audit_at = seq
                        .next_element()?
                        .ok_or_else(|| Error::missing_field("audit_at"))?;
                    let flags: u8 = seq
                        .next_element()?
                        .ok_or_else(|| Error::missing_field("flags"))?;
                    if flags > 7 {
                        return Err(Error::custom("invalid deferral flags"));
                    }
                    let attempt = if flags & 4 != 0 {
                        Some(
                            seq.next_element()?
                                .ok_or_else(|| Error::missing_field("attempt"))?,
                        )
                    } else {
                        None
                    };
                    Ok(Deferred {
                        position,
                        retry_not_before,
                        cause,
                        audit_at,
                        proven_unsent: flags & 1 != 0,
                        refund_dispatch: flags & 2 == 0,
                        attempt,
                    })
                }
            }
            deserializer.deserialize_tuple(6, Binary)
        }
    }
}
fn refund_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub position: u64,
    pub attempts: u64,
    pub idempotency_key: Option<String>,
    pub choice: String,
    pub observed_outcome: Option<Arrival>,
    pub usage: Option<Usage>,
    pub elicitation_id: String,
}

/// Run identity and activation generation accompany every frame, including snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub run_id: String,
    pub generation: u64,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Body {
    Claimed {
        first_claim_ms: i64,
        deadline_at: i64,
    },
    Activated,
    Requeued {
        broken: u64,
    },
    Fingerprint {
        value: String,
    },
    ScriptStarted,
    Intent(Intent),
    Issued(CallIdentity),
    Accepted {
        position: u64,
        handle: String,
    },
    Arrived(Arrival),
    Released {
        position: u64,
        delivery_order: u64,
    },
    Sync(Sync),
    Deferred(Deferred),
    Unknown {
        position: u64,
        reason: String,
    },
    ScopeCut {
        deadline_exempt: bool,
    },
    EventBody {
        digest: String,
        body: Option<String>,
        refusal: Option<String>,
        attempt: u64,
        retry_at: Option<i64>,
    },
    Parked {
        awaited: Vec<u64>,
    },
    Woken,
    Resolution(Resolution),
    Snapshot {
        cutoff_lsn: Lsn,
        next_delivery_order: u64,
        next_arrival_seq: u64,
        state: Box<State>,
    },
    Closed {
        commit_seq: u64,
    },
    /// A dispatched write lost its target authority. This marker is synced
    /// before exposing the post-dispatch boundary, independently of its reply.
    CallScopeCut {
        position: u64,
    },
    MeasuredUsage {
        position: u64,
        report: UsageReport,
    },
}
