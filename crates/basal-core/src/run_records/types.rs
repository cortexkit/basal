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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deferred {
    pub position: u64,
    pub retry_not_before: i64,
    pub cause: String,
    pub audit_at: i64,
    pub proven_unsent: bool,
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
}
