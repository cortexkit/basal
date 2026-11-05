use std::collections::BTreeMap;

use basal_proto::{ArgsDigest, HostCall, RecordedCall, RecordedOutcome};
use serde::{Deserialize, Serialize};

use super::{Arrival, Body, CallIdentity, Deferred, Intent, KvEffect, Resolution, Sync};
use crate::{CoreError, Result, model::StoredClass};

fn corrupt(message: &str) -> CoreError {
    CoreError::Corrupt(format!("run records: {message}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub first_claim_ms: i64,
    pub deadline_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallState {
    pub identity: CallIdentity,
    pub intent: Option<Intent>,
    /// Older dispatch attempts still contribute accounting and audit facts.
    pub prior_intents: Vec<Intent>,
    pub sync: Option<Sync>,
    pub accepted: Option<String>,
    pub arrival: Option<Arrival>,
    pub delivery_order: Option<u64>,
    pub deferred: Option<Deferred>,
    pub prior_deferrals: Vec<Deferred>,
    pub unknown: Option<String>,
    pub resolutions: Vec<Resolution>,
}

/// The complete replay start point. The worker receives every issued call but
/// only outcomes that have been released to the script. Unreleased arrivals
/// remain in the mailbox; local kv effects already belong to the pending writes
/// that this run reads and will commit to SQLite when it ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub admitted_ms: i64,
    pub generation: u64,
    pub claim: Option<Claim>,
    pub activation_count: u64,
    pub broken: u64,
    pub fingerprint: Option<String>,
    pub script_started: bool,
    pub phase: String,
    pub deadline_exempt: bool,
    pub awaited: Vec<u64>,
    pub parked: bool,
    pub readiness: u64,
    pub event_bodies: Vec<Body>,
    pub calls: BTreeMap<u64, CallState>,
    pub overlay: BTreeMap<String, Option<String>>,
    pub next_delivery_order: u64,
    pub next_arrival_seq: u64,
    pub commit_seq: Option<u64>,
}

impl State {
    pub fn new(admitted_ms: i64) -> Self {
        Self {
            admitted_ms,
            generation: 0,
            claim: None,
            activation_count: 0,
            broken: 0,
            fingerprint: None,
            script_started: false,
            phase: "admitted".into(),
            deadline_exempt: false,
            awaited: Vec::new(),
            parked: false,
            readiness: 0,
            event_bodies: Vec::new(),
            calls: BTreeMap::new(),
            overlay: BTreeMap::new(),
            next_delivery_order: 0,
            next_arrival_seq: 0,
            commit_seq: None,
        }
    }

    fn insert(&mut self, identity: &CallIdentity) -> Result<&mut CallState> {
        if identity.args_digest != ArgsDigest::of(&identity.args) {
            return Err(corrupt("identity argument digest mismatch"));
        }
        if identity.position != self.calls.len() as u64 {
            return Err(corrupt("duplicate identity or noncontiguous position"));
        }
        Ok(self
            .calls
            .entry(identity.position)
            .or_insert_with(|| CallState {
                identity: identity.clone(),
                intent: None,
                prior_intents: Vec::new(),
                sync: None,
                accepted: None,
                arrival: None,
                delivery_order: None,
                deferred: None,
                prior_deferrals: Vec::new(),
                unknown: None,
                resolutions: Vec::new(),
            }))
    }

    fn call_mut(&mut self, position: u64) -> Result<&mut CallState> {
        self.calls
            .get_mut(&position)
            .ok_or_else(|| corrupt("call has no identity"))
    }

    fn order(&mut self, order: u64) -> Result<()> {
        if order != self.next_delivery_order {
            return Err(corrupt("delivery order is not the next shared order"));
        }
        self.next_delivery_order = order
            .checked_add(1)
            .ok_or_else(|| corrupt("delivery order overflow"))?;
        Ok(())
    }

    pub fn check_issued(&self, call: &HostCall) -> Result<()> {
        let recorded = self
            .calls
            .get(&call.position)
            .ok_or_else(|| corrupt("call has no identity"))?;
        if recorded.identity.kind != call.kind
            || recorded.identity.args_digest != ArgsDigest::of(&call.args)
        {
            return Err(CoreError::Worker(format!(
                "nondeterminism at position {}: call identity changed",
                call.position
            )));
        }
        Ok(())
    }

    pub fn prefix(&self) -> Vec<RecordedCall> {
        self.calls
            .values()
            .map(|call| RecordedCall {
                position: call.identity.position,
                kind: call.identity.kind.clone(),
                args_digest: call.identity.args_digest,
                outcome: if let Some(sync) = &call.sync {
                    Some(RecordedOutcome {
                        settlement: basal_proto::Settlement::Fulfilled,
                        value: sync.value.clone(),
                        delivery_order: sync.delivery_order,
                    })
                } else {
                    call.delivery_order.and_then(|delivery_order| {
                        call.arrival.as_ref().map(|arrival| RecordedOutcome {
                            settlement: arrival.settlement,
                            value: arrival.value.clone(),
                            delivery_order,
                        })
                    })
                },
            })
            .collect()
    }

    pub(super) fn requires_sync(&self, body: &Body) -> Result<bool> {
        Ok(match body {
            Body::Intent(_)
            | Body::Accepted { .. }
            | Body::Requeued { .. }
            | Body::Unknown { .. }
            | Body::Resolution(_)
            | Body::Snapshot { .. } => true,
            Body::Arrived(arrival) => {
                let call = self
                    .calls
                    .get(&arrival.position)
                    .ok_or_else(|| corrupt("arrival has no identity"))?;
                call.identity.class == StoredClass::Mutation
                    || arrival.durability.refusal_committed
                    || arrival.durability.host_callback
                    || arrival.durability.immediate_completion
                    || arrival.durability.settles_reservation
                    || call
                        .intent
                        .as_ref()
                        .is_some_and(|i| i.reservation.is_some())
            }
            Body::Deferred(deferred) => {
                let call = self
                    .calls
                    .get(&deferred.position)
                    .ok_or_else(|| corrupt("deferral has no identity"))?;
                call.identity.class == StoredClass::Mutation || deferred.proven_unsent
            }
            _ => false,
        })
    }

    pub(super) fn apply(&mut self, generation: u64, body: &Body) -> Result<()> {
        match body {
            Body::Claimed {
                first_claim_ms,
                deadline_at,
            } => {
                if self.claim.is_some() {
                    return Err(corrupt("claim and deadline are already fixed"));
                }
                self.claim = Some(Claim {
                    first_claim_ms: *first_claim_ms,
                    deadline_at: *deadline_at,
                });
                self.generation = generation;
                self.phase = "claimed".into();
            }
            Body::Activated => {
                if generation < self.generation {
                    return Err(corrupt("activation generation went backwards"));
                }
                self.generation = generation;
                self.activation_count = self
                    .activation_count
                    .checked_add(1)
                    .ok_or_else(|| corrupt("activation count overflow"))?;
            }
            Body::Requeued { broken } => {
                if *broken < self.broken {
                    return Err(corrupt("broken count went backwards"));
                }
                self.broken = *broken;
                self.phase = "requeued".into();
            }
            Body::Fingerprint { value } => {
                if self.fingerprint.as_ref().is_some_and(|old| old != value) {
                    return Err(corrupt("fingerprint changed"));
                }
                self.fingerprint = Some(value.clone());
            }
            Body::ScriptStarted => {
                self.script_started = true;
                self.phase = "script".into();
            }
            Body::Intent(intent) => {
                if matches!(intent.call.class, StoredClass::Local | StoredClass::Sync) {
                    return Err(corrupt("local or sync call cannot be possibly sent"));
                }
                if let Some(old) = self.calls.get_mut(&intent.call.position) {
                    let previous = old
                        .intent
                        .as_ref()
                        .ok_or_else(|| corrupt("second identity record"))?;
                    if old.identity != intent.call
                        || intent.idempotency_key != previous.idempotency_key
                        || intent.attempts <= previous.attempts
                    {
                        return Err(corrupt(
                            "retry changed identity/key or did not increase attempts",
                        ));
                    }
                    old.prior_intents.push(previous.clone());
                    old.intent = Some(intent.clone());
                    // Proof that the previous attempt was unsent cannot
                    // authorize another retry after this attempt may be sent.
                    if let Some(deferred) = old.deferred.take() {
                        old.prior_deferrals.push(deferred);
                    }
                } else {
                    self.insert(&intent.call)?.intent = Some(intent.clone());
                }
            }
            Body::Issued(identity) => {
                self.insert(identity)?;
            }
            Body::Accepted { position, handle } => {
                let call = self.call_mut(*position)?;
                if call.intent.is_none() || call.accepted.as_ref().is_some_and(|old| old != handle)
                {
                    return Err(corrupt("acceptance has no intent or changed handle"));
                }
                call.accepted = Some(handle.clone());
            }
            Body::Arrived(arrival) => {
                if arrival.arrival_seq != self.next_arrival_seq {
                    return Err(corrupt("arrival sequence is not next"));
                }
                if arrival.value_hash
                    != crate::ids::payload_hash(arrival.settlement, &arrival.value)
                {
                    return Err(corrupt("arrival value hash mismatch"));
                }
                let call = self.call_mut(arrival.position)?;
                if call.arrival.is_some() || call.sync.is_some() {
                    return Err(corrupt("second outcome"));
                }
                if arrival.kv_effect.is_some() && call.identity.class != StoredClass::Local {
                    return Err(corrupt("non-local call has a kv effect"));
                }
                call.arrival = Some(arrival.clone());
                match &arrival.kv_effect {
                    Some(KvEffect::Set { key, value }) => {
                        self.overlay
                            .insert(key.clone(), Some(value.as_str().to_owned()));
                    }
                    Some(KvEffect::Delete { key }) => {
                        self.overlay.insert(key.clone(), None);
                    }
                    None => {}
                }
                self.next_arrival_seq = self
                    .next_arrival_seq
                    .checked_add(1)
                    .ok_or_else(|| corrupt("arrival sequence overflow"))?;
                self.readiness = self
                    .readiness
                    .checked_add(1)
                    .ok_or_else(|| corrupt("readiness overflow"))?;
            }
            Body::Released {
                position,
                delivery_order,
            } => {
                let call = self.call_mut(*position)?;
                if call.arrival.is_none() || call.delivery_order.is_some() {
                    return Err(corrupt("release without a fresh arrival"));
                }
                call.delivery_order = Some(*delivery_order);
                self.order(*delivery_order)?;
            }
            Body::Sync(sync) => {
                if sync.call.class != StoredClass::Sync || !sync.call.kind.is_synchronous() {
                    return Err(corrupt("Sync is not clock or random"));
                }
                self.insert(&sync.call)?.sync = Some(sync.clone());
                self.order(sync.delivery_order)?;
            }
            Body::Deferred(deferred) => {
                let call = self.call_mut(deferred.position)?;
                if let Some(prior) = call.deferred.replace(deferred.clone()) {
                    call.prior_deferrals.push(prior);
                }
            }
            Body::Unknown { position, reason } => {
                self.call_mut(*position)?.unknown = Some(reason.clone());
            }
            Body::ScopeCut { deadline_exempt } => {
                self.deadline_exempt = *deadline_exempt;
                self.phase = "scope_cut".into();
            }
            Body::EventBody { .. } => {
                self.event_bodies.push(body.clone());
            }
            Body::Parked { awaited } => {
                if awaited.iter().any(|p| !self.calls.contains_key(p)) {
                    return Err(corrupt("awaited call has no identity"));
                }
                self.awaited = awaited.clone();
                self.parked = true;
                self.phase = "parked".into();
            }
            Body::Woken => {
                self.parked = false;
                self.phase = "woken".into();
                self.readiness = self
                    .readiness
                    .checked_add(1)
                    .ok_or_else(|| corrupt("readiness overflow"))?;
            }
            Body::Resolution(resolution) => {
                self.call_mut(resolution.position)?
                    .resolutions
                    .push(resolution.clone());
                if matches!(
                    resolution.choice.as_str(),
                    "not_applied" | "applied" | "observed_result"
                ) {
                    self.call_mut(resolution.position)?.unknown = None;
                    if let Some(arrival) = &resolution.observed_outcome {
                        self.apply(generation, &Body::Arrived(arrival.clone()))?;
                    }
                    if self
                        .calls
                        .values()
                        .all(|c| c.unknown.is_none() || c.arrival.is_some())
                    {
                        self.parked = false;
                        self.phase = "pending".into();
                        self.readiness = self
                            .readiness
                            .checked_add(1)
                            .ok_or_else(|| corrupt("readiness overflow"))?;
                    }
                }
            }
            Body::Closed { commit_seq } => {
                self.commit_seq = Some(*commit_seq);
                self.phase = "closed".into();
            }
            Body::Snapshot { .. } => {
                return Err(corrupt(
                    "snapshot must be installed as a replay start point",
                ));
            }
        }
        Ok(())
    }
}
