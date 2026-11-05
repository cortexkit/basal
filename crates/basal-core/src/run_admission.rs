//! Staged admission into the run-log control plane.
//!
//! No production constructor selects this path yet. Every field the old
//! admission wrote has a home in `live_runs` or `run_identities`; the
//! `AdmissionLog` and `EventWakes` traits are the seams later stages bind.

use std::collections::BTreeSet;
use std::sync::Arc;

use basal_proto::JsonText;
use jiff::Timestamp;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::admission::{self, Admission, AdmitContext, TriggerSpec};
use crate::events::{self, HeaderMatch, Notice};
use crate::run_store_sql::{self as sql, EpochLsn, LiveRun, RunIdentity};
use crate::schedule::{self, SchedulerConfig, TickReport};
use crate::{Boundary, CoreError, Hooks, Result, Step, Store, install};

/// A read of the already-published watermark, never a request to sync the log.
/// Implementations must not wait for log I/O or acquire the SQLite connection.
/// The log binding supplies the epoch together with the writer's `synced_lsn`.
pub trait AdmissionLog: Send + Sync {
    fn synced_lsn(&self) -> EpochLsn;
}

/// Host integration owns model wake matching; admission owns its transaction.
/// `committed` runs without the SQLite connection, before the reader may ack.
pub trait EventWakes {
    fn wake(&self, tx: &Transaction<'_>, notice: &Notice, now: i64) -> Result<usize>;
    fn committed(&self);
}

pub struct RunAdmission {
    store: Arc<Store>,
    log: Arc<dyn AdmissionLog>,
    hooks: Arc<dyn Hooks>,
}

impl RunAdmission {
    /// Construct log-backed admission explicitly. Production keeps using the
    /// SQLite execution journal until the run-log schema is enabled.
    pub fn new_staged(
        store: Arc<Store>,
        log: Arc<dyn AdmissionLog>,
        hooks: Arc<dyn Hooks>,
    ) -> Self {
        Self { store, log, hooks }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_for_test(
        store: Arc<Store>,
        log: Arc<dyn AdmissionLog>,
        hooks: Arc<dyn Hooks>,
    ) -> Self {
        Self { store, log, hooks }
    }

    fn at(&self, run_id: &str, boundary: Boundary) -> Result<()> {
        match self.hooks.at(run_id, &boundary) {
            Step::Continue => Ok(()),
            Step::Crash => {
                self.store.cut();
                Err(CoreError::Cut)
            }
        }
    }

    fn watermark(&self, tx: &Transaction<'_>) -> Result<EpochLsn> {
        let synced = self.log.synced_lsn();
        // A stale writer must not turn a new epoch's admission into a recovery
        // anchor in the previous log. Treat this as a batch-fatal fence error.
        let meta = sql::meta(tx)?;
        if synced.log_epoch != meta.log_epoch || meta.store_id != self.store.store_id() {
            return Err(CoreError::Store("admission log identity changed".into()));
        }
        sql::advance_log_hwm(tx, synced)?;
        Ok(synced)
    }

    fn batch<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let value = self.store.write(|tx| {
            self.watermark(tx)?;
            let value = f(tx)?;
            self.watermark(tx)?;
            Ok(value)
        })?;
        self.at("", Boundary::AdmissionCommitted)?;
        Ok(value)
    }

    fn current(
        &self,
        tx: &Transaction<'_>,
        flow: &str,
        trigger_id: &str,
        trigger: JsonText,
        kind: &str,
        ctx: &AdmitContext,
    ) -> Result<Admission> {
        self.current_attempt(tx, flow, trigger_id, trigger, kind, ctx, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn current_attempt(
        &self,
        tx: &Transaction<'_>,
        flow: &str,
        trigger_id: &str,
        trigger: JsonText,
        kind: &str,
        ctx: &AdmitContext,
        retry: Option<u32>,
    ) -> Result<Admission> {
        if retry.is_none() {
            let tomb: Option<String> = tx
                .query_row(
                    "SELECT run_id FROM tombstones WHERE kind='trigger' AND key=?1",
                    [admission::trigger_key(flow, trigger_id)],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(run_id) = tomb {
                return Ok(Admission::Tombstoned { run_id });
            }
            let existing: Option<String> = tx.query_row(
            "SELECT run_id FROM trigger_inbox WHERE flow_id=?1 AND trigger_id=?2 ORDER BY attempt LIMIT 1",
            params![flow, trigger_id], |r| r.get(0),
        ).optional()?;
            if let Some(run_id) = existing {
                return Ok(Admission::Duplicate { run_id });
            }
        }
        let attempt = retry.unwrap_or(1);
        let approved = install::approved(tx, flow)?;
        let spec = TriggerSpec {
            flow_id: flow.into(),
            trigger_id: trigger_id.into(),
            trigger,
            script: approved
                .as_ref()
                .map(|a| a.script.clone())
                .unwrap_or_default(),
            manifest: approved
                .as_ref()
                .map(|a| a.manifest.clone())
                .unwrap_or_default(),
        };
        let grant = match admission::gate_with_slots(
            tx,
            &spec,
            ctx,
            approved.as_ref(),
            || {
                Ok(tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM live_runs WHERE flow_id=?1)",
                    [flow],
                    |r| r.get(0),
                )?)
            },
            || crate::rate::take_staged(tx, flow, crate::rate::Kind::Run, ctx.now_ms, &ctx.rate),
        )? {
            Ok(grant) => grant,
            Err(refused) => return Ok(refused),
        };
        crate::state_epoch::reset(tx, flow, &spec.manifest, ctx.now_ms)?;
        let (run_id, seq) = next_run_id(tx, self.store.store_id())?;
        let snapshot = crate::runs::snapshot(
            tx,
            flow,
            Some(grant.version),
            &grant.code_hash,
            &spec.script,
            &spec.manifest,
        )?;
        sql::insert_identity(
            tx,
            &RunIdentity {
                run_id: run_id.clone(),
                flow_id: flow.into(),
                flow_version: grant.version,
                code_hash: grant.code_hash,
                snapshot_id: snapshot,
            },
        )?;
        let (self_input, params_input, grant_guards, incarnation, end_instance):
            (Option<String>, Option<String>, Option<String>, Option<i64>, bool) = tx.query_row(
            "SELECT CASE WHEN f.package IS NOT NULL THEN json_object('agent_id',f.owner) END,
             i.params,i.grant_guards,i.incarnation,
             CASE WHEN i.flow_id IS NOT NULL AND json_type(?2,'$.trigger.schedule.at')='text' THEN 1 ELSE 0 END
             FROM flows f LEFT JOIN template_instances i ON i.flow_id=f.flow_id WHERE f.flow_id=?1",
            params![flow, spec.manifest], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        sql::insert_live(
            tx,
            &LiveRun {
                run_id: run_id.clone(),
                flow_id: flow.into(),
                flow_version: grant.version,
                trigger_id: trigger_id.into(),
                trigger_kind: kind.into(),
                trigger: decode(spec.trigger.as_str())?,
                attempt,
                admitted_at: ctx.now_ms,
                admit_seq: seq,
                admit_lsn: self.watermark(tx)?,
                deadline_ms: grant.deadline_ms,
                self_input: optional_json(self_input)?,
                params_input: optional_json(params_input)?,
                grant_guards: optional_json(grant_guards)?,
                template_incarnation: incarnation,
                end_instance,
            },
        )?;
        tx.execute("INSERT INTO trigger_inbox(flow_id,trigger_id,attempt,run_id,admitted_at) VALUES (?1,?2,?3,?4,?5)",
            params![flow, trigger_id, attempt, run_id, ctx.now_ms])?;
        self.at(&run_id, Boundary::AdmissionWritten)?;
        Ok(Admission::Admitted { run_id })
    }

    /// Admit an explicit trigger under the currently approved flow version.
    pub fn admit_trigger(
        &self,
        flow: &str,
        trigger_id: &str,
        trigger: JsonText,
        ctx: &AdmitContext,
    ) -> Result<Admission> {
        self.batch(|tx| {
            fire(tx, || {
                self.current(tx, flow, trigger_id, trigger, "manual", ctx)
            })
        })
    }

    /// Explicitly retry finished work under the current approval and normal gate.
    /// It runs in its own transaction after close, with a new ID and attempt.
    pub fn retrigger(&self, run_id: &str, ctx: &AdmitContext) -> Result<Admission> {
        self.batch(|tx| {
            let run = sql::history(tx, run_id)?.ok_or_else(|| CoreError::NoSuchRun(run_id.into()))?;
            let attempt: u32 = tx.query_row("SELECT COALESCE(MAX(attempt),0)+1 FROM trigger_inbox WHERE flow_id=?1 AND trigger_id=?2", params![run.flow_id, run.trigger_id], |r| r.get(0))?;
            self.current_attempt(tx, &run.flow_id, &run.trigger_id, JsonText::new(run.trigger.to_string()).map_err(|e| CoreError::Invalid(e.to_string()))?, &run.trigger_kind, ctx, Some(attempt))
        })
    }

    /// One event pull commits atomically; only then may the reader acknowledge.
    /// A crash after commit returns an error, leaving redelivery to deduplication.
    pub fn admit_events(
        &self,
        notices: &[Notice],
        ctx: &AdmitContext,
        wakes: &dyn EventWakes,
        acknowledge: impl FnOnce() -> Result<()>,
    ) -> Result<events::Report> {
        let report = self.batch(|tx| {
            let flows: Vec<(String, Option<i64>)> = tx.prepare(
                "SELECT flow_id,first_approved_at FROM flows WHERE state='enabled' ORDER BY flow_id"
            )?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            let mut report = events::Report::default();
            for notice in notices {
                notice.target_flow()?;
                report.model_wakes += wakes.wake(tx, notice, ctx.now_ms)?;
                for (flow, first) in &flows {
                    let before = report.clone();
                    if let Err(error) = fire(tx, || {
                        self.offer_event(tx, flow, *first, notice, ctx, &mut report)
                    }) {
                        if !fire_error(&error) {
                            return Err(error);
                        }
                        report = before;
                        fire_audit(
                            tx,
                            "event.corrupt",
                            flow,
                            &notice.trigger_id(),
                            &error,
                            ctx.now_ms,
                        )?;
                    }
                }
            }
            Ok(report)
        })?;
        if report.model_wakes > 0 {
            wakes.committed();
        }
        acknowledge()?;
        Ok(report)
    }

    fn offer_event(
        &self,
        tx: &Transaction<'_>,
        flow: &str,
        first: Option<i64>,
        notice: &Notice,
        ctx: &AdmitContext,
        report: &mut events::Report,
    ) -> Result<()> {
        let Some(approved) = install::approved(tx, flow)? else {
            return Ok(());
        };
        let manifest = events::effective_manifest(tx, &approved.manifest, flow)?;
        match events::matches(&manifest, notice, flow)? {
            HeaderMatch::Unrelated => return Ok(()),
            HeaderMatch::Filtered => {
                tx.execute("INSERT INTO event_flow_health(flow_id,filtered_out) VALUES (?1,1) ON CONFLICT(flow_id) DO UPDATE SET filtered_out=filtered_out+1", [flow])?;
                report.filtered_out += 1;
                return Ok(());
            }
            HeaderMatch::Accepted => {}
        }
        if !events::after_first_approval(tx, flow, notice, first)? {
            report.skipped_before_install += 1;
            return Ok(());
        }
        if events::stale(&manifest, notice, ctx.now_ms)? {
            events::count_stale_drop(tx, flow)?;
            report.stale_dropped += 1;
            return Ok(());
        }
        events::offer_with(tx, flow, notice, ctx, report, |trigger| {
            self.current(tx, flow, &notice.trigger_id(), trigger, "event", ctx)
        })
    }

    /// Schedule planning, old planned fires, new fires and accounting share
    /// one fenced commit. Planning continues while admission is draining.
    pub fn tick_at(
        &self,
        now: Timestamp,
        config: &SchedulerConfig,
        ctx: &AdmitContext,
    ) -> Result<TickReport> {
        let ctx = AdmitContext {
            now_ms: now.as_millisecond(),
            ..*ctx
        };
        self.batch(|tx| {
            crate::templates::expire_schedules(tx, ctx.now_ms)?;
            let mut report = TickReport::default();
            self.admit_planned(tx, &ctx, &mut report)?;
            for flow in schedule::tick::due_flows(tx, now)? {
                match fire(tx, || schedule::tick::plan_flow(tx, &flow, now, config)) {
                    Ok(Some(planned)) => report.planned.push(planned),
                    Ok(None) => {}
                    Err(error) if fire_error(&error) => {
                        tx.execute(
                            "UPDATE schedules SET state='disabled',updated_at=?2 WHERE flow_id=?1",
                            params![flow, ctx.now_ms],
                        )?;
                        fire_audit(tx, "schedule.corrupt", &flow, "", &error, ctx.now_ms)?;
                    }
                    Err(error) => return Err(error),
                }
            }
            self.admit_planned(tx, &ctx, &mut report)?;
            report.waiting = schedule::tick::planned_count(tx)?;
            Ok(report)
        })
    }

    fn admit_planned(
        &self,
        tx: &Transaction<'_>,
        ctx: &AdmitContext,
        report: &mut TickReport,
    ) -> Result<()> {
        let mut blocked = BTreeSet::new();
        while let Some(fire) = schedule::tick::admit_next_with(
            tx,
            ctx,
            &blocked,
            fire_error,
            |flow, trigger_id, trigger| {
                self.current(tx, flow, trigger_id, trigger, "schedule", ctx)
            },
        )? {
            match fire.admission {
                Admission::Draining => break,
                Admission::StateBusy => {
                    blocked.insert(fire.flow_id);
                }
                _ => report.admitted.push(fire),
            }
        }
        Ok(())
    }

    /// A bounded backlog pass rechecks current approval and preserves each
    /// flow's arrival order, using the same new-path admission transaction.
    pub fn drain_event_backlog(&self, ctx: &AdmitContext) -> Result<usize> {
        self.batch(|tx| {
            let pending: Vec<(i64, String, String, Option<i64>)> = tx.prepare(
                "SELECT e.rowid,e.flow_id,e.notice,f.first_approved_at FROM event_receipts e JOIN flows f USING(flow_id) WHERE e.state='backlog' AND f.state='enabled' ORDER BY e.received_at,e.rowid LIMIT 128"
            )?.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
            let mut blocked = BTreeSet::new();
            let mut admitted = 0;
            for (rowid, flow, encoded, first) in pending {
                if blocked.contains(&flow) { continue; }
                let outcome = fire(tx, || {
                    let notice: Notice = serde_json::from_str(&encoded).map_err(|e| CoreError::Corrupt(e.to_string()))?;
                    if !events::after_first_approval(tx, &flow, &notice, first)? { return Ok(None); }
                    let approved = install::approved(tx, &flow)?;
                    let manifest = approved.as_ref().map(|a| events::effective_manifest(tx, &a.manifest, &flow)).transpose()?;
                    if manifest.as_ref().map(|m| events::matches(m, &notice, &flow)).transpose()? != Some(HeaderMatch::Accepted) {
                        return Ok(None);
                    }
                    if manifest.as_ref().map(|m| events::stale(m, &notice, ctx.now_ms)).transpose()?.unwrap_or(false) {
                        events::count_stale_drop(tx, &flow)?;
                        return Ok(None);
                    }
                    self.current(tx, &flow, &notice.trigger_id(), notice.trigger()?, "event", ctx).map(Some)
                });
                let state = match outcome {
                    Ok(Some(Admission::Admitted { .. })) => { admitted += 1; "admitted" }
                    Ok(Some(Admission::Duplicate { .. } | Admission::Tombstoned { .. })) => "admitted",
                    Ok(Some(_)) => { blocked.insert(flow); continue; }
                    Ok(None) => "refused",
                    Err(error) if fire_error(&error) => {
                        fire_audit(tx, "event.corrupt", &flow, &rowid.to_string(), &error, ctx.now_ms)?;
                        "refused"
                    }
                    Err(error) => return Err(error),
                };
                tx.execute("UPDATE event_receipts SET state=?2 WHERE rowid=?1", params![rowid, state])?;
            }
            Ok(admitted)
        })
    }
}

fn decode(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|e| CoreError::Corrupt(e.to_string()))
}

fn optional_json(text: Option<String>) -> Result<Value> {
    text.map(|s| decode(&s))
        .transpose()
        .map(|v| v.unwrap_or(Value::Null))
}

fn next_run_id(tx: &Transaction<'_>, store_id: &str) -> Result<(String, i64)> {
    let counter: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key='next_run'", [], |r| {
            r.get(0)
        })
        .optional()?;
    let n: i64 = counter
        .as_deref()
        .unwrap_or("1")
        .parse()
        .map_err(|_| CoreError::Store("invalid admission counter".into()))?;
    let next = n
        .checked_add(1)
        .filter(|_| n > 0)
        .ok_or_else(|| CoreError::Store("admission counter exhausted".into()))?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random)
        .map_err(|e| CoreError::Store(format!("run identity entropy: {e}")))?;
    let random = u64::from_ne_bytes(random);
    tx.execute("INSERT INTO meta(key,value) VALUES ('next_run',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [next.to_string()])?;
    Ok((format!("run-{store_id}-{n}-{random:016x}"), n))
}

fn fire_error(error: &CoreError) -> bool {
    matches!(error, CoreError::Corrupt(_) | CoreError::Invalid(_))
}

fn fire<T>(tx: &Transaction<'_>, f: impl FnOnce() -> Result<T>) -> Result<T> {
    tx.execute_batch("SAVEPOINT run_admission_fire")?;
    match f() {
        Ok(value) => {
            tx.execute_batch("RELEASE run_admission_fire")?;
            Ok(value)
        }
        Err(error) => {
            tx.execute_batch("ROLLBACK TO run_admission_fire; RELEASE run_admission_fire")?;
            Err(error)
        }
    }
}

fn fire_audit(
    tx: &Transaction<'_>,
    action: &str,
    flow: &str,
    trigger: &str,
    error: &CoreError,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO audit(at,actor,action,detail) VALUES (?1,'runtime',?2,?3)",
        params![
            now,
            action,
            serde_json::json!({"flow_id":flow,"trigger_id":trigger,"error":error.to_string()})
                .to_string()
        ],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "run_admission_tests.rs"]
pub(crate) mod tests;
