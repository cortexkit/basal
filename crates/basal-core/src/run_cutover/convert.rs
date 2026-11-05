use super::{
    audit,
    legacy::{self, Row, blob32, corrupt, integer, optional_text, text, unsigned},
};
use crate::{
    NoHooks, Result, Store,
    model::{self, RunState, StoredClass},
    run_log::{self, Lsn, RunLog},
    run_records::{
        self as records, Arrival, ArrivalDurability, Body, CallIdentity, CallState, Intent, Record,
        State, Sync,
    },
    run_store_sql as sql,
};
use basal_proto::{ArgsDigest, CallKind, JsonText, Settlement};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc};

fn json_text(value: &str) -> Result<JsonText> {
    model::json(value.to_owned(), "migration payload")
}
fn parse_optional(row: &Row, key: &str, default: Value) -> Result<Value> {
    optional_text(row, key)
        .map(|s| serde_json::from_str(&s).map_err(|e| corrupt(e.to_string())))
        .unwrap_or(Ok(default))
}
fn bounded(value: Option<String>, limit: usize) -> Option<String> {
    value.map(|mut s| {
        if s.len() > limit {
            let mut n = limit;
            while !s.is_char_boundary(n) {
                n -= 1;
            }
            s.truncate(n);
        }
        s
    })
}

pub(super) fn create(tx: &Transaction<'_>, store: &Store) -> Result<()> {
    let high: i64=tx.query_row("SELECT MAX(n) FROM (SELECT COALESCE(MAX(admit_seq),0) n FROM runs UNION ALL SELECT COALESCE(MAX(rowid),0) FROM runs UNION ALL SELECT COALESCE(MAX(seq),0) FROM schedule_fires UNION ALL SELECT COALESCE(MAX(seq),0) FROM sqlite_sequence WHERE name='schedule_fires')",[],|r|r.get(0))?;
    let next = high
        .checked_add(1)
        .ok_or_else(|| corrupt("admission sequence exhausted"))?;
    tx.execute_batch(include_str!("../schema_run_log.sql"))?;
    tx.execute("INSERT INTO meta(key,value) VALUES('next_run',?1) ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)",[next.to_string()])?;
    if store.store_id().len() != 16 {
        return Err(corrupt("store identity must contain 16 bytes"));
    }
    let base = Lsn {
        sequence: 1,
        offset: 0,
    };
    sql::write_meta(
        tx,
        &sql::Meta {
            store_id: store.store_id().to_owned(),
            log_epoch: 1,
            log_hwm: sql::EpochLsn {
                log_epoch: 1,
                lsn: base,
            },
            commit_seq: 0,
            gc_floor: sql::GcFloor {
                log_epoch: 1,
                first_retained: 1,
                predecessor: None,
                synced_lsn: base,
                live_starts: BTreeMap::new(),
            },
            migration_phase: "create".into(),
        },
    )?;
    sql::set_restore_pending(tx, false)?;
    audit::check_shared(tx)
}

fn identity(run: &Row) -> Result<sql::RunIdentity> {
    let version = run.get("flow_version").and_then(Value::as_i64).unwrap_or(0);
    if integer(run, "snapshot_version")? != version
        || blob32(run, "snapshot_hash")? != blob32(run, "code_hash")?
        || crate::ids::code_hash(text(run, "script")?, text(run, "manifest")?)
            != blob32(run, "code_hash")?
    {
        return Err(corrupt("run snapshot identity/code hash mismatch"));
    }
    Ok(sql::RunIdentity {
        run_id: text(run, "run_id")?.into(),
        flow_id: text(run, "flow_id")?.into(),
        flow_version: u32::try_from(version).map_err(|_| corrupt("invalid version"))?,
        code_hash: blob32(run, "code_hash")?,
        snapshot_id: integer(run, "snapshot_id")?,
    })
}
fn admit_seq(run: &Row) -> Result<i64> {
    Ok(run
        .get("admit_seq")
        .and_then(Value::as_i64)
        .unwrap_or(integer(run, "legacy_rowid")?))
}
fn trigger_kind(run: &Row) -> Result<String> {
    let trigger = legacy::json(run, "trigger")?;
    Ok(trigger
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("manual")
        .to_owned())
}
fn audit_at(c: &Connection, id: &str, position: u64, admitted: i64) -> Result<i64> {
    Ok(c.query_row(
        "SELECT at FROM call_audit WHERE run_id=?1 AND position=?2",
        params![id, position],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or(admitted))
}
fn call(c: &Connection, row: &Row, admitted: i64) -> Result<CallState> {
    let mut kind = model::kind_from_columns(
        integer(row, "kind_code")?,
        optional_text(row, "module"),
        optional_text(row, "op"),
    )?;
    if row.get("targeted").and_then(Value::as_i64) == Some(1)
        && let CallKind::Op { module, op } = kind
    {
        kind = CallKind::OpFor { module, op };
    }
    let position = unsigned(row, "position")?;
    let class = StoredClass::parse(text(row, "class")?)?;
    let identity = CallIdentity {
        position,
        op: match &kind {
            CallKind::Op { op, .. } | CallKind::OpFor { op, .. } => op.clone(),
            CallKind::Primitive(p) => p.name().into(),
            CallKind::Tool { name } => name.clone(),
        },
        kind,
        class,
        args: json_text(optional_text(row, "args").as_deref().unwrap_or("null"))?,
        args_digest: ArgsDigest(blob32(row, "args_digest")?),
    };
    let attempts = unsigned(row, "attempts")?;
    let id = text(row, "run_id")?;
    let at = audit_at(c, id, position, admitted)?;
    let token = legacy::rows(
        c,
        "SELECT * FROM token_ledger WHERE run_id=?1 AND position=?2",
        params![id, position],
    )?
    .pop();
    let dollars = legacy::rows(
        c,
        "SELECT * FROM classify_costs WHERE kind='flow' AND run_id=?1 AND position=?2",
        params![id, position],
    )?
    .pop();
    let reservation = if token.is_some() || dollars.is_some() {
        Some(records::Reservation {
            input_tokens: token
                .as_ref()
                .map(|r| unsigned(r, "reserved"))
                .transpose()?
                .unwrap_or(0),
            output_tokens: 0,
            cost_micros: dollars
                .as_ref()
                .map(|r| ((r["reserved"].as_f64().unwrap_or(0.0) * 1_000_000.0).ceil()) as u64)
                .unwrap_or(0),
            window_start: token
                .as_ref()
                .map(|r| integer(r, "window_start"))
                .transpose()?
                .unwrap_or(at),
        })
    } else {
        None
    };
    let intent = if !matches!(class, StoredClass::Sync | StoredClass::Local) && attempts > 0 {
        // Legacy rate counts have no durable per-attempt window. They remain
        // pre-accounted and are never refunded from a guessed window. This may
        // conservatively overcount an in-flight legacy send for one rate window.
        Some(Intent {
            call: identity.clone(),
            idempotency_key: Some(text(row, "idempotency_key")?.into()),
            frozen_request: json_text(
                optional_text(row, "request")
                    .as_deref()
                    .unwrap_or(identity.args.as_str()),
            )?,
            attempts,
            dispatch_count_window: None,
            audit_at: at,
            reservation: reservation.clone(),
        })
    } else {
        None
    };
    let mut state = CallState {
        identity,
        intent,
        pre_accounted_attempts: attempts,
        pre_accounted_reservation: reservation.is_some(),
        prior_intents: Vec::new(),
        sync: None,
        accepted: optional_text(row, "handle"),
        arrival: None,
        delivery_order: row.get("delivery_order").and_then(Value::as_u64),
        deferred: None,
        prior_deferrals: Vec::new(),
        unknown: optional_text(row, "unknown_reason"),
        resolutions: Vec::new(),
    };
    if let Some(outcome) = legacy::established(c, row)? {
        let settlement = model::parse_settlement(&outcome.settlement)?;
        let value = json_text(&outcome.value)?;
        if class == StoredClass::Sync {
            state.sync = Some(Sync {
                call: state.identity.clone(),
                delivery_order: unsigned(row, "delivery_order")?,
                value,
                clock_ms: row
                    .get("clock_ms")
                    .and_then(Value::as_f64)
                    .map(|n| n as i64),
            });
        } else {
            state.delivery_order = outcome.order;
            state.arrival = Some(Arrival {
                position,
                arrival_seq: outcome.arrival,
                outcome: if settlement == Settlement::Fulfilled {
                    "fulfilled"
                } else {
                    "rejected"
                }
                .into(),
                settlement,
                value,
                value_hash: outcome.hash,
                handle: outcome.handle,
                usage: None,
                cost_micros: None,
                kv_effect: None,
                durability: ArrivalDurability::default(),
            });
        }
    }
    if text(row, "dispatch")? == "not_applied" {
        state.unknown = None;
        state.resolutions.push(records::Resolution {
            position,
            attempts,
            idempotency_key: Some(text(row, "idempotency_key")?.into()),
            choice: "not_applied".into(),
            observed_outcome: None,
            usage: None,
            elicitation_id: format!("migration:{id}:{position}"),
        });
    }
    if text(row, "dispatch")? == "deferred" {
        state.deferred = Some(records::Deferred {
            position,
            retry_not_before: integer(row, "retry_not_before")?,
            cause: text(row, "refusal_detail")?.into(),
            audit_at: at,
            proven_unsent: class == StoredClass::Mutation,
            refund_dispatch: true,
                    attempt: None,
        });
    }
    Ok(state)
}

pub(super) fn state(c: &Connection, run: &Row) -> Result<State> {
    let id = text(run, "run_id")?;
    let mut state = State::new(integer(run, "admitted_at")?);
    state.generation = unsigned(run, "generation")?;
    state.broken = unsigned(run, "broken")?;
    state.activation_count = c.query_row(
        "SELECT COUNT(*) FROM activations WHERE run_id=?1",
        [id],
        |r| r.get(0),
    )?;
    state.fingerprint = optional_text(run, "fingerprint");
    state.script_started = integer(run, "script_started")? != 0;
    state.phase = if state.script_started {
        "script"
    } else {
        "preamble"
    }
    .into();
    state.readiness = unsigned(run, "readiness")?;
    state.awaited = serde_json::from_value(parse_optional(run, "awaited", json!([]))?)
        .map_err(|e| corrupt(e.to_string()))?;
    state.parked = text(run, "state")? == "suspended";
    if let Some(deadline) = run.get("deadline_at").and_then(Value::as_i64) {
        let first: Option<i64> = c.query_row(
            "SELECT MIN(started_at) FROM activations WHERE run_id=?1",
            [id],
            |r| r.get(0),
        )?;
        state.claim = Some(records::Claim {
            first_claim_ms: first.unwrap_or(state.admitted_ms),
            deadline_at: deadline,
        });
    }
    for row in legacy::calls(c, id)? {
        let position = unsigned(&row, "position")?;
        if position != state.calls.len() as u64 {
            return Err(corrupt("noncontiguous live journal"));
        }
        state.deadline_exempt |= integer(&row, "scope_cut")? != 0;
        let call = call(c, &row, state.admitted_ms)?;
        if let Some(order) = call.delivery_order {
            state.next_delivery_order = state.next_delivery_order.max(
                order
                    .checked_add(1)
                    .ok_or_else(|| corrupt("delivery order exhausted"))?,
            );
        }
        state.calls.insert(position, call);
    }
    let mut arrivals: Vec<_> = state
        .calls
        .iter()
        .filter_map(|(p, c)| {
            c.arrival
                .as_ref()
                .map(|a| (*p, c.delivery_order.is_none(), a.arrival_seq))
        })
        .collect();
    arrivals.sort_by_key(|(_, pending, seq)| (*pending, *seq));
    for (seq, (position, _, _)) in arrivals.into_iter().enumerate() {
        state
            .calls
            .get_mut(&position)
            .unwrap()
            .arrival
            .as_mut()
            .unwrap()
            .arrival_seq = seq as u64;
        state.next_arrival_seq = seq as u64 + 1;
    }
    // Old kv effects are already in SQLite, so the migrated overlay is empty.
    for row in legacy::rows(c, "SELECT * FROM event_body_journal WHERE run_id=?1", [id])? {
        state.event_bodies.push(Body::EventBody {
            digest: optional_text(&row, "digest").unwrap_or_default(),
            body: match row.get("body") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(v) if !v.is_null() => Some(
                    String::from_utf8(
                        serde_json::from_value(v["blob"].clone())
                            .map_err(|e| corrupt(e.to_string()))?,
                    )
                    .map_err(|_| corrupt("invalid event body UTF-8"))?,
                ),
                _ => None,
            },
            refusal: optional_text(&row, "last_provider_refusal"),
            attempt: unsigned(&row, "attempt")?,
            retry_at: row.get("retry_at").and_then(Value::as_i64),
        });
    }
    Ok(state)
}

pub(super) fn finished(tx: &Transaction<'_>) -> Result<()> {
    let source_details = audit::source_details(tx)?;
    for id in legacy::run_ids(tx, true)? {
        let run = legacy::run(tx, &id)?;
        let identity = identity(&run)?;
        let state = state(tx, &run)?;
        let ended = integer(&run, "ended_at")?;
        let result = optional_text(&run, "result")
            .map(|s| serde_json::from_str::<Value>(&s).map_err(|e| corrupt(e.to_string())))
            .transpose()?;
        let size = optional_text(&run, "result").map_or(0, |s| s.len() as u64);
        let mut counts = json!({"class":{},"outcome":{}});
        let mut writes = 0;
        for call in state.calls.values() {
            let class = call.identity.class.as_str();
            let outcome = if call.sync.is_some() {
                "fulfilled"
            } else if let Some(a) = &call.arrival {
                a.outcome.as_str()
            } else if call.unknown.is_some() {
                "unknown"
            } else {
                "pending"
            };
            counts["class"][class] = Value::from(counts["class"][class].as_u64().unwrap_or(0) + 1);
            counts["outcome"][outcome] =
                Value::from(counts["outcome"][outcome].as_u64().unwrap_or(0) + 1);
            if matches!(
                call.identity.class,
                StoredClass::Mutation | StoredClass::KeyedMutation
            ) {
                writes += call.intent.as_ref().map_or(0, |i| i.attempts);
            }
        }
        let tokens:u64=tx.query_row("SELECT COALESCE(SUM(CASE state WHEN 'reserved' THEN reserved ELSE input_tokens+cache_write_tokens+output_tokens+unreported_tokens END),0) FROM token_ledger WHERE run_id=?1",[&id],|r|r.get(0))?;
        let cost:f64=tx.query_row("SELECT COALESCE(SUM(COALESCE(charged,reserved)),0) FROM classify_costs WHERE kind='flow' AND run_id=?1",[&id],|r|r.get(0))?;
        let history = sql::RunHistory {
            run_id: id.clone(),
            flow_id: identity.flow_id.clone(),
            flow_version: identity.flow_version,
            trigger_id: text(&run, "trigger_id")?.into(),
            trigger_kind: trigger_kind(&run)?,
            trigger: legacy::json(&run, "trigger")?,
            admitted_at: state.admitted_ms,
            admit_seq: admit_seq(&run)?,
            first_claim_at: state.claim.as_ref().map(|c| c.first_claim_ms),
            ended_at: ended,
            state: RunState::parse(text(&run, "state")?)?,
            error_kind: bounded(optional_text(&run, "error_kind"), 256),
            error_detail: bounded(optional_text(&run, "error_detail"), 8192),
            result,
            result_size: size,
            call_counts: counts,
            writes_sent: writes,
            tokens_charged: tokens,
            cost_microusd: (cost * 1_000_000.0).ceil() as u64,
            broken: u32::try_from(state.broken).map_err(|_| corrupt("broken counter overflow"))?,
            activation_count: state.activation_count,
            script_started: state.script_started,
            phase: state.phase.clone(),
            deadline_exempt: state.deadline_exempt,
            history_lost: false,
            commit_seq: sql::next_commit_seq(tx)?,
        };
        sql::insert_history(tx, &history, &identity)?;
        tx.execute("INSERT OR IGNORE INTO tombstones(kind,key,run_id,pruned_at) VALUES('trigger',?1,?2,?3)",params![crate::admission::trigger_key(&history.flow_id,&history.trigger_id),id,ended])?;
        for row in legacy::calls(tx, &id)? {
            let position = unsigned(&row, "position")?;
            let call = &state.calls[&position];
            if matches!(
                call.identity.class,
                StoredClass::Mutation | StoredClass::KeyedMutation
            ) {
                tx.execute("INSERT OR IGNORE INTO tombstones(kind,key,run_id,pruned_at) VALUES('idempotency_key',?1,?2,?3)",params![text(&row,"idempotency_key")?,id,ended])?;
            }
            let mut detail = serde_json::to_value(call).map_err(|e| corrupt(e.to_string()))?;
            detail["legacy"] = Value::Object(row.clone());
            if history.state != RunState::Succeeded {
                sql::insert_call(
                    tx,
                    &sql::RunCall {
                        run_id: id.clone(),
                        position,
                        kind: call.identity.kind.clone(),
                        class: call.identity.class,
                        args_digest: call.identity.args_digest.0,
                        outcome: if call.sync.is_some() {
                            sql::CallOutcome::Fulfilled
                        } else if let Some(a) = &call.arrival {
                            if a.settlement == Settlement::Fulfilled {
                                sql::CallOutcome::Fulfilled
                            } else {
                                sql::CallOutcome::Rejected
                            }
                        } else if call.unknown.is_some() {
                            sql::CallOutcome::Unknown
                        } else {
                            sql::CallOutcome::Pending
                        },
                        error_code: call
                            .arrival
                            .as_ref()
                            .filter(|a| a.settlement == Settlement::Rejected)
                            .and_then(|a| serde_json::from_str::<Value>(a.value.as_str()).ok())
                            .and_then(|v| v["code"].as_str().map(str::to_owned)),
                        issued_at: audit_at(tx, &id, position, state.admitted_ms)?,
                        arrived_at: None,
                        released_at: None,
                        detail: detail.clone(),
                    },
                )?;
            }
            let sources = source_details
                .get(&(id.clone(), position))
                .cloned()
                .unwrap_or_default();
            detail["migration_sources"] =
                serde_json::to_value(&sources).map_err(|e| corrupt(e.to_string()))?;
            let remote = call.intent.is_some();
            let receipt = call
                .arrival
                .as_ref()
                .map(|a| sql::Receipt::Value(a.value_hash));
            if remote || !sources.is_empty() {
                sql::insert_obligation(
                    tx,
                    &sql::Obligation {
                        run_id: id.clone(),
                        position,
                        handle: call
                            .arrival
                            .as_ref()
                            .and_then(|a| a.handle.clone())
                            .or_else(|| call.accepted.clone()),
                        state: if receipt.is_some() {
                            sql::ObligationState::Settled
                        } else if call.unknown.is_some() {
                            sql::ObligationState::Unknown
                        } else {
                            sql::ObligationState::Open
                        },
                        audit_at: audit_at(tx, &id, position, state.admitted_ms)?,
                        detail,
                        receipt,
                        settled_at: call.arrival.as_ref().map(|_| ended),
                    },
                )?;
            }
        }
    }
    audit::after_finished(tx)
}

pub(super) struct LiveImage {
    rows: Vec<(sql::RunIdentity, sql::LiveRun)>,
    watermark: sql::EpochLsn,
}
impl LiveImage {
    pub(super) fn apply(&self, tx: &Transaction<'_>) -> Result<()> {
        for (id, row) in &self.rows {
            sql::insert_live(tx, row, id)?;
        }
        sql::advance_log_hwm(tx, self.watermark)
    }
}
fn config(store: &Store, directory: &Path, meta: &sql::Meta) -> Result<run_log::Config> {
    Ok(run_log::Config {
        directory: directory.to_owned(),
        identity: run_log::Identity {
            store_id: meta
                .store_id
                .as_bytes()
                .try_into()
                .map_err(|_| corrupt("invalid store id"))?,
            log_epoch: meta.log_epoch,
        },
        lease_epoch: store.lease_epoch(),
        target_size: u64::MAX,
        hooks: Arc::new(NoHooks),
    })
}
fn log_error(error: run_log::Error) -> crate::CoreError {
    crate::CoreError::Store(format!("run-log migration: {error}"))
}

pub(super) fn live(store: &Arc<Store>, directory: &Path) -> Result<LiveImage> {
    let meta = store.read(sql::meta)?;
    let config = config(store, directory, &meta)?;
    let registry = Arc::new(run_log::SqliteRegistry::new(store.clone(), meta.log_epoch));
    let reset = store.clone();
    let log = RunLog::<Record>::create_migration(config, registry, move || {
        reset
            .write(|tx| {
                let n: i64 = tx.query_row("SELECT COUNT(*) FROM live_runs", [], |r| r.get(0))?;
                if n != 0 || super::phase_from_connection(tx)? != super::Phase::LiveRuns {
                    return Err(corrupt("cannot replace a published migration log"));
                }
                tx.execute("DELETE FROM run_log_segments", [])?;
                let mut meta = sql::meta(tx)?;
                meta.log_hwm.lsn = Lsn {
                    sequence: 1,
                    offset: 0,
                };
                meta.gc_floor.synced_lsn = meta.log_hwm.lsn;
                sql::write_meta(tx, &meta)
            })
            .map_err(|e| run_log::Error::Registry(e.to_string()))
    })
    .map_err(log_error)?;
    let base = log.synced_lsn();
    let mut rows = Vec::new();
    let mut end = base;
    let mut without_refund = 0u64;
    for id in store.read(|c| legacy::run_ids(c, false))? {
        let (run, state) = store.read(|c| {
            let row = legacy::run(c, &id)?;
            let state = state(c, &row)?;
            Ok((row, state))
        })?;
        let identity = identity(&run)?;
        without_refund += state
            .calls
            .values()
            .filter(|c| c.intent.is_some() && c.arrival.is_none())
            .count() as u64;
        let live = sql::LiveRun {
            run_id: id.clone(),
            flow_id: identity.flow_id.clone(),
            flow_version: identity.flow_version,
            trigger_id: text(&run, "trigger_id")?.into(),
            trigger_kind: trigger_kind(&run)?,
            trigger: legacy::json(&run, "trigger")?,
            attempt: u32::try_from(unsigned(&run, "attempt")?)
                .map_err(|_| corrupt("attempt overflow"))?,
            admitted_at: integer(&run, "admitted_at")?,
            admit_seq: admit_seq(&run)?,
            admit_lsn: sql::EpochLsn {
                log_epoch: meta.log_epoch,
                lsn: base,
            },
            deadline_ms: run
                .get("deadline_ms")
                .and_then(Value::as_i64)
                .unwrap_or(300_000),
            self_input: parse_optional(&run, "self_input", Value::Null)?,
            params_input: parse_optional(&run, "params_input", Value::Null)?,
            grant_guards: parse_optional(&run, "grant_guards", json!([]))?,
            template_incarnation: run.get("template_incarnation").and_then(Value::as_i64),
            end_instance: integer(&run, "end_instance")? != 0,
        };
        end = log
            .append(vec![Record {
                run_id: id,
                generation: state.generation,
                body: Body::Snapshot {
                    cutoff_lsn: base,
                    next_delivery_order: state.next_delivery_order,
                    next_arrival_seq: state.next_arrival_seq,
                    state: Box::new(state),
                },
            }])
            .map_err(log_error)?;
        rows.push((identity, live));
    }
    let synced = log.sync_through(end).map_err(log_error)?;
    tracing::info!(target:"migration", legacy_inflight_dispatches_without_refund=without_refund,"legacy in-flight dispatches kept without refund");
    Ok(LiveImage {
        rows,
        watermark: sql::EpochLsn {
            log_epoch: meta.log_epoch,
            lsn: synced,
        },
    })
}

pub(super) fn snapshots(
    store: &Arc<Store>,
    directory: &Path,
) -> Result<Vec<run_log::RecoveredRecord<Record>>> {
    let meta = store.read(sql::meta)?;
    let (log, recovered) = RunLog::<Record>::open(
        config(store, directory, &meta)?,
        Arc::new(run_log::SqliteRegistry::new(store.clone(), meta.log_epoch)),
    )
    .map_err(log_error)?;
    drop(log);
    Ok(recovered.records)
}

pub(super) fn broca(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("DROP TRIGGER IF EXISTS broca_calls_forget_wait;
        -- A single FK cannot own a call in both live_runs and run_history.
        -- The storage API validates union ownership inside its write transaction.
        CREATE TABLE cutover_broca_calls(send_id TEXT PRIMARY KEY,run_id TEXT NOT NULL,position INTEGER NOT NULL CHECK(position>=0),snapshot TEXT NOT NULL,UNIQUE(run_id,position));
        INSERT INTO cutover_broca_calls SELECT * FROM broca_calls;
        DROP TABLE broca_calls;
        ALTER TABLE cutover_broca_calls RENAME TO broca_calls;
        CREATE INDEX broca_calls_pending ON broca_calls(send_id) WHERE json_extract(snapshot,'$.acknowledged')=0 AND COALESCE(json_extract(snapshot,'$.deferred'),0)=0;")?;
    super::check_broca_ownership(tx)?;
    audit::check_broca(tx)
}
pub(super) fn drop_legacy(tx: &Transaction<'_>) -> Result<()> {
    audit::drop_dependencies(tx)?;
    tx.execute_batch("DROP VIEW IF EXISTS runs_with_snapshots;DROP VIEW IF EXISTS journal_with_payloads;DROP TABLE IF EXISTS mailbox;DROP TABLE IF EXISTS journal_payloads;DROP TABLE IF EXISTS event_body_journal;DROP TABLE IF EXISTS call_audit;DROP TABLE IF EXISTS activations;DROP TABLE IF EXISTS journal;DROP TABLE IF EXISTS runs;")?;
    audit::drop_dependencies(tx)
}
