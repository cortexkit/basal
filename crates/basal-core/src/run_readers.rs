//! Reader snapshots for the staged execution model. Actors are copied before
//! SQLite is opened; a terminal row wins if a run closed between the two reads.

use basal_proto::{JsonText, RecordedOutcome};
use rusqlite::{Connection, OptionalExtension};

use crate::{
    CoreError, Result,
    model::{CallRow, DispatchState, Run, RunState},
    run_records::{CallState, State},
    run_store_sql as sql,
};

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

pub(crate) fn load(
    conn: &Connection,
    id: &str,
    snapshot: Option<&State>,
    active: bool,
    owner: &str,
) -> Result<Run> {
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
            .or_else(|| history_lost.then(|| "history_lost".into())),
        error_detail: history.as_ref().and_then(|h| h.error_detail.clone()),
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
        scope_cut: false,
        payload_pruned: false,
    })
}

pub(crate) fn calls(conn: &Connection, id: &str, snapshot: Option<&State>) -> Result<Vec<CallRow>> {
    if let Some(state) = snapshot {
        return state.calls.values().map(call).collect();
    }
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
        .collect()
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
        let states = self.log_states();
        let hosts = self.shared.staged_hosts.as_ref().unwrap();
        hosts
            .registry()
            .accounting_read(flow, self.store(), |conn, runs| {
                let Some(approved) = crate::install::approved(conn, flow)? else {
                    return Ok(None);
                };
                let manifest = crate::manifest::Manifest::parse_stored(&approved.manifest)
                    .map_err(|e| CoreError::Corrupt(e.to_string()))?;
                let Some(ms) = manifest.token_window_ms() else {
                    return Ok(None);
                };
                let start = crate::tokens::window_start(at, ms);
                let key = format!("token:{ms}:{start}");
                let mut usage = crate::tokens::window_usage(conn, flow, ms, start)?;
                for (id, rows) in runs {
                    for row in rows.iter().filter(|r| r.window.0 == key) {
                        let call = states.get(id).and_then(|s| s.calls.get(&row.position));
                        match call.and_then(|c| c.arrival.as_ref()) {
                            Some(arrival)
                                if arrival.usage.as_ref().is_some_and(|u| {
                                    u.input_tokens.saturating_add(u.output_tokens) == row.amount
                                }) =>
                            {
                                let measured = arrival.usage.as_ref().expect("matched usage");
                                usage.input_tokens =
                                    usage.input_tokens.saturating_add(measured.input_tokens);
                                usage.output_tokens =
                                    usage.output_tokens.saturating_add(measured.output_tokens);
                            }
                            Some(arrival) if arrival.usage.is_none() => {
                                usage.unreported_tokens =
                                    usage.unreported_tokens.saturating_add(row.amount)
                            }
                            // A callback may have settled since the actor snapshot.
                            // The accounting amount is authoritative; categorize an
                            // unmatched row as reserved rather than under-counting it.
                            _ => usage.reserved = usage.reserved.saturating_add(row.amount),
                        }
                    }
                }
                Ok(Some(usage))
            })
    }

    pub fn open_reservations(&self, id: &str) -> Result<u64> {
        if !self.uses_run_log() {
            return self
                .store()
                .read(|c| crate::tokens::open_reservations(c, id));
        }
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
        if !self.uses_run_log() {
            return self
                .store()
                .read(|c| crate::classify_cost::spent(c, "flow", id));
        }
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
        if !self.uses_run_log() {
            return self.store().read(crate::journal::deferred_calls);
        }
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
    /// Ordered run projection for module readers. Finished triggers come from
    /// history; live execution fields come only from the registry.
    pub fn runs_for_flow(&self, flow: &str) -> Result<Vec<Run>> {
        if !self.uses_run_log() {
            return self.store().read(|c| crate::runs::for_flow(c, flow));
        }
        self.log_runs(Some(flow))
    }

    pub(crate) fn log_runs(&self, flow: Option<&str>) -> Result<Vec<Run>> {
        let states = self.log_states();
        let owners = self.shared.staged_owners.lock().unwrap().clone();
        self.store().read(|c| {
            let mut stmt = c.prepare("SELECT run_id,admit_seq FROM live_runs WHERE (?1 IS NULL OR flow_id=?1) UNION ALL SELECT run_id,admit_seq FROM run_history WHERE (?1 IS NULL OR flow_id=?1) ORDER BY admit_seq,run_id")?;
            let ids = stmt.query_map([flow], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
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
        if self.uses_run_log() {
            return Ok(self.staged_snapshot(id).map_or(0, |s| s.calls.len() as u64));
        }
        self.store().read(|c| crate::journal::replay_size(c, id))
    }

    pub(crate) fn log_health(&self) -> Result<crate::ops::Health> {
        let runs = self.log_runs(None)?;
        let states = self.log_states();
        self.store().read(|c| {
            let mut health = crate::ops::Health::default();
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
