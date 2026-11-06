use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use rusqlite::{Transaction, params};
use serde_json::{Value, json};

use super::{Budgets, accounting};
use crate::{
    CoreError, Result,
    model::{RunState, StoredClass},
    run_records::{Records, State},
    run_store::{Reservation, TerminalBuilder, TerminalWrite},
    run_store_sql::{self as sql, LiveRun, RunHistory},
};

/// Owner notices, template hooks, deferred refusal outbox rows and host-specific
/// obligations are captured while closing holds the actor and accounting lock.
/// The returned closure owns its inputs and performs SQLite work only. It must
/// not query the old execution tables or take a registry/accounting lock.
pub trait TerminalEffects: Send + Sync + 'static {
    fn capture(
        &self,
        live: &LiveRun,
        state: &State,
        terminal: RunState,
    ) -> Result<Box<dyn TerminalWrite>>;
}

pub struct Terminal {
    pub live: LiveRun,
    pub state: RunState,
    pub ended_at: i64,
    pub error_kind: Option<String>,
    pub error_detail: Option<String>,
    pub result: Option<Value>,
    pub budgets: Budgets,
    pub effects: Arc<dyn TerminalEffects>,
    pub(super) receipt: Arc<AtomicU64>,
}

impl Terminal {
    pub fn new(
        live: LiveRun,
        state: RunState,
        ended_at: i64,
        budgets: Budgets,
        effects: Arc<dyn TerminalEffects>,
    ) -> Self {
        Self {
            live,
            state,
            ended_at,
            error_kind: None,
            error_detail: None,
            result: None,
            budgets,
            effects,
            receipt: Arc::new(AtomicU64::new(0)),
        }
    }
    pub(crate) fn commit_seq(&self) -> u64 {
        self.receipt.load(Ordering::Acquire)
    }
}

fn bounded(text: &Option<String>, bytes: usize) -> Option<String> {
    text.as_ref().map(|s| {
        let mut end = bytes.min(s.len());
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_owned()
    })
}

fn outcome(call: &crate::run_records::CallState) -> sql::CallOutcome {
    if call.sync.is_some() {
        return sql::CallOutcome::Fulfilled;
    }
    if let Some(arrival) = &call.arrival {
        return match arrival.settlement {
            basal_proto::Settlement::Fulfilled => sql::CallOutcome::Fulfilled,
            basal_proto::Settlement::Rejected => sql::CallOutcome::Rejected,
        };
    }
    if call.unknown.is_some() {
        sql::CallOutcome::Unknown
    } else {
        sql::CallOutcome::Pending
    }
}

impl TerminalBuilder<Records> for Terminal {
    fn prepare(&self, records: &mut Records) -> Result<()> {
        let deferred: Vec<_> = records
            .state()
            .calls
            .values()
            .filter(|c| c.arrival.is_none() && c.deferred.as_ref().is_some_and(|d| d.proven_unsent))
            .cloned()
            .collect();
        for call in deferred {
            let deferred = call.deferred.expect("filtered deferral");
            let outcome = crate::refusal_receipt::decode_refusal(&deferred.cause)
                .map(|r| r.outcome())
                .unwrap_or_else(|_| crate::kv::rejection("route_unavailable", &deferred.cause));
            records.append(
                records.state().generation,
                crate::run_records::Body::Arrived(crate::run_records::Arrival {
                    position: call.identity.position,
                    arrival_seq: records.state().next_arrival_seq,
                    outcome: "rejected".into(),
                    settlement: basal_proto::Settlement::Rejected,
                    value_hash: crate::ids::payload_hash(outcome.settlement, &outcome.value),
                    value: outcome.value,
                    handle: None,
                    usage: Some(crate::run_records::Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    }),
                    cost_micros: Some(0),
                    kv_effect: None,
                    durability: crate::run_records::ArrivalDurability {
                        refusal_committed: true,
                        ..Default::default()
                    },
                }),
            )?;
        }
        Ok(())
    }
    fn can_close(&self, records: &Records) -> bool {
        self.error_kind.as_deref() != Some("deadline")
            || !records
                .state()
                .calls
                .values()
                .any(|c| c.unknown.is_some() && c.arrival.is_none())
    }
    fn is_success(&self) -> bool {
        self.state == RunState::Succeeded
    }
    fn build(
        &self,
        run_id: &str,
        records: &Records,
        _: &[Reservation],
    ) -> Result<Box<dyn TerminalWrite>> {
        if run_id != self.live.run_id || !self.state.is_terminal() {
            return Err(CoreError::Invalid(
                "terminal identity or state is invalid".into(),
            ));
        }
        let state = records.state().clone();
        let rows = accounting::rows(&state, &self.budgets)?;
        let mut classes = BTreeMap::<String, u64>::new();
        let mut outcomes = BTreeMap::<String, u64>::new();
        let mut writes_sent = 0;
        for call in state.calls.values() {
            *classes
                .entry(call.identity.class.as_str().into())
                .or_default() += 1;
            *outcomes
                .entry(format!("{:?}", outcome(call)).to_lowercase())
                .or_default() += 1;
            if matches!(
                call.identity.class,
                StoredClass::Mutation | StoredClass::KeyedMutation
            ) {
                writes_sent += call.intent.iter().chain(&call.prior_intents).count() as u64;
            }
        }
        let total = |kind: &str| -> Result<u64> {
            rows.iter()
                .filter(|r| r.window.0.starts_with(kind))
                .try_fold(0u64, |n, r| {
                    n.checked_add(r.amount)
                        .ok_or_else(|| CoreError::Invalid("summary overflow".into()))
                })
        };
        let summary = RunHistory {
            run_id: run_id.into(),
            flow_id: self.live.flow_id.clone(),
            flow_version: self.live.flow_version,
            trigger_id: self.live.trigger_id.clone(),
            trigger_kind: self.live.trigger_kind.clone(),
            trigger: self.live.trigger.clone(),
            admitted_at: self.live.admitted_at,
            admit_seq: self.live.admit_seq,
            first_claim_at: state.claim.as_ref().map(|c| c.first_claim_ms),
            ended_at: self.ended_at,
            state: self.state,
            error_kind: bounded(&self.error_kind, 256),
            error_detail: bounded(&self.error_detail, 8192),
            result: self.result.clone(),
            result_size: self
                .result
                .as_ref()
                .map_or(0, |r| r.to_string().len() as u64),
            call_counts: json!({"class": classes, "outcome": outcomes}),
            writes_sent,
            tokens_charged: total("token:")?,
            cost_microusd: total("cost:")?,
            broken: u32::try_from(state.broken)
                .map_err(|_| CoreError::Invalid("broken count overflow".into()))?,
            activation_count: state.activation_count,
            script_started: state.script_started,
            phase: state.phase.clone(),
            // Target-registry outages are control-plane readiness failures,
            // not script failures. Expiry of that still-pending, proven-unsent
            // wait must not trip the flow's consecutive-failure breaker.
            deadline_exempt: state.deadline_exempt || (self.error_kind.as_deref()==Some("deadline") && state.calls.values().any(|call| {
                call.deferred.as_ref().filter(|d| d.proven_unsent).and_then(|d| crate::refusal_receipt::decode_refusal(&d.cause).ok()).is_some_and(|r| matches!(r.reason,basal_host::flow_refusal::RefusalReason::SinkAudienceUnavailable | basal_host::flow_refusal::RefusalReason::SinkBoardTargetUnavailable))
            })),
            history_lost: false,
            commit_seq: 0,
        };
        let effects = self.effects.capture(&self.live, &state, self.state)?;
        records.sync_terminal()?;
        Ok(Box::new(Projection {
            summary,
            incarnation: self.live.template_incarnation,
            state,
            budgets: self.budgets.clone(),
            effects,
            receipt: self.receipt.clone(),
        }))
    }
}

struct Projection {
    incarnation: Option<i64>,
    summary: RunHistory,
    state: State,
    budgets: Budgets,
    effects: Box<dyn TerminalWrite>,
    receipt: Arc<AtomicU64>,
}

impl TerminalWrite for Projection {
    fn apply(&self, tx: &Transaction<'_>) -> Result<()> {
        let mut history = self.summary.clone();
        history.commit_seq = sql::next_commit_seq(tx)?;
        sql::finish_live(tx, &history)?;
        let id = &history.run_id;
        // A re-created template owns a new state domain. Ordinary closes,
        // including failure/cancellation in the same incarnation, keep overlays.
        if crate::run_removal::same_incarnation(tx, &history.flow_id, self.incarnation)? {
            let mut effects: Vec<_> = self
                .state
                .calls
                .values()
                .filter_map(|c| c.arrival.as_ref())
                .collect();
            effects.sort_by_key(|a| a.arrival_seq);
            let mut revisions = BTreeMap::<String, (u64, bool)>::new();
            for arrival in effects {
                match &arrival.kv_effect {
                    Some(crate::run_records::KvEffect::Set { key, .. }) => {
                        revisions.entry(key.clone()).or_default().0 += 1;
                    }
                    Some(crate::run_records::KvEffect::Delete { key }) => {
                        revisions.insert(key.clone(), (0, true));
                    }
                    None => {}
                }
            }
            // This run's overlay commits with its summary in the same transaction.
            for (key, value) in &self.state.overlay {
                match value {
                    Some(value) => {
                        let (sets, reset) = revisions[key];
                        if reset {
                            tx.execute(
                                "DELETE FROM kv WHERE flow_id=?1 AND key=?2",
                                params![history.flow_id, key],
                            )?;
                        }
                        tx.execute("INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(flow_id,key) DO UPDATE SET value=excluded.value,bytes=excluded.bytes,revision=kv.revision+excluded.revision", params![history.flow_id,key,value, (key.len()+value.len()) as i64, accounting::integer(sets)?])?;
                    }
                    None => {
                        tx.execute(
                            "DELETE FROM kv WHERE flow_id=?1 AND key=?2",
                            params![history.flow_id, key],
                        )?;
                    }
                }
            }
        }
        accounting::fold(
            tx,
            &history.flow_id,
            id,
            &self.state,
            &self.budgets,
            history.ended_at,
        )?;
        tx.execute("INSERT OR IGNORE INTO tombstones(kind,key,run_id,pruned_at) VALUES('trigger',?1,?2,?3)", params![crate::admission::trigger_key(&history.flow_id,&history.trigger_id),id,history.ended_at])?;
        for call in self.state.calls.values() {
            if matches!(
                call.identity.class,
                StoredClass::Mutation | StoredClass::KeyedMutation
            ) && let Some(key) = call
                .intent
                .as_ref()
                .and_then(|i| i.idempotency_key.as_ref())
            {
                tx.execute("INSERT OR IGNORE INTO tombstones(kind,key,run_id,pruned_at) VALUES('idempotency_key',?1,?2,?3)", params![key,id,history.ended_at])?;
            }
            if history.state != RunState::Succeeded {
                sql::insert_call(
                    tx,
                    &sql::RunCall {
                        run_id: id.clone(),
                        position: call.identity.position,
                        kind: call.identity.kind.clone(),
                        class: call.identity.class,
                        args_digest: call.identity.args_digest.0,
                        outcome: outcome(call),
                        error_code: call
                            .arrival
                            .as_ref()
                            .filter(|a| a.settlement == basal_proto::Settlement::Rejected)
                            .and_then(|a| serde_json::from_str::<Value>(a.value.as_str()).ok())
                            .and_then(|v| v["code"].as_str().map(str::to_owned)),
                        issued_at: call
                            .intent
                            .as_ref()
                            .map_or(history.admitted_at, |i| i.audit_at),
                        arrived_at: None,
                        released_at: None,
                        detail: serde_json::to_value(call)
                            .map_err(|e| CoreError::Invalid(e.to_string()))?,
                    },
                )?;
            }
            if let Some(intent) = &call.intent {
                // Receipts survive terminal close so a host can redeliver an
                // already acknowledged value without being quarantined.
                let arrival = call.arrival.as_ref();
                let settled = arrival.is_some();
                sql::insert_obligation(
                    tx,
                    &sql::Obligation {
                        run_id: id.clone(),
                        position: call.identity.position,
                        handle: arrival
                            .and_then(|a| a.handle.clone())
                            .or_else(|| call.accepted.clone()),
                        state: if settled {
                            sql::ObligationState::Settled
                        } else if call.unknown.is_some() {
                            sql::ObligationState::Unknown
                        } else {
                            sql::ObligationState::Open
                        },
                        audit_at: intent.audit_at,
                        detail: if settled {
                            Value::Null
                        } else {
                            serde_json::to_value(call)
                                .map_err(|e| CoreError::Invalid(e.to_string()))?
                        },
                        receipt: arrival.map(|a| sql::Receipt::Value(a.value_hash)),
                        settled_at: settled.then_some(history.ended_at),
                    },
                )?;
            }
        }
        self.effects.apply(tx)?;
        self.receipt.store(history.commit_seq, Ordering::Release);
        Ok(())
    }
}
