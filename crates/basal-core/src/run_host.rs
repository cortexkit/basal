//! Staged host callbacks for log-backed runs. Production construction remains
//! on the journal path until cutover. Recovery builds this value before a host
//! may attach, since attach can deliver saved completions synchronously.
//!
//! | Entry point | Locks, in acquisition order | Work after releasing those locks |
//! | --- | --- | --- |
//! | Recovery | SQLite reads, released before registry publication | Host attachment afterward |
//! | Live callback | Broca send gate (if any), actor, accounting | Quarantine transaction, crash hook and acknowledgement afterward |
//! | Closed callback | Broca send gate (if any), accounting, SQLite | Crash hook and acknowledgement after commit |
//! | Broca state read | Actor snapshot, released before any SQLite fallback | Host polling afterward |

use std::sync::Arc;

use basal_host::{
    Completion, CompletionAck, CompletionSink, HostOutcome, SinkError, UnknownOutcome,
};
use rusqlite::params;

use crate::{
    CoreError, Result, Store,
    clock::Clock,
    hooks::{Boundary, Hooks, Step},
    run_admission::AdmissionLog,
    run_driver::{Budgets, accounting},
    run_log::RecoveredRecord,
    run_records::{Arrival, ArrivalDurability, Body, Record, RecordLog, Records, Usage},
    run_store::{Routing, RunStore},
    run_store_sql::{self as sql, Receipt, SettleResult},
};

pub struct RecoveredHosts {
    pub(crate) store: Arc<Store>,
    registry: Arc<RunStore<Records>>,
    pub(crate) budgets: Budgets,
    hooks: Arc<dyn Hooks>,
    pub(crate) clock: Clock,
    pub(crate) watermark: Arc<dyn AdmissionLog>,
    pub(crate) log: Arc<dyn RecordLog>,
    pub(crate) decisions: std::sync::Mutex<Option<crate::run_decisions::CloseServices>>,
}

fn budgets_for(store: &Store, id: &str, defaults: &Budgets) -> Result<Budgets> {
    let manifest: String = store.read(|conn| Ok(conn.query_row(
        "SELECT s.manifest FROM run_identities i JOIN run_snapshots s USING(snapshot_id) WHERE i.run_id=?1", [id], |r| r.get(0))?))?;
    let value: serde_json::Value =
        serde_json::from_str(&manifest).map_err(|e| CoreError::Corrupt(e.to_string()))?;
    let mut budgets = defaults.clone();
    if let Some(cap) = value.get("llm").and_then(|v| v.get("token_cap")) {
        budgets.tokens = cap["tokens"]
            .as_u64()
            .ok_or_else(|| CoreError::Corrupt("invalid token cap".into()))?;
        budgets.token_window_ms = cap["window"]
            .as_str()
            .and_then(crate::manifest::duration_ms)
            .ok_or_else(|| CoreError::Corrupt("invalid token window".into()))?;
    }
    if let Some(cap) = value.get("model_cost_usd").filter(|cap| !cap.is_null()) {
        let micros = |key: &str| -> Result<u64> {
            let n = cap[key]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0 && *n * 1_000_000.0 <= u64::MAX as f64)
                .ok_or_else(|| CoreError::Corrupt("invalid dollar cap".into()))?;
            Ok((n * 1_000_000.0).ceil() as u64)
        };
        budgets.cost_micros = micros("per_run")?;
        budgets.cost_window = Some((
            cap["window"]
                .as_str()
                .and_then(crate::manifest::duration_ms)
                .ok_or_else(|| CoreError::Corrupt("invalid cost window".into()))?,
            micros("usd")?,
        ));
    }
    Ok(budgets)
}

impl RecoveredHosts {
    /// The supplied frames must come from RunLog recovery in the current epoch.
    /// Publish the registry only after every live run and accounting contribution
    /// has been rebuilt. No host is attached on a partial recovery failure.
    pub fn recover(
        store: Arc<Store>,
        log: Arc<dyn RecordLog>,
        watermark: Arc<dyn AdmissionLog>,
        frames: &[RecoveredRecord<Record>],
        budgets: Budgets,
        hooks: Arc<dyn Hooks>,
        clock: Clock,
    ) -> Result<Self> {
        let live = store.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT run_id FROM live_runs WHERE history_lost=0 ORDER BY admit_seq,run_id",
            )?;
            let ids = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ids.into_iter()
                .map(|id| {
                    let live = sql::live(conn, &id)?
                        .ok_or_else(|| CoreError::Corrupt("missing live run".into()))?;
                    let identity = sql::identity(conn, &id)?
                        .ok_or_else(|| CoreError::Corrupt("missing run identity".into()))?;
                    Ok((live, identity))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let registry = Arc::new(RunStore::new_bound(
            store.clone(),
            hooks.clone(),
            watermark.clone(),
        ));
        let epoch = watermark.synced_lsn().log_epoch;
        for (live, identity) in live {
            let run_budgets = budgets_for(&store, &live.run_id, &budgets)?;
            if live.admit_lsn.log_epoch != epoch {
                return Err(CoreError::Corrupt(
                    "live admission belongs to another log epoch".into(),
                ));
            }
            let frames: Vec<_> = frames
                .iter()
                .filter(|f| f.lsn >= live.admit_lsn.lsn && f.record.run_id == live.run_id)
                .map(|f| RecoveredRecord {
                    lsn: f.lsn,
                    record: f.record.clone(),
                })
                .collect();
            let mut records =
                Records::recover(live.run_id.clone(), live.admitted_at, log.clone(), &frames)?;
            records.bind_identity(identity);
            let rows = accounting::rows(records.state(), &run_budgets)?;
            registry.insert_accounted(
                live.run_id,
                live.flow_id,
                records.state().generation,
                records,
                rows,
            )?;
        }
        Ok(Self {
            store,
            registry,
            budgets,
            hooks,
            clock,
            watermark,
            log,
            decisions: std::sync::Mutex::new(None),
        })
    }

    pub fn registry(&self) -> &Arc<RunStore<Records>> {
        &self.registry
    }

    pub(crate) fn budgets_for(&self, id: &str) -> Result<Budgets> {
        budgets_for(&self.store, id, &self.budgets)
    }

    fn arrived(&self, run_id: &str) -> Result<()> {
        crate::run_store::lock_order::assert_lock_free();
        if self.hooks.at(run_id, &Boundary::HostArrived) == Step::Crash {
            self.store.cut();
            return Err(CoreError::Cut);
        }
        Ok(())
    }

    fn accept(
        &self,
        run_id: &str,
        position: u64,
        handle: &str,
        outcome: Option<&HostOutcome>,
        unknown_reason: Option<&str>,
    ) -> Result<CompletionAck> {
        if let Some(run) = self.registry.get(run_id) {
            match run.enter() {
                Ok(entrant) => {
                    let budgets = self.budgets_for(run_id)?;
                    let (ack, reason) = entrant.account(|records| {
                        records.check_healthy()?;
                        let state = records.state();
                        let call = state.calls.get(&position);
                        let reason = if !records.durable_intent(position) {
                            Some("unknown_call")
                        } else if call
                            .and_then(|c| c.accepted.as_deref())
                            .is_some_and(|h| h != handle)
                        {
                            Some("handle_mismatch")
                        } else {
                            None
                        };
                        if let Some(reason) = reason {
                            return Ok((
                                (CompletionAck::Quarantined, Some(reason)),
                                accounting::rows(state, &budgets)?,
                            ));
                        }
                        let call = call.expect("durable intent has a call");
                        if let Some(arrival) = &call.arrival {
                            let matches = outcome.is_none_or(|o| {
                                arrival.value_hash
                                    == crate::ids::payload_hash(o.settlement, &o.value)
                            });
                            return Ok((
                                (
                                    if matches {
                                        CompletionAck::Duplicate
                                    } else {
                                        CompletionAck::Quarantined
                                    },
                                    (!matches).then_some("contradicts_outcome"),
                                ),
                                accounting::rows(state, &budgets)?,
                            ));
                        }
                        if outcome.is_none() && call.unknown.is_some() {
                            return Ok((
                                (CompletionAck::Duplicate, None),
                                accounting::rows(state, &budgets)?,
                            ));
                        }
                        let generation = state.generation;
                        let reservation = call
                            .intent
                            .as_ref()
                            .and_then(|i| i.reservation.as_ref())
                            .cloned();
                        if call.accepted.is_none() {
                            records.append_pending(
                                generation,
                                Body::Accepted {
                                    position,
                                    handle: handle.to_owned(),
                                },
                            )?;
                        }
                        if let Some(outcome) = outcome {
                            let usage = outcome.usage.map(|u| {
                                let input = u
                                    .input_tokens
                                    .unwrap_or(0)
                                    .saturating_add(u.cache_write_tokens.unwrap_or(0));
                                let output = u.output_tokens.unwrap_or(0);
                                let reserved = reservation
                                    .as_ref()
                                    .map_or(0, |r| r.input_tokens.saturating_add(r.output_tokens));
                                let missing = u.input_tokens.is_none()
                                    || u.cache_write_tokens.is_none()
                                    || u.output_tokens.is_none();
                                Usage {
                                    input_tokens: if missing {
                                        input.max(reserved.saturating_sub(output))
                                    } else {
                                        input
                                    },
                                    output_tokens: output,
                                }
                            });
                            let value: serde_json::Value =
                                serde_json::from_str(outcome.value.as_str())
                                    .map_err(|e| CoreError::Invalid(e.to_string()))?;
                            let cost = reservation
                                .as_ref()
                                .filter(|r| {
                                    r.cost_micros > 0
                                        || r.input_tokens.saturating_add(r.output_tokens) == 0
                                })
                                .map(|r| {
                                    (crate::classify_cost::charge(
                                        r.cost_micros as f64 / 1_000_000.0,
                                        Some(&value),
                                        None,
                                    ) * 1_000_000.0)
                                        .ceil() as u64
                                });
                            records.append(
                                generation,
                                Body::Arrived(Arrival {
                                    position,
                                    arrival_seq: records.state().next_arrival_seq,
                                    outcome: if outcome.settlement
                                        == basal_proto::Settlement::Fulfilled
                                    {
                                        "fulfilled"
                                    } else {
                                        "rejected"
                                    }
                                    .into(),
                                    settlement: outcome.settlement,
                                    value: outcome.value.clone(),
                                    value_hash: crate::ids::payload_hash(
                                        outcome.settlement,
                                        &outcome.value,
                                    ),
                                    handle: Some(handle.to_owned()),
                                    usage,
                                    cost_micros: cost,
                                    kv_effect: None,
                                    durability: ArrivalDurability {
                                        host_callback: true,
                                        ..Default::default()
                                    },
                                }),
                            )?;
                        } else {
                            records.append(
                                generation,
                                Body::Unknown {
                                    position,
                                    reason: unknown_reason
                                        .unwrap_or("host outcome unavailable")
                                        .into(),
                                },
                            )?;
                        }
                        if records.state().parked {
                            records.append(generation, Body::Woken)?;
                        }
                        if let Some(identity) = records.identity()
                            && self.hooks.at_run(
                                run_id,
                                &identity.flow_id,
                                &Boundary::HostArrived,
                                Some(position),
                                records.state(),
                            ) == Step::Crash
                        {
                            self.store.cut();
                            return Err(CoreError::Cut);
                        }
                        Ok((
                            (CompletionAck::Accepted, None),
                            accounting::rows(records.state(), &budgets)?,
                        ))
                    })?;
                    drop(entrant);
                    if let Some(reason) = reason {
                        self.quarantine(run_id, position, handle, outcome, reason)?;
                    }
                    if ack == CompletionAck::Accepted {
                        self.arrived(run_id)?;
                    }
                    return Ok(ack);
                }
                Err(Routing::Closing) => {
                    return Err(CoreError::Unavailable(
                        "run is closing; redeliver completion".into(),
                    ));
                }
                Err(Routing::Closed) => {}
                Err(Routing::Open) => unreachable!(),
            }
        }
        self.closed(run_id, position, handle, outcome)
    }
}

impl RecoveredHosts {
    fn closed(
        &self,
        run_id: &str,
        position: u64,
        handle: &str,
        outcome: Option<&HostOutcome>,
    ) -> Result<CompletionAck> {
        let flow = self
            .store
            .read(|conn| Ok(sql::identity(conn, run_id)?.map(|i| i.flow_id)))?;
        let Some(flow) = flow else {
            return Err(CoreError::Unavailable(
                "run identity not visible; redeliver completion".into(),
            ));
        };
        let now = self.clock.now_ms();
        let receipt = outcome.map_or(Receipt::Unknown, |o| {
            Receipt::Value(crate::ids::payload_hash(o.settlement, &o.value))
        });
        let result = self.registry.accounting_write(&flow, &self.store, |tx| {
            let result = sql::settle_obligation(tx, run_id, position, Some(handle), receipt, now)?;
            match result {
                SettleResult::Settled => {
                    if let Some(outcome) = outcome {
                        crate::tokens::settle_outcome(tx, run_id, position, outcome, now)?;
                        let value = serde_json::from_str(outcome.value.as_str())
                            .map_err(|e| CoreError::Invalid(e.to_string()))?;
                        crate::classify_cost::settle(
                            tx,
                            "flow",
                            run_id,
                            position,
                            Some(&value),
                            None,
                        )?;
                    } else {
                        crate::tokens::settle(
                            tx,
                            crate::tokens::Key::Call { run_id, position },
                            crate::tokens::Report::Unreported,
                            now,
                        )?;
                        crate::classify_cost::settle(tx, "flow", run_id, position, None, None)?;
                    }
                }
                SettleResult::Missing if sql::history(tx, run_id)?.is_none() => {
                    return Err(CoreError::Unavailable(
                        "run is closing; obligation not visible; redeliver completion".into(),
                    ));
                }
                _ => {}
            }
            if matches!(result, SettleResult::Conflict | SettleResult::Missing) {
                self.quarantine_in(
                    tx,
                    run_id,
                    position,
                    handle,
                    outcome,
                    if result == SettleResult::Missing {
                        "unknown_call"
                    } else {
                        "contradicts_outcome"
                    },
                )?;
            }
            Ok(result)
        })?;
        Ok(match result {
            SettleResult::Settled => {
                self.arrived(run_id)?;
                CompletionAck::Accepted
            }
            SettleResult::Duplicate => CompletionAck::Duplicate,
            SettleResult::Conflict | SettleResult::Missing => CompletionAck::Quarantined,
        })
    }

    fn quarantine(
        &self,
        run_id: &str,
        position: u64,
        handle: &str,
        outcome: Option<&HostOutcome>,
        reason: &str,
    ) -> Result<()> {
        self.store
            .write(|tx| self.quarantine_in(tx, run_id, position, handle, outcome, reason))
    }

    fn quarantine_in(
        &self,
        tx: &rusqlite::Transaction<'_>,
        run_id: &str,
        position: u64,
        handle: &str,
        outcome: Option<&HostOutcome>,
        reason: &str,
    ) -> Result<()> {
        let unknown = HostOutcome::rejected(basal_proto::JsonText::null());
        let outcome = outcome.unwrap_or(&unknown);
        tx.execute("INSERT OR IGNORE INTO quarantine(run_id,position,handle,settlement,value,payload_hash,reason,at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![run_id, crate::model::position_i64(position)?, handle,
                if outcome.settlement == basal_proto::Settlement::Fulfilled { "fulfilled" } else { "rejected" },
                outcome.value.as_str(), crate::ids::payload_hash(outcome.settlement, &outcome.value).as_slice(), reason, self.clock.now_ms()])?;
        tracing::warn!(
            run_id,
            position,
            reason,
            "host completion quarantined; operator review required"
        );
        Ok(())
    }
}

impl CompletionSink for RecoveredHosts {
    fn complete(&self, completion: &Completion) -> std::result::Result<CompletionAck, SinkError> {
        self.accept(
            &completion.run_id,
            completion.position,
            &completion.handle,
            Some(&completion.outcome),
            None,
        )
        .map_err(|e| SinkError(e.to_string()))
    }
    fn unknown(&self, unknown: &UnknownOutcome) -> std::result::Result<CompletionAck, SinkError> {
        self.accept(
            &unknown.run_id,
            unknown.position,
            &unknown.handle,
            None,
            Some(&format!("{}: {}", unknown.reason.as_str(), unknown.detail)),
        )
        .map_err(|e| SinkError(e.to_string()))
    }
}

#[cfg(test)]
#[path = "run_host_tests.rs"]
pub(crate) mod tests;
