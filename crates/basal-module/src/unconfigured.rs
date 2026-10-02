//! The hosts the production binary runs with until real adapters exist.
//!
//! Each one refuses honestly rather than pretending to work: the host
//! refuses every dispatch as a transport failure proven never sent (so the
//! runtime records a journaled `unavailable` rejection and never treats the
//! call as possibly applied), the catalog knows no module, op or agent (so
//! every install that names one is refused), and the consent plane is
//! unreachable (so no version can be approved).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use basal_host::{
    CallClass, CallRequest, Catalog, Consent, ConsentError, DecisionCard, DecisionSink, Dispatched,
    EventDecl, Host, InstallCard, OpDecl, TransportError,
};
use basal_proto::CallKind;

/// Why the unconfigured host refused a dispatch.
pub const NO_HOST: &str =
    "no host adapter is configured in this build of ck-basal; the call was never sent";

/// A host that sends nothing.
#[derive(Default)]
pub struct UnconfiguredHost {
    random_counter: AtomicU64,
    random_seed: RandomState,
}

impl UnconfiguredHost {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Host for UnconfiguredHost {
    fn classify(&self, _kind: &CallKind) -> CallClass {
        // Nothing is known about any op, so every call is treated as a
        // mutation that does not deduplicate: the most careful class.
        // Nothing is ever sent, and the refusal below is proven unsent, so
        // the class never leads to a resend or a reconcile.
        CallClass::Mutation {
            honours_idempotency_keys: false,
        }
    }

    fn dispatch(&self, _request: &CallRequest) -> Result<Dispatched, TransportError> {
        Err(TransportError::Unavailable {
            proven_unsent: true,
            detail: NO_HOST.to_owned(),
        })
    }

    fn now_ms(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0)
    }

    fn random(&self) -> f64 {
        // The standard library's hasher is seeded randomly per process; a
        // counter through it gives independent samples without another
        // dependency. A script's randomness is journaled, so replay never
        // asks for the same sample twice.
        let mut hasher = self.random_seed.build_hasher();
        hasher.write_u64(self.random_counter.fetch_add(1, Ordering::Relaxed));
        let bits = hasher.finish() >> 11;
        bits as f64 / (1u64 << 53) as f64
    }

    fn attach(&self, _sink: Arc<dyn basal_host::CompletionSink>) {
        // Nothing is ever accepted as long-running, so no completion will
        // ever arrive.
    }
}

/// A catalog that declares nothing.
pub struct EmptyCatalog;

impl Catalog for EmptyCatalog {
    fn event(&self, _module: &str, _name: &str, _version: u32) -> Option<EventDecl> {
        None
    }

    fn op(&self, _module: &str, _op: &str) -> Option<OpDecl> {
        None
    }

    fn agent_known(&self, _agent: &str) -> bool {
        false
    }
}

/// A consent plane that cannot be reached.
pub struct UnconfiguredConsent;

impl Consent for UnconfiguredConsent {
    fn raise(&self, _card: &InstallCard) -> Result<(), ConsentError> {
        Err(ConsentError::Unavailable(
            "no consent adapter is configured in this build of ck-basal".to_owned(),
        ))
    }

    fn raise_decision(&self, _card: &DecisionCard) -> Result<String, ConsentError> {
        Err(ConsentError::Unavailable(
            "no consent adapter is configured in this build of ck-basal".to_owned(),
        ))
    }

    fn attach(&self, _sink: Arc<dyn DecisionSink>) {}
}
