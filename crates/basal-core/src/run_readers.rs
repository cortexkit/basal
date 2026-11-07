//! Reader snapshots for the staged execution model. Actors are copied before
//! SQLite is opened; a terminal row wins if a run closed between the two reads.

use basal_proto::{JsonText, RecordedOutcome};
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeMap;

use crate::{
    CoreError, Result,
    model::{CallRow, DispatchState, Run, RunState},
    run_records::{CallState, State},
    run_store_sql as sql,
};

#[cfg(test)]
thread_local! {
    static LOAD_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[path = "run_reader_tests.rs"]
mod tests;

pub(crate) fn retirement_busy(conn: &Connection, flow: &str, version: u64) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM run_identities r WHERE r.flow_id=?1 AND r.flow_version=?2 AND (
        EXISTS(SELECT 1 FROM live_runs l WHERE l.run_id=r.run_id)
        OR EXISTS(SELECT 1 FROM obligations o WHERE o.run_id=r.run_id AND o.state<>'settled' AND NOT EXISTS(SELECT 1 FROM quarantine q WHERE q.run_id=o.run_id AND q.position=o.position AND q.reason='run_cancelled'))
        OR EXISTS(SELECT 1 FROM token_ledger t WHERE t.run_id=r.run_id AND t.state='reserved')
        OR EXISTS(SELECT 1 FROM classify_costs c WHERE c.kind='flow' AND c.run_id=r.run_id AND c.charged IS NULL)
        OR EXISTS(SELECT 1 FROM decision_cards d WHERE d.run_id=r.run_id AND d.state='open')
        OR EXISTS(SELECT 1 FROM broca_calls b WHERE b.run_id=r.run_id AND COALESCE(json_extract(b.snapshot,'$.acknowledged'),0)=0)
        OR EXISTS(SELECT 1 FROM outbox o WHERE o.kind='refusal_committed' AND CAST(json_extract(o.body,'$.run_id') AS TEXT)=r.run_id)))", rusqlite::params![flow,version], |r| r.get(0))?)
}

pub(crate) fn lifecycle(state: &State, active: bool) -> RunState {
    if state
        .calls
        .values()
        .any(|c| c.unknown.is_some() && c.arrival.is_none())
    {
        RunState::NeedsReconcile
    } else if active {
        RunState::Running
    } else if state.parked {
        RunState::Suspended
    } else {
        RunState::Pending
    }
}

fn live_runs(
    conn: &Connection,
    states: &BTreeMap<String, State>,
    owners: &std::collections::HashMap<String, (u64, String)>,
) -> Result<Vec<Run>> {
    let mut stmt = conn.prepare("SELECT run_id FROM live_runs ORDER BY admit_seq,run_id")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ids.iter()
        .map(|id| {
            load(
                conn,
                id,
                states.get(id),
                owners
                    .get(id)
                    .is_some_and(|(g, _)| states.get(id).is_some_and(|s| s.generation == *g)),
                owners.get(id).map_or("", |(_, owner)| owner),
            )
        })
        .collect()
}

pub(crate) fn load(
    conn: &Connection,
    id: &str,
    snapshot: Option<&State>,
    active: bool,
    owner: &str,
) -> Result<Run> {
    #[cfg(test)]
    LOAD_CALLS.with(|calls| calls.set(calls.get() + 1));
    let identity = sql::identity(conn, id)?.ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
    let (script, manifest) = conn.query_row(
        "SELECT script,manifest FROM run_snapshots WHERE snapshot_id=?1",
        [identity.snapshot_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let history = sql::history(conn, id)?;
    let live = sql::live(conn, id)?;
    let history_lost = live.is_some() && sql::history_lost(conn, id)?;
    let state = snapshot
        .cloned()
        .unwrap_or_else(|| State::new(live.as_ref().map_or(0, |l| l.admitted_at)));
    let unknown: Vec<_> = state.calls.values().filter(|c| c.arrival.is_none() && c.unknown.is_some()).take(5).map(|call| {
        let reason = call.unknown.as_deref().and_then(crate::run_records::parse_unknown_reason);
        let code = if reason == Some(basal_host::UnknownReason::ProviderLostRun) { "unknown_run" } else { reason.map_or("unknown", |r| r.as_str()) };
        serde_json::json!({"position":call.identity.position,"code":code,"reason":reason.map(|r| r.as_str()),"handle":call.accepted.as_ref().map(|h| h.chars().take(256).collect::<String>())})
    }).collect();
    let (trigger_id, trigger, admitted_at, admit_seq) = if let Some(h) = &history {
        (
            h.trigger_id.clone(),
            h.trigger.clone(),
            h.admitted_at,
            h.admit_seq,
        )
    } else if let Some(l) = &live {
        (
            l.trigger_id.clone(),
            l.trigger.clone(),
            l.admitted_at,
            l.admit_seq,
        )
    } else {
        return Err(CoreError::NoSuchRun(id.into()));
    };
    let attempt = if let Some(l) = &live {
        l.attempt
    } else {
        conn.query_row(
            "SELECT attempt FROM trigger_inbox WHERE run_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1)
    };
    Ok(Run {
        run_id: id.into(),
        flow_id: identity.flow_id,
        trigger_id,
        attempt,
        trigger: crate::model::json(trigger.to_string(), "trigger")?,
        // Finished summaries deliberately do not retain activation inputs.
        self_input: crate::model::json(
            live.as_ref()
                .map_or_else(|| "null".into(), |l| l.self_input.to_string()),
            "self input",
        )?,
        params_input: crate::model::json(
            live.as_ref()
                .map_or_else(|| "null".into(), |l| l.params_input.to_string()),
            "params input",
        )?,
        grant_guards: crate::model::json(
            live.as_ref()
                .map_or_else(|| "[]".into(), |l| l.grant_guards.to_string()),
            "grant guards",
        )?,
        script,
        manifest,
        code_hash: identity.code_hash,
        fingerprint: state.fingerprint.clone(),
        state: history.as_ref().map_or_else(
            || {
                if history_lost {
                    RunState::NeedsReconcile
                } else {
                    lifecycle(&state, active)
                }
            },
            |h| h.state,
        ),
        owner: (history.is_none() && active).then(|| owner.into()),
        generation: state.generation,
        readiness: state.readiness,
        awaited: state.awaited,
        result: history
            .as_ref()
            .and_then(|h| h.result.as_ref().map(ToString::to_string)),
        error_kind: history
            .as_ref()
            .and_then(|h| h.error_kind.clone())
            .or_else(|| history_lost.then(|| "history_lost".into()))
            .or_else(|| (!unknown.is_empty()).then(|| "unknown_outcome".into())),
        error_detail: history
            .as_ref()
            .and_then(|h| h.error_detail.clone())
            .or_else(|| {
                (!unknown.is_empty()).then(|| serde_json::json!({"positions":unknown}).to_string())
            }),
        broken: history
            .as_ref()
            .map_or(u32::try_from(state.broken).unwrap_or(u32::MAX), |h| {
                h.broken
            }),
        admitted_at,
        ended_at: history.as_ref().map(|h| h.ended_at),
        flow_version: Some(identity.flow_version),
        admit_seq: Some(admit_seq),
        deadline_ms: live.as_ref().map(|l| l.deadline_ms),
        deadline_at: history
            .is_none()
            .then(|| state.claim.as_ref().map(|c| c.deadline_at))
            .flatten(),
    })
}

pub(crate) fn call(call: &CallState) -> Result<CallRow> {
    let intent = call.intent.as_ref();
    let outcome = if let Some(sync) = &call.sync {
        Some(RecordedOutcome {
            settlement: basal_proto::Settlement::Fulfilled,
            value: sync.value.clone(),
            delivery_order: sync.delivery_order,
        })
    } else {
        call.delivery_order.and_then(|order| {
            call.arrival.as_ref().map(|a| RecordedOutcome {
                settlement: a.settlement,
                value: a.value.clone(),
                delivery_order: order,
            })
        })
    };
    Ok(CallRow {
        position: call.identity.position,
        kind: call.identity.kind.clone(),
        args: call.identity.args.clone(),
        args_digest: call.identity.args_digest,
        idempotency_key: intent
            .and_then(|i| i.idempotency_key.clone())
            .unwrap_or_default(),
        class: call.identity.class,
        dispatch: if call.unknown.is_some() && call.arrival.is_none() {
            DispatchState::Unknown
        } else if call.deferred.is_some() && call.arrival.is_none() {
            DispatchState::Deferred
        } else if call.accepted.is_some() {
            DispatchState::Accepted
        } else if intent.is_some() {
            DispatchState::Sent
        } else {
            DispatchState::None
        },
        attempts: u32::try_from(intent.map_or(0, |i| i.attempts))
            .map_err(|_| CoreError::Corrupt("attempts overflow".into()))?,
        handle: call.accepted.clone(),
        outcome,
        request: intent.map(|i| i.frozen_request.clone()),
        scope_cut: call.scope_cut,
        payload_pruned: false,
    })
}

pub(crate) fn calls(conn: &Connection, id: &str, snapshot: Option<&State>) -> Result<Vec<CallRow>> {
    let mut calls: Vec<CallRow> = if let Some(state) = snapshot {
        state.calls.values().map(call).collect::<Result<_>>()?
    } else {
        let mut stmt =
            conn.prepare("SELECT detail FROM run_calls WHERE run_id=?1 ORDER BY position")?;
        let rows = stmt
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|text| {
                let state: CallState =
                    serde_json::from_str(&text).map_err(|e| CoreError::Corrupt(e.to_string()))?;
                call(&state)
            })
            .collect::<Result<_>>()?
    };
    if calls.iter().any(|c| c.idempotency_key.is_empty()) {
        let identity = sql::identity(conn, id)?.ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
        for call in &mut calls {
            if call.idempotency_key.is_empty() {
                // Preparation may not have authorized an Intent yet. Its public
                // key is still determined by the durable run identity/position.
                call.idempotency_key =
                    crate::ids::idempotency_key(&identity.flow_id, id, call.position);
            }
        }
    }
    Ok(calls)
}

pub(crate) fn json(value: &serde_json::Value) -> Result<JsonText> {
    crate::model::json(value.to_string(), "run input")
}

impl crate::Runtime {
    pub(crate) fn log_token_usage(
        &self,
        flow: &str,
        at: i64,
    ) -> Result<Option<crate::tokens::WindowUsage>> {
        Ok(self.log_token_usage_many(&[flow], at)?.remove(flow))
    }

    pub(crate) fn log_token_usage_many(
        &self,
        flow_ids: &[&str],
        at: i64,
    ) -> Result<BTreeMap<String, crate::tokens::WindowUsage>> {
        let flow_ids: Vec<_> = flow_ids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let states = self.log_states();
        let hosts = self.shared.staged_hosts.as_ref().unwrap();
        hosts
            .registry()
            .accounting_read_many(&flow_ids, self.store(), |conn, flows| {
                let mut windows = crate::tokens::window_usage_many(conn, &flow_ids, at)?;
                for (flow, usage) in &mut windows {
                    let key = format!("token:{}:{}", usage.window_ms, usage.window_start);
                    let Some(runs) = flows.get(flow) else {
                        continue;
                    };
                    for (id, rows) in *runs {
                        for row in rows.iter().filter(|r| r.window.0 == key) {
                            let call = states.get(id).and_then(|s| s.calls.get(&row.position));
                            match call.and_then(|c| c.token_usage()) {
                                Some(measured)
                                    if measured
                                        .input_tokens
                                        .saturating_add(measured.output_tokens)
                                        == row.amount =>
                                {
                                    let report = call.and_then(|c| c.usage_report).unwrap_or(
                                        crate::run_records::UsageReport {
                                            input_tokens: Some(measured.input_tokens),
                                            cache_write_tokens: None,
                                            output_tokens: Some(measured.output_tokens),
                                            cached_input_tokens: None,
                                        },
                                    );
                                    usage.input_tokens = usage
                                        .input_tokens
                                        .saturating_add(report.input_tokens.unwrap_or(0));
                                    usage.cache_write_tokens = usage
                                        .cache_write_tokens
                                        .saturating_add(report.cache_write_tokens.unwrap_or(0));
                                    usage.output_tokens = usage
                                        .output_tokens
                                        .saturating_add(report.output_tokens.unwrap_or(0));
                                    usage.cached_input_tokens = usage
                                        .cached_input_tokens
                                        .saturating_add(report.cached_input_tokens.unwrap_or(0));
                                    let known = report
                                        .input_tokens
                                        .unwrap_or(0)
                                        .saturating_add(report.cache_write_tokens.unwrap_or(0))
                                        .saturating_add(report.output_tokens.unwrap_or(0));
                                    usage.unreported_tokens = usage
                                        .unreported_tokens
                                        .saturating_add(row.amount.saturating_sub(known));
                                }
                                None if call.is_some_and(|c| c.arrival.is_some()) => {
                                    usage.unreported_tokens =
                                        usage.unreported_tokens.saturating_add(row.amount)
                                }
                                // A callback may have settled after the run state used for
                                // this accounting read was captured.
                                // The accounting amount is authoritative; categorize an
                                // unmatched row as reserved rather than under-counting it.
                                _ => usage.reserved = usage.reserved.saturating_add(row.amount),
                            }
                        }
                    }
                }
                Ok(windows)
            })
    }

    pub fn open_reservations(&self, id: &str) -> Result<u64> {
        {}
        let state = self.staged_snapshot(id);
        let identity = self
            .store()
            .read(|c| sql::identity(c, id))?
            .ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
        self.shared
            .staged_hosts
            .as_ref()
            .unwrap()
            .registry()
            .accounting_read(&identity.flow_id, self.store(), |conn, runs| {
                let committed = crate::tokens::open_reservations(conn, id)?;
                let pending = runs
                    .get(id)
                    .into_iter()
                    .flatten()
                    .filter(|r| r.window.0.starts_with("token:"))
                    .filter(|r| {
                        state
                            .as_ref()
                            .and_then(|s| s.calls.get(&r.position))
                            .is_none_or(|c| c.arrival.is_none())
                    })
                    .count() as u64;
                Ok(committed.saturating_add(pending))
            })
    }

    pub fn classify_cost_spent(&self, id: &str) -> Result<f64> {
        {}
        let identity = self
            .store()
            .read(|c| sql::identity(c, id))?
            .ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
        self.shared
            .staged_hosts
            .as_ref()
            .unwrap()
            .registry()
            .accounting_read(&identity.flow_id, self.store(), |conn, runs| {
                let committed = crate::classify_cost::spent(conn, "flow", id)?;
                let live = runs
                    .get(id)
                    .into_iter()
                    .flatten()
                    .filter(|r| r.window.0.starts_with("cost:"))
                    .fold(0u64, |n, r| n.saturating_add(r.amount));
                Ok(committed + live as f64 / 1_000_000.0)
            })
    }

    pub fn deferred_calls(&self) -> Result<Vec<(String, String, u64, i64)>> {
        {}
        let mut calls = Vec::new();
        for run in self
            .shared
            .staged_hosts
            .as_ref()
            .unwrap()
            .registry()
            .snapshot()
        {
            let (id, flow, _, state) = run.reader_snapshot();
            if state.commit_seq.is_some() {
                continue;
            }
            for call in state.calls.values().filter(|c| c.arrival.is_none()) {
                if let Some(d) = &call.deferred {
                    calls.push((
                        id.clone(),
                        flow.clone(),
                        call.identity.position,
                        d.retry_not_before,
                    ));
                }
            }
        }
        calls.sort_by(|a, b| (a.3, &a.0, a.2).cmp(&(b.3, &b.0, b.2)));
        Ok(calls)
    }
    /// All runs for a flow, in ascending (admission sequence, run id) order.
    /// Readers showing a history page should use `recent_runs_for_flow` instead.
    pub fn runs_for_flow(&self, flow: &str) -> Result<Vec<Run>> {
        self.log_flow_runs(flow, None)
    }

    /// The newest `limit` runs, returned in ascending (admission sequence, run
    /// id) order, just like `runs_for_flow`. The limit is applied before loading
    /// run details, so a history page does not materialize the flow's past.
    pub fn recent_runs_for_flow(&self, flow: &str, limit: u32) -> Result<Vec<Run>> {
        self.log_flow_runs(flow, Some(limit))
    }

    /// Runs not yet finished, in admission order. Every unfinished run is in
    /// `live_runs`; history holds only terminal ones. Timers and deadline
    /// checks run on every engine pass, so they must never touch history,
    /// which grows to hundreds of thousands of rows.
    pub(crate) fn log_live_runs(&self) -> Result<Vec<Run>> {
        let states = self.log_states();
        let owners = self.shared.staged_owners.lock().unwrap().clone();
        self.store().read(|c| live_runs(c, &states, &owners))
    }

    fn log_flow_runs(&self, flow: &str, limit: Option<u32>) -> Result<Vec<Run>> {
        let states = self.log_states();
        let owners = self.shared.staged_owners.lock().unwrap().clone();
        self.store().read(|c| {
            let mut stmt = c.prepare("SELECT run_id,admit_seq FROM live_runs WHERE flow_id=?1 UNION ALL SELECT run_id,admit_seq FROM run_history WHERE flow_id=?1 ORDER BY admit_seq DESC,run_id DESC LIMIT ?2")?;
            let mut ids = stmt.query_map(rusqlite::params![flow, limit.map_or(-1, i64::from)], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            ids.reverse();
            ids.iter().map(|id| load(c, id, states.get(id), owners.get(id).is_some_and(|(g,_)|states.get(id).is_some_and(|s|s.generation==*g)), owners.get(id).map_or("", |(_,owner)|owner))).collect()
        })
    }

    pub(crate) fn log_states(&self) -> std::collections::BTreeMap<String, State> {
        self.shared
            .staged_hosts
            .as_ref()
            .map(|h| {
                h.registry()
                    .snapshot()
                    .into_iter()
                    .map(|r| {
                        let (id, _, _, state) = r.reader_snapshot();
                        (id, state)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn replay_size(&self, id: &str) -> Result<u64> {
        Ok(self.staged_snapshot(id).map_or(0, |s| s.calls.len() as u64))
    }

    pub(crate) fn log_health(&self) -> Result<crate::ops::Health> {
        let states = self.log_states();
        let owners = self.shared.staged_owners.lock().unwrap().clone();
        self.store().read(|c| {
            // Read both halves under one store lock: a finishing run must not
            // appear in both the live projection and the history counts.
            let runs = live_runs(c, &states, &owners)?;
            let mut health = crate::ops::Health::default();
            let mut counts = c.prepare("SELECT state, count(*) FROM run_history GROUP BY state")?;
            for row in counts.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))? {
                let (state, count) = row?;
                health.runs.insert(state, count);
            }
            for run in &runs {
                *health.runs.entry(run.state.as_str().into()).or_default() += 1;
                if !run.state.is_terminal() { health.unfinished.entry(run.state.as_str().into()).or_default().push(run.run_id.clone()); }
                if !run.state.is_terminal() && let Some(state) = states.get(&run.run_id) {
                    for call in state.calls.values() {
                        if call.unknown.is_some() && call.arrival.is_none() { health.unknown_calls.push((run.run_id.clone(),call.identity.position)); }
                        if call.arrival.is_none() && call.sync.is_none() { health.open_obligations.push((run.run_id.clone(),call.identity.position)); }
                    }
                }
            }
            let mut stmt = c.prepare("SELECT run_id,position,state FROM obligations WHERE state<>'settled' ORDER BY run_id,position")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?)))? {
                let (id, position, state) = row?;
                health.open_obligations.push((id.clone(),position));
                if state == "unknown" { health.unknown_calls.push((id,position)); }
            }
            health.open_obligations.sort(); health.open_obligations.dedup();
            health.unknown_calls.sort(); health.unknown_calls.dedup();
            health.quarantined = c.query_row("SELECT COUNT(*) FROM quarantine", [], |r| r.get(0))?;
            health.draining = c.query_row("SELECT value FROM meta WHERE key='draining'", [], |r| r.get::<_,String>(0)).optional()?.as_deref() == Some("1");
            Ok(health)
        })
    }
}
