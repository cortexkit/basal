use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::{
    CoreError, Result,
    run_records::{Intent, State},
    run_store::{Reservation, Window},
};

/// Window sizes and limits are frozen by the embedding runtime. Zero-sized
/// windows are rejected before any dispatch.
#[derive(Clone, Debug)]
pub struct Budgets {
    pub dispatch_window_ms: i64,
    pub token_window_ms: i64,
    pub dispatches: u64,
    pub tokens: u64,
    pub cost_micros: u64,
    pub cost_window: Option<(i64, u64)>,
}

pub(super) fn integer(n: u64) -> Result<i64> {
    i64::try_from(n).map_err(|_| CoreError::Invalid("accounting integer overflow".into()))
}

fn window(kind: &str, ms: i64, start: i64) -> Window {
    Window(format!("{kind}:{ms}:{start}"))
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| CoreError::Invalid("accounting overflow".into()))
}

pub(crate) fn rows(state: &State, budgets: &Budgets) -> Result<Vec<Reservation>> {
    if budgets.dispatch_window_ms <= 0 || budgets.token_window_ms <= 0 {
        return Err(CoreError::Invalid(
            "accounting windows must be positive".into(),
        ));
    }
    let mut rows = Vec::new();
    for call in state.calls.values() {
        for intent in call.prior_intents.iter().chain(call.intent.iter()) {
            if intent.attempts <= call.pre_accounted_attempts {
                continue;
            }
            if let Some(count) = &intent.dispatch_count_window {
                if call
                    .intent
                    .as_ref()
                    .is_some_and(|current| std::ptr::eq(current, intent))
                    && call.deferred.as_ref().is_some_and(|d| d.proven_unsent)
                {
                    continue;
                }
                rows.push(Reservation {
                    position: call.identity.position,
                    window: window("dispatch", budgets.dispatch_window_ms, count.window_start),
                    audit_at: intent.audit_at,
                    amount: count.count,
                });
            }
        }
        if let Some(intent) = &call.intent
            && !call.pre_accounted_reservation
            && let Some(reservation) = &intent.reservation
        {
            let usage = call.arrival.as_ref().and_then(|a| a.usage.as_ref());
            if reservation
                .input_tokens
                .saturating_add(reservation.output_tokens)
                > 0
            {
                rows.push(Reservation {
                    position: call.identity.position,
                    window: window("token", budgets.token_window_ms, reservation.window_start),
                    audit_at: intent.audit_at,
                    amount: match usage {
                        Some(u) => add(u.input_tokens, u.output_tokens)?,
                        None => add(reservation.input_tokens, reservation.output_tokens)?,
                    },
                });
            }
            rows.push(Reservation {
                position: call.identity.position,
                window: window("cost", budgets.token_window_ms, reservation.window_start),
                audit_at: call
                    .prior_intents
                    .first()
                    .map_or(intent.audit_at, |i| i.audit_at),
                amount: call
                    .arrival
                    .as_ref()
                    .and_then(|a| a.cost_micros)
                    .unwrap_or(reservation.cost_micros),
            });
        }
    }
    Ok(rows)
}

pub(super) fn check(
    conn: &Connection,
    flow: &str,
    run_id: &str,
    budgets: &Budgets,
    others: &[Reservation],
    proposed: &[Reservation],
) -> Result<()> {
    let dollars = |run: &str| -> Result<u64> {
        let n: f64 = conn.query_row("SELECT COALESCE(SUM(COALESCE(charged,reserved)),0) FROM classify_costs WHERE kind='flow' AND run_id=?1", [run], |r| r.get(0))?;
        if !n.is_finite() || n < 0.0 || n * 1_000_000.0 > u64::MAX as f64 {
            return Err(CoreError::Corrupt("invalid dollar accounting".into()));
        }
        Ok((n * 1_000_000.0).ceil() as u64)
    };
    let cost = proposed
        .iter()
        .filter(|r| r.window.0.starts_with("cost:"))
        .try_fold(0, |n, r| add(n, r.amount))?;
    if add(dollars(run_id)?, cost)? > budgets.cost_micros {
        return Err(CoreError::Invalid("cost budget exhausted".into()));
    }
    if let Some((ms, cap)) = budgets.cost_window {
        let now = proposed.iter().map(|r| r.audit_at).max().unwrap_or(0);
        let floor = now.saturating_sub(ms);
        let committed: f64 = conn.query_row("SELECT COALESCE(SUM(COALESCE(charged,reserved)),0) FROM classify_costs WHERE kind='flow' AND flow_id=?1 AND created_at>?2", params![flow,floor], |r| r.get(0))?;
        let pending = others
            .iter()
            .chain(proposed)
            .filter(|r| r.window.0.starts_with("cost:") && r.audit_at > floor)
            .try_fold(0, |n, r| add(n, r.amount))?;
        if !committed.is_finite() || committed < 0.0 || committed * 1_000_000.0 > u64::MAX as f64 {
            return Err(CoreError::Corrupt("invalid dollar accounting".into()));
        }
        if add((committed * 1_000_000.0).ceil() as u64, pending)? > cap {
            return Err(CoreError::Invalid("cost window budget exhausted".into()));
        }
    }
    let mut totals = BTreeMap::<Window, u64>::new();
    // Dollar ceilings are per run; other live runs of the flow contribute only
    // dispatch and token windows. The proposed rows contain this run's entirety.
    for row in others
        .iter()
        .chain(proposed)
        .filter(|r| !r.window.0.starts_with("cost:"))
    {
        let n = totals.entry(row.window.clone()).or_default();
        *n = add(*n, row.amount)?;
    }
    for (window, live) in totals {
        let mut parts = window.0.split(':');
        let kind = parts.next().unwrap_or_default();
        let ms: i64 = parts
            .next()
            .unwrap_or_default()
            .parse()
            .map_err(|_| CoreError::Invalid("invalid accounting window".into()))?;
        let start: i64 = parts
            .next()
            .unwrap_or_default()
            .parse()
            .map_err(|_| CoreError::Invalid("invalid accounting window".into()))?;
        let (sql, limit) = match kind {
            "dispatch" => (
                "SELECT dispatches FROM rate_windows WHERE flow_id=?1 AND window_ms=?2 AND window_start=?3",
                budgets.dispatches,
            ),
            "token" => (
                "SELECT reserved+input_tokens+cache_write_tokens+output_tokens+unreported_tokens FROM token_windows WHERE flow_id=?1 AND window_ms=?2 AND window_start=?3",
                budgets.tokens,
            ),
            _ => return Err(CoreError::Invalid("invalid accounting resource".into())),
        };
        let committed: i64 = conn
            .query_row(sql, params![flow, ms, start], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        let committed = u64::try_from(committed)
            .map_err(|_| CoreError::Corrupt("negative committed accounting".into()))?;
        if add(committed, live)? > limit {
            return Err(CoreError::Invalid(format!("{kind} budget exhausted")));
        }
    }
    Ok(())
}

pub(crate) fn fold(
    tx: &Transaction<'_>,
    flow: &str,
    id: &str,
    state: &State,
    budgets: &Budgets,
    now: i64,
) -> Result<()> {
    for call in state.calls.values() {
        for intent in call.prior_intents.iter().chain(call.intent.iter()) {
            if intent.attempts <= call.pre_accounted_attempts {
                continue;
            }
            if let Some(count) = &intent.dispatch_count_window {
                if call
                    .intent
                    .as_ref()
                    .is_some_and(|current| std::ptr::eq(current, intent))
                    && call.deferred.as_ref().is_some_and(|d| d.proven_unsent)
                {
                    continue;
                }
                tx.execute("INSERT INTO rate_windows(flow_id,window_ms,window_start,dispatches) VALUES(?1,?2,?3,?4) ON CONFLICT(flow_id,window_ms,window_start) DO UPDATE SET dispatches=dispatches+excluded.dispatches",
                    params![flow, budgets.dispatch_window_ms, count.window_start, integer(count.count)?])?;
            }
        }
        let Some(intent) = &call.intent else {
            continue;
        };
        if call.pre_accounted_reservation {
            if let Some(arrival) = &call.arrival {
                let outcome = basal_host::HostOutcome {
                    settlement: arrival.settlement,
                    value: arrival.value.clone(),
                    usage: arrival.usage.as_ref().map(|u| basal_host::TokenUsage {
                        input_tokens: Some(u.input_tokens),
                        output_tokens: Some(u.output_tokens),
                        ..Default::default()
                    }),
                };
                crate::tokens::settle_outcome(tx, id, call.identity.position, &outcome, now)?;
                let value = serde_json::from_str::<serde_json::Value>(arrival.value.as_str())
                    .map_err(|e| CoreError::Corrupt(e.to_string()))?;
                crate::classify_cost::settle(
                    tx,
                    "flow",
                    id,
                    call.identity.position,
                    Some(&value),
                    None,
                )?;
            } else if call.deferred.as_ref().is_some_and(|d| d.proven_unsent) {
                crate::tokens::settle(
                    tx,
                    crate::tokens::Key::Call {
                        run_id: id,
                        position: call.identity.position,
                    },
                    crate::tokens::Report::NoEffect,
                    now,
                )?;
                crate::classify_cost::settle(
                    tx,
                    "flow",
                    id,
                    call.identity.position,
                    None,
                    Some("not_dispatched"),
                )?;
            }
            continue;
        }
        let Some(reservation) = &intent.reservation else {
            continue;
        };
        let reserved = add(reservation.input_tokens, reservation.output_tokens)?;
        let reserved_at = call
            .prior_intents
            .first()
            .map_or(intent.audit_at, |i| i.audit_at);
        let settled = call.arrival.is_some();
        let usage = call.arrival.as_ref().and_then(|a| a.usage.as_ref());
        let input = usage.map_or(0, |u| u.input_tokens);
        let output = usage.map_or(0, |u| u.output_tokens);
        let unreported = if settled && usage.is_none() {
            reserved
        } else {
            0
        };
        let pending = if settled { 0 } else { reserved };
        let key = crate::ids::idempotency_key(flow, id, call.identity.position);
        if reserved > 0 {
            tx.execute("INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,input_tokens,output_tokens,unreported_tokens,reserved_at,settled_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![key, flow, id, integer(call.identity.position)?, budgets.token_window_ms, reservation.window_start, integer(reserved)?, if settled {"settled"} else {"reserved"}, integer(input)?, integer(output)?, integer(unreported)?, reserved_at, settled.then_some(now)])?;
            tx.execute("INSERT INTO token_windows(flow_id,window_ms,window_start,reserved,input_tokens,output_tokens,unreported_tokens) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(flow_id,window_ms,window_start) DO UPDATE SET reserved=reserved+excluded.reserved,input_tokens=input_tokens+excluded.input_tokens,output_tokens=output_tokens+excluded.output_tokens,unreported_tokens=unreported_tokens+excluded.unreported_tokens",
            params![flow, budgets.token_window_ms, reservation.window_start, integer(pending)?, integer(input)?, integer(output)?, integer(unreported)?])?;
        }
        fold_cost(
            tx,
            flow,
            id,
            intent,
            reserved_at,
            call.arrival.as_ref().and_then(|a| a.cost_micros),
            settled,
        )?;
    }
    Ok(())
}

fn fold_cost(
    tx: &Transaction<'_>,
    flow: &str,
    id: &str,
    intent: &Intent,
    created_at: i64,
    cost: Option<u64>,
    settled: bool,
) -> Result<()> {
    let Some(reservation) = &intent.reservation else {
        return Ok(());
    };
    if reservation.cost_micros == 0
        && cost.is_none()
        && reservation
            .input_tokens
            .saturating_add(reservation.output_tokens)
            > 0
    {
        return Ok(());
    }
    tx.execute("INSERT INTO classify_costs(kind,flow_id,run_id,position,created_at,reserved,charged,args) VALUES('flow',?1,?2,?3,?4,?5,?6,?7)",
        params![flow, id, integer(intent.call.position)?, created_at, reservation.cost_micros as f64 / 1_000_000.0, if settled { Some(cost.unwrap_or(reservation.cost_micros) as f64 / 1_000_000.0) } else { None }, intent.frozen_request.as_str()])?;
    Ok(())
}
