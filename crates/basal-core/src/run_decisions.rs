//! Decisions for log-backed RunStore actors, rather than SQLite journal rows.
//! Snapshot actors before SQLite; never take
//! an actor from a SQLite closure. Effective answers serialize under the actor,
//! sync before the card CAS, and run cancellation through the terminal committer.
//!
//! | Entry | Lock order | Lock-free work |
//! | --- | --- | --- |
//! | Due cards | Actor snapshots; SQLite; actor revalidation, separately | Card raising |
//! | Live answer | Actor → accounting → SQLite; sync before SQLite | Worker stop and slot release during close |
//! | Closed answer | Accounting → SQLite | Return to caller after commit |
//! | Lost-history close and retrigger | Separate SQLite transactions | Admission after close |

use crate::{
    CoreError, Result,
    decisions::{self, Answered, CardState, DecisionRecord},
    model::RunState,
    reconcile::Resolution,
    run_driver::{Terminal, TerminalEffects},
    run_host::RecoveredHosts,
    run_records::{Arrival, ArrivalDurability, Body, State, Usage},
    run_store::{CloseDriver, TerminalWrite},
    run_store_sql::{self as sql, Receipt, SettleResult},
};
use basal_host::{DecisionAnswer, DecisionContext, DecisionKind, DecisionPath};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct CloseServices {
    driver: Arc<dyn CloseDriver>,
    effects: Arc<dyn TerminalEffects>,
}

#[derive(Clone)]
struct Snapshot {
    id: String,
    state: State,
    generation: u64,
    revision: crate::run_log::Lsn,
}

impl RecoveredHosts {
    /// Bind CloseDriver and TerminalEffects before host attachment or Cancel
    /// answers. They must be the adapters used by the owning Driver, so close
    /// stops its worker and releases its slot. Durable cancellations resume here.
    pub fn configure_decisions(
        &self,
        driver: Arc<dyn CloseDriver>,
        effects: Arc<dyn TerminalEffects>,
    ) -> Result<()> {
        *self.decisions.lock().unwrap() = Some(CloseServices { driver, effects });
        for snapshot in self.decision_snapshots()? {
            for call in snapshot.state.calls.values() {
                if let Some(resolution) = call.resolutions.last()
                    && resolution.choice == "cancel"
                {
                    let card = self.store.read(decisions::all)?.into_iter().find(|c| {
                        c.state == CardState::Open
                            && c.run_id.as_deref() == Some(&snapshot.id)
                            && c.position == Some(resolution.position)
                            && c.instance == resolution.attempts
                    });
                    self.finish_cancel(
                        &snapshot.id,
                        resolution.position,
                        card.as_ref(),
                        decisions::OPERATOR_ACTOR,
                        self.clock.now_ms(),
                    )?;
                    break;
                }
            }
        }
        Ok(())
    }

    fn decision_snapshots(&self) -> Result<Vec<Snapshot>> {
        let mut snapshots = Vec::new();
        for run in self.registry().snapshot() {
            if let Ok(entrant) = run.enter() {
                snapshots.push(entrant.inspect(|generation, r| {
                    Ok(Snapshot {
                        id: r.run_id().into(),
                        state: r.state().clone(),
                        generation,
                        revision: r.revision(),
                    })
                })?);
            }
        }
        Ok(snapshots)
    }

    pub fn decisions_due(&self, now: i64) -> Result<Vec<DecisionRecord>> {
        self.due_after(now, || {})
    }

    #[cfg(test)]
    pub(crate) fn decisions_due_interleave(
        &self,
        now: i64,
        after: impl FnOnce(),
    ) -> Result<Vec<DecisionRecord>> {
        self.due_after(now, after)
    }

    fn due_after(&self, now: i64, after_transaction: impl FnOnce()) -> Result<Vec<DecisionRecord>> {
        let before = self.decision_snapshots()?;
        let candidates = self.store.write(|tx| {
            for snapshot in &before {
                refresh(tx, snapshot, now)?;
            }
            refresh_closed(tx, now)?;
            let cards = decisions::all(tx)?;
            // A synced resolution can outlive the card commit. Finish that
            // commit on recovery rather than asking the operator a second time.
            for card in &cards {
                if card.kind != DecisionKind::Reconcile || card.state != CardState::Open {
                    continue;
                }
                if let Some(snapshot) = before.iter().find(|s| Some(&s.id) == card.run_id.as_ref())
                    && let Some(call) = card.position.and_then(|p| snapshot.state.calls.get(&p))
                    && let Some(resolution) = call.resolutions.iter().find(|r| {
                        r.attempts == card.instance
                            && r.choice != "cancel"
                            && !r.elicitation_id.is_empty()
                            && card
                                .elicitation_id
                                .as_ref()
                                .is_none_or(|id| id == &r.elicitation_id)
                    })
                {
                    cas(tx, card, CardState::Applied, Some(&resolution.choice), now)?;
                }
            }
            Ok(decisions::all(tx)?
                .into_iter()
                .filter(|c| {
                    c.state == CardState::Open && c.raised_revision.is_none_or(|r| r < c.revision)
                })
                .collect::<Vec<_>>())
        })?;
        after_transaction();
        let after = self.decision_snapshots()?;
        let mut due = Vec::new();
        for card in candidates {
            if card.kind == DecisionKind::Reconcile {
                let old = before.iter().find(|s| Some(&s.id) == card.run_id.as_ref());
                let new = after.iter().find(|s| Some(&s.id) == card.run_id.as_ref());
                if let (Some(old), Some(new)) = (old, new) {
                    // Generation fences claims; the LSN also fences callbacks
                    // and other state changes within the same activation.
                    if old.generation == new.generation
                        && old.revision == new.revision
                        && stands(new, &card)
                    {
                        due.push(card);
                    }
                } else if old.is_none()
                    && new.is_none()
                    && self.store.read(|c| closed_stands(c, &card))?
                {
                    due.push(card);
                }
            } else if self.store.read(|c| decisions::stands(c, &card))? {
                due.push(card);
            }
        }
        Ok(due)
    }

    pub fn answer_decision(
        &self,
        path: &DecisionPath,
        answer: &DecisionAnswer,
        now: i64,
    ) -> Result<(Answered, Option<String>)> {
        if answer.decision != DecisionKind::Reconcile {
            return self
                .store
                .write(|tx| decisions::answer_on(tx, path, answer, now));
        }
        let Some(mut card) = self.store.read(|c| decisions::find(c, path, answer))? else {
            return Ok((Answered::NoSuchCard, None));
        };
        if card.kind != answer.decision || card.flow_id != answer.flow_id {
            return Ok((Answered::NoSuchCard, None));
        }
        if card.state != CardState::Open {
            return Ok((Answered::AlreadyAnswered(card.state), None));
        }
        if card
            .elicitation_id
            .as_deref()
            .is_some_and(|id| id != answer.elicitation_id)
        {
            return Ok((Answered::Superseded, None));
        }
        let id = card
            .run_id
            .as_deref()
            .ok_or_else(|| CoreError::Corrupt("reconcile card without run".into()))?;
        if card.elicitation_id.is_none() {
            card.elicitation_id = Some(answer.elicitation_id.clone());
        }
        let position = card
            .position
            .ok_or_else(|| CoreError::Corrupt("reconcile card without position".into()))?;
        let choice = answer.choice.as_deref();
        if choice.is_none() || choice == Some(decisions::LEAVE) {
            let state = if choice.is_none() {
                CardState::Expired
            } else {
                CardState::Declined
            };
            let apply = || {
                self.store.write(|tx| {
                    let changed = cas(tx, &card, state, choice, now)?;
                    if changed {
                        crate::reconcile::audit_answer(
                            tx,
                            crate::decisions::OPERATOR_ACTOR,
                            if choice.is_none() {
                                "decision.expired"
                            } else {
                                "decision.declined"
                            },
                            Some(id),
                            Some(position),
                            "",
                            Some(&answer.elicitation_id),
                        )?;
                    }
                    Ok(changed)
                })
            };
            // Leave has no log record, but shares the answer's actor serialization
            // so it cannot win the card CAS halfway through an effective answer.
            let changed =
                if let Some(run) = self.registry().get(id) {
                    let entrant = run
                        .enter()
                        .map_err(|_| CoreError::Unavailable("run is closing".into()))?;
                    entrant.finish(|r| {
                        if r.state().calls.get(&position).is_some_and(|c| {
                            c.resolutions.iter().any(|r| r.attempts == card.instance)
                        }) {
                            return Ok(false);
                        }
                        apply()
                    })?
                } else {
                    apply()?
                };
            return Ok((
                if changed {
                    Answered::Now(state)
                } else {
                    Answered::Superseded
                },
                None,
            ));
        }
        let resolution = match choice {
            Some(decisions::NOT_APPLIED) => Resolution::NotApplied,
            Some(decisions::APPLIED) => Resolution::ReconciledAsApplied,
            Some(decisions::CANCEL) => Resolution::Cancel,
            _ => return Err(CoreError::Invalid("unknown reconcile choice".into())),
        };
        match self.resolve(
            id,
            position,
            resolution,
            crate::decisions::OPERATOR_ACTOR,
            Some(&card),
            now,
        ) {
            Ok(state) => Ok((
                Answered::Now(CardState::Applied),
                (state == RunState::Pending).then(|| id.into()),
            )),
            Err(CoreError::Invalid(detail))
                if detail == "attempt is no longer unknown"
                    || detail == "closed runs accept only observed_result" =>
            {
                self.store.write(|tx| {
                    let changed = cas(tx, &card, CardState::Stale, choice, now)?;
                    if changed {
                        crate::reconcile::audit_answer(
                            tx,
                            crate::decisions::OPERATOR_ACTOR,
                            "decision.stale",
                            Some(id),
                            Some(position),
                            "",
                            Some(&answer.elicitation_id),
                        )?;
                    }
                    Ok((
                        if changed {
                            Answered::Now(CardState::Stale)
                        } else {
                            Answered::Superseded
                        },
                        None,
                    ))
                })
            }
            Err(CoreError::Invalid(_)) => Ok((Answered::Superseded, None)),
            Err(e) => Err(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        &self,
        id: &str,
        position: u64,
        resolution: Resolution,
        actor: &str,
        card: Option<&DecisionRecord>,
        now: i64,
    ) -> Result<RunState> {
        let Some(run) = self
            .registry()
            .get(id)
            .filter(|run| run.routing() != crate::run_store::Routing::Closed)
        else {
            return self.resolve_closed(id, position, resolution, actor, card, now);
        };
        let budgets = self.budgets_for(id)?;
        let close = if resolution == Resolution::Cancel {
            Some(self.decisions.lock().unwrap().clone().ok_or_else(|| {
                CoreError::Unavailable("staged close services are not bound".into())
            })?)
        } else {
            None
        };
        let entrant = run
            .enter()
            .map_err(|_| CoreError::Unavailable("run is closing".into()))?;
        let after = entrant.account(|records| {
            let state = records.state();
            let call = state
                .calls
                .get(&position)
                .ok_or_else(|| CoreError::Invalid("no such unknown call".into()))?;
            let intent = call
                .intent
                .as_ref()
                .ok_or_else(|| CoreError::Invalid("unknown call has no intent".into()))?;
            if let Some(card) = card
                && (card.instance != intent.attempts || !self.store.read(|c| card_open(c, card))?)
            {
                return Err(CoreError::Invalid("superseded decision attempt".into()));
            }
            if call.unknown.is_none() || call.arrival.is_some() {
                return Err(CoreError::Invalid("attempt is no longer unknown".into()));
            }
            if call
                .resolutions
                .last()
                .is_some_and(|r| r.attempts == intent.attempts && r.choice == "cancel")
            {
                return Err(CoreError::Invalid("attempt already cancelled".into()));
            }
            let outcome = match &resolution {
                Resolution::ObservedResult(o) => Some(o.clone()),
                Resolution::ReconciledAsApplied => Some(crate::reconcile::reconciled_as_applied()),
                _ => None,
            };
            let choice = match resolution {
                Resolution::ObservedResult(_) => "observed_result",
                Resolution::ReconciledAsApplied => "applied",
                Resolution::NotApplied => "not_applied",
                Resolution::Cancel => "cancel",
            };
            let usage = outcome.as_ref().and_then(|o| o.usage).map(|u| {
                let input = u
                    .input_tokens
                    .unwrap_or(0)
                    .saturating_add(u.cache_write_tokens.unwrap_or(0));
                let output = u.output_tokens.unwrap_or(0);
                let reserved = intent
                    .reservation
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
            let cost = outcome
                .as_ref()
                .and_then(|o| serde_json::from_str::<Value>(o.value.as_str()).ok())
                .and_then(|v| v.get("structuredContent").unwrap_or(&v)["cost_usd"].as_f64())
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| (n * 1_000_000.0).ceil() as u64);
            let arrival = outcome.map(|o| Arrival {
                position,
                arrival_seq: state.next_arrival_seq,
                outcome: if o.settlement == basal_proto::Settlement::Fulfilled {
                    "fulfilled".into()
                } else {
                    "rejected".into()
                },
                value_hash: crate::ids::payload_hash(o.settlement, &o.value),
                settlement: o.settlement,
                value: o.value,
                handle: call.accepted.clone(),
                usage: usage.clone(),
                cost_micros: cost,
                kv_effect: None,
                durability: ArrivalDurability::default(),
            });
            let record = crate::run_records::Resolution {
                position,
                attempts: intent.attempts,
                idempotency_key: intent.idempotency_key.clone(),
                choice: choice.into(),
                observed_outcome: arrival,
                usage,
                elicitation_id: card
                    .and_then(|c| c.elicitation_id.clone())
                    .unwrap_or_default(),
            };
            records.append(state.generation, Body::Resolution(record))?;
            if resolution != Resolution::Cancel {
                self.store.write(|tx| {
                    crate::reconcile::audit_answer(
                        tx,
                        actor,
                        &format!("reconcile.{choice}"),
                        Some(id),
                        Some(position),
                        "",
                        card.and_then(|c| c.elicitation_id.as_deref()),
                    )?;
                    if let Some(card) = card {
                        cas(tx, card, CardState::Applied, Some(choice), now)?;
                    }
                    Ok(())
                })?;
            }
            let pending = records
                .state()
                .calls
                .values()
                .all(|c| c.unknown.is_none() || c.arrival.is_some());
            let rows = crate::run_driver::accounting::rows(records.state(), &budgets)?;
            Ok((
                if pending {
                    RunState::Pending
                } else {
                    RunState::NeedsReconcile
                },
                rows,
            ))
        })?;
        drop(entrant);
        if close.is_some() {
            self.finish_cancel(id, position, card, actor, now)?;
            return Ok(RunState::Cancelled);
        }
        Ok(after)
    }

    fn finish_cancel(
        &self,
        id: &str,
        position: u64,
        card: Option<&DecisionRecord>,
        actor: &str,
        now: i64,
    ) -> Result<()> {
        let close =
            self.decisions.lock().unwrap().clone().ok_or_else(|| {
                CoreError::Unavailable("staged close services are not bound".into())
            })?;
        let run = self
            .registry()
            .get(id)
            .ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
        let live = self
            .store
            .read(|c| sql::live(c, id))?
            .ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
        let effects = Arc::new(CancelEffects {
            inner: close.effects,
            card: card.cloned(),
            actor: actor.into(),
            id: id.into(),
            position,
            now,
        });
        let terminal = Terminal::new(
            live,
            RunState::Cancelled,
            now,
            self.budgets_for(id)?,
            effects,
        );
        self.registry()
            .close(&run, &terminal, close.driver.as_ref())
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_closed(
        &self,
        id: &str,
        position: u64,
        resolution: Resolution,
        actor: &str,
        card: Option<&DecisionRecord>,
        now: i64,
    ) -> Result<RunState> {
        let Resolution::ObservedResult(outcome) = resolution else {
            return Err(CoreError::Invalid(
                "closed runs accept only observed_result".into(),
            ));
        };
        let history = self
            .store
            .read(|c| sql::history(c, id))?
            .ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
        self.registry()
            .accounting_write(&history.flow_id, &self.store, |tx| {
                let row = sql::obligation(tx, id, position)?
                    .ok_or_else(|| CoreError::Invalid("no open obligation".into()))?;
                if row.state != sql::ObligationState::Unknown {
                    return Err(CoreError::Invalid("obligation is no longer unknown".into()));
                }
                if let Some(card) = card
                    && (!closed_stands(tx, card)? || !card_open(tx, card)?)
                {
                    return Err(CoreError::Invalid("superseded obligation attempt".into()));
                }
                let receipt =
                    Receipt::Value(crate::ids::payload_hash(outcome.settlement, &outcome.value));
                if sql::settle_obligation(tx, id, position, row.handle.as_deref(), receipt, now)?
                    != SettleResult::Settled
                {
                    return Err(CoreError::Invalid("obligation changed".into()));
                }
                crate::tokens::settle_outcome(tx, id, position, &outcome, now)?;
                let value: Value = serde_json::from_str(outcome.value.as_str())
                    .map_err(|e| CoreError::Invalid(e.to_string()))?;
                crate::classify_cost::settle(tx, "flow", id, position, Some(&value), None)?;
                crate::reconcile::audit_answer(
                    tx,
                    actor,
                    "reconcile.observed_result",
                    Some(id),
                    Some(position),
                    "",
                    card.and_then(|c| c.elicitation_id.as_deref()),
                )?;
                if let Some(card) = card {
                    cas(tx, card, CardState::Applied, Some("observed_result"), now)?;
                }
                sql::advance_log_hwm(tx, self.watermark.synced_lsn())?;
                Ok(())
            })?;
        Ok(history.state)
    }
}

fn stands(snapshot: &Snapshot, card: &DecisionRecord) -> bool {
    card.position
        .and_then(|p| snapshot.state.calls.get(&p))
        .is_some_and(|c| {
            c.unknown.is_some()
                && c.arrival.is_none()
                && c.intent
                    .as_ref()
                    .is_some_and(|i| i.attempts == card.instance)
        })
}
fn closed_stands(conn: &rusqlite::Connection, card: &DecisionRecord) -> Result<bool> {
    let (Some(id), Some(position)) = (&card.run_id, card.position) else {
        return Ok(false);
    };
    Ok(sql::obligation(conn, id, position)?.is_some_and(|o| {
        o.state == sql::ObligationState::Unknown
            && o.detail.pointer("/intent/attempts").and_then(Value::as_u64) == Some(card.instance)
    }))
}
fn card_open(conn: &rusqlite::Connection, card: &DecisionRecord) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM decision_cards WHERE dedup_key=?1 AND instance=?2 AND revision=?3 AND state='open')", params![card.dedup_key, card.instance, card.revision], |r| r.get(0))?)
}
fn cas(
    tx: &Transaction<'_>,
    card: &DecisionRecord,
    state: CardState,
    choice: Option<&str>,
    now: i64,
) -> Result<bool> {
    Ok(tx.execute("UPDATE decision_cards SET state=?3,choice=?4,answered_at=?5,elicitation_id=COALESCE(elicitation_id,?7) WHERE dedup_key=?1 AND instance=?2 AND revision=?6 AND state='open'", params![card.dedup_key, card.instance, state.as_str(), choice, now, card.revision, card.elicitation_id])? == 1)
}
fn refresh(tx: &Transaction<'_>, snapshot: &Snapshot, now: i64) -> Result<()> {
    let Some(live) = sql::live(tx, &snapshot.id)? else {
        return Ok(());
    };
    for call in snapshot
        .state
        .calls
        .values()
        .filter(|c| c.unknown.is_some() && c.arrival.is_none())
    {
        let Some(intent) = &call.intent else {
            continue;
        };
        let reason = basal_host::UnknownReason::parse(call.unknown.as_deref().unwrap())
            .ok_or_else(|| CoreError::Corrupt("invalid unknown reason".into()))?;
        let context = DecisionContext::Reconcile {
            run_id: snapshot.id.clone(),
            run_admitted_at_ms: live.admitted_at,
            step: None,
            call_key: intent.idempotency_key.clone().unwrap_or_default(),
            op: basal_host::op_label(&call.identity.kind),
            attempts: u32::try_from(intent.attempts)
                .map_err(|_| CoreError::Corrupt("attempt count overflow".into()))?,
            unknown_reason: reason,
        };
        let mut body = decisions::context_json(&context);
        body["args_digest"] = json!(
            call.identity
                .args_digest
                .0
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        upsert(
            tx,
            &live.flow_id,
            live.flow_version,
            &snapshot.id,
            call.identity.position,
            intent.attempts,
            intent.idempotency_key.as_deref(),
            body,
            now,
        )?;
    }
    Ok(())
}

fn refresh_closed(tx: &Transaction<'_>, now: i64) -> Result<()> {
    let mut stmt = tx.prepare("SELECT run_id,detail FROM obligations WHERE state='unknown'")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, detail) in rows {
        // A history-lost Broca snapshot has no call identity and cannot produce
        // a per-call card. Its run-level diagnostic is the operator's entry point.
        let Ok(call) = serde_json::from_str::<crate::run_records::CallState>(&detail) else {
            continue;
        };
        let Some(intent) = &call.intent else {
            continue;
        };
        let history = sql::history(tx, &id)?
            .ok_or_else(|| CoreError::Corrupt("obligation without history".into()))?;
        let reason =
            basal_host::UnknownReason::parse(call.unknown.as_deref().unwrap_or("basal_restarted"))
                .ok_or_else(|| CoreError::Corrupt("invalid unknown reason".into()))?;
        let context = DecisionContext::Reconcile {
            run_id: id.clone(),
            run_admitted_at_ms: history.admitted_at,
            step: None,
            call_key: intent.idempotency_key.clone().unwrap_or_default(),
            op: basal_host::op_label(&call.identity.kind),
            attempts: u32::try_from(intent.attempts)
                .map_err(|_| CoreError::Corrupt("attempt count overflow".into()))?,
            unknown_reason: reason,
        };
        upsert(
            tx,
            &history.flow_id,
            history.flow_version,
            &id,
            call.identity.position,
            intent.attempts,
            intent.idempotency_key.as_deref(),
            decisions::context_json(&context),
            now,
        )?;
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn upsert(
    tx: &Transaction<'_>,
    flow: &str,
    version: u32,
    id: &str,
    position: u64,
    instance: u64,
    key: Option<&str>,
    body: Value,
    now: i64,
) -> Result<()> {
    let dedup = decisions::reconcile_key(flow, id, position);
    tx.execute("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,run_id,position,call_key,instance,card,state,created_at)
        SELECT ?1,'reconcile',?2,?3,?4,?5,?6,?7,?8,'open',?9
        WHERE NOT EXISTS(SELECT 1 FROM decision_cards WHERE dedup_key=?1 AND instance=?7 AND state<>'open')
        ON CONFLICT(dedup_key) WHERE state='open' DO UPDATE SET instance=excluded.instance,card=excluded.card,revision=revision+1
        WHERE instance<>excluded.instance OR card<>excluded.card", params![dedup, flow, version, id, position, key.unwrap_or(""), instance, body.to_string(), now])?;
    Ok(())
}

struct CancelEffects {
    inner: Arc<dyn TerminalEffects>,
    card: Option<DecisionRecord>,
    actor: String,
    id: String,
    position: u64,
    now: i64,
}
struct CancelWrite {
    inner: Box<dyn TerminalWrite>,
    card: Option<DecisionRecord>,
    actor: String,
    id: String,
    position: u64,
    now: i64,
}
impl TerminalEffects for CancelEffects {
    fn capture(
        &self,
        live: &sql::LiveRun,
        state: &State,
        terminal: RunState,
    ) -> Result<Box<dyn TerminalWrite>> {
        Ok(Box::new(CancelWrite {
            inner: self.inner.capture(live, state, terminal)?,
            card: self.card.clone(),
            actor: self.actor.clone(),
            id: self.id.clone(),
            position: self.position,
            now: self.now,
        }))
    }
}
impl TerminalWrite for CancelWrite {
    fn apply(&self, tx: &Transaction<'_>) -> Result<()> {
        self.inner.apply(tx)?;
        if let Some(card) = &self.card {
            cas(tx, card, CardState::Applied, Some("cancel"), self.now)?;
        }
        crate::reconcile::audit_answer(
            tx,
            &self.actor,
            "reconcile.cancel",
            Some(&self.id),
            Some(self.position),
            "",
            self.card.as_ref().and_then(|c| c.elicitation_id.as_deref()),
        )
    }
}

impl RecoveredHosts {
    /// The log is unavailable, so this close exports admission and saved host
    /// snapshots only. Zero counters mean unavailable, not measured zero usage.
    pub fn cancel_history_lost(&self, id: &str, actor: &str, now: i64) -> Result<()> {
        self.store.write(|tx| {
            if !sql::history_lost(tx, id)? { return Err(CoreError::Invalid("run-level reconcile requires history_lost".into())); }
            let live = sql::live(tx, id)?.ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
            let history = sql::RunHistory { run_id: id.into(), flow_id: live.flow_id.clone(), flow_version: live.flow_version,
                trigger_id: live.trigger_id.clone(), trigger_kind: live.trigger_kind, trigger: live.trigger,
                admitted_at: live.admitted_at, admit_seq: live.admit_seq, first_claim_at: None, ended_at: now,
                state: RunState::Cancelled, error_kind: Some("history_lost".into()),
                error_detail: Some("Run log lost during restore; call detail and unfolded accounting are unavailable. Flow windows may under-count once.".into()),
                result: None, result_size: 0, call_counts: json!({"history":"lost","detail":"unavailable","accounting":"unavailable"}),
                writes_sent: 0, tokens_charged: 0, cost_microusd: 0, broken: 0, activation_count: 0,
                script_started: false, phase: "history_lost".into(), deadline_exempt: false, history_lost: true, commit_seq: sql::next_commit_seq(tx)? };
            sql::finish_live(tx, &history)?;
            let snapshots = {
                let mut stmt = tx.prepare("SELECT position,snapshot FROM broca_calls WHERE run_id=?1 ORDER BY position")?;
                stmt.query_map([id], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?
            };
            for (position, snapshot) in snapshots {
                let detail: Value = serde_json::from_str(&snapshot).map_err(|e| CoreError::Corrupt(format!("Broca snapshot: {e}")))?;
                let handle = detail.get("handle").and_then(Value::as_str).map(str::to_owned);
                let receipt = if detail["acknowledged"] == true {
                    if let Some(value) = detail.pointer("/outcome/value").and_then(Value::as_str) {
                        let text = basal_proto::JsonText::new(value).map_err(|e| CoreError::Corrupt(e.to_string()))?;
                        let settlement = if detail.pointer("/outcome/rejected") == Some(&Value::Bool(true)) { basal_proto::Settlement::Rejected } else { basal_proto::Settlement::Fulfilled };
                        Some(Receipt::Value(crate::ids::payload_hash(settlement, &text)))
                    } else if detail["unknown"].is_string() { Some(Receipt::Unknown) } else { None }
                } else { None };
                sql::insert_obligation(tx, &sql::Obligation { run_id: id.into(), position, handle,
                    state: if receipt.is_some() { sql::ObligationState::Settled } else if detail["unknown"].is_string() { sql::ObligationState::Unknown } else { sql::ObligationState::Open },
                    audit_at: now, detail, receipt, settled_at: receipt.map(|_| now) })?;
            }
            tx.execute("INSERT OR IGNORE INTO tombstones(kind,key,run_id,pruned_at) VALUES('trigger',?1,?2,?3)", params![crate::admission::trigger_key(&live.flow_id, &live.trigger_id), id, now])?;
            sql::advance_log_hwm(tx, self.watermark.synced_lsn())?;
            crate::reconcile::audit(tx, actor, "reconcile.history_lost.cancel", Some(id), None, "call detail unavailable; accounting not folded")
        })
    }
}

impl crate::Runtime {
    /// Run-level reconciliation is deliberately distinct from resolving a call:
    /// the lost log no longer supplies call identities or replayable outcomes.
    pub fn reconcile_history_lost(
        &self,
        id: &str,
        retrigger: bool,
        actor: &str,
    ) -> Result<Option<crate::admission::Admission>> {
        let hosts = self.shared.staged_hosts.as_ref().ok_or_else(|| {
            CoreError::Invalid("run-level reconcile requires history_lost".into())
        })?;
        hosts.cancel_history_lost(id, actor, self.config().clock.now_ms())?;
        self.shared.signal.bump();
        if retrigger {
            // Closing and admission are separate transactions: the old slot is
            // already released before the ordinary gate examines the retry.
            let result = self.staged_retrigger(id)?;
            Ok(Some(result))
        } else {
            Ok(None)
        }
    }
}
