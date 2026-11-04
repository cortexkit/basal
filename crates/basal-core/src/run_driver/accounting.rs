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

pub(super) fn rows(state: &State, budgets: &Budgets) -> Result<Vec<Reservation>> {
    if budgets.dispatch_window_ms <= 0 || budgets.token_window_ms <= 0 {
        return Err(CoreError::Invalid(
            "accounting windows must be positive".into(),
        ));
    }
    let mut rows = Vec::new();
    for call in state.calls.values() {
        for intent in call.prior_intents.iter().chain(call.intent.iter()) {
            if let Some(count) = &intent.dispatch_count_window {
                rows.push(Reservation {
                    position: call.identity.position,
                    window: window("dispatch", budgets.dispatch_window_ms, count.window_start),
                    audit_at: intent.audit_at,
                    amount: count.count,
                });
            }
        }
        if let Some(intent) = &call.intent
            && let Some(reservation) = &intent.reservation
        {
            let usage = call.arrival.as_ref().and_then(|a| a.usage.as_ref());
            rows.push(Reservation {
                position: call.identity.position,
                window: window("token", budgets.token_window_ms, reservation.window_start),
                audit_at: intent.audit_at,
                amount: match usage {
                    Some(u) => add(u.input_tokens, u.output_tokens)?,
                    None => add(reservation.input_tokens, reservation.output_tokens)?,
                },
            });
            rows.push(Reservation {
                position: call.identity.position,
                window: window("cost", budgets.token_window_ms, reservation.window_start),
                audit_at: intent.audit_at,
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
    budgets: &Budgets,
    others: &[Reservation],
    proposed: &[Reservation],
) -> Result<()> {
    let mut totals = BTreeMap::<Window, u64>::new();
    for row in others.iter().chain(proposed) {
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
            "cost" => {
                let dollars: f64 = conn.query_row("SELECT COALESCE(SUM(COALESCE(charged,reserved)),0) FROM classify_costs WHERE kind='flow' AND flow_id=?1 AND created_at>=?2 AND created_at<?3", params![flow, start, start.saturating_add(ms)], |r| r.get(0))?;
                if !dollars.is_finite()
                    || dollars < 0.0
                    || (dollars * 1_000_000.0).ceil() > u64::MAX as f64
                {
                    return Err(CoreError::Corrupt(
                        "invalid committed dollar accounting".into(),
                    ));
                }
                if add((dollars * 1_000_000.0).ceil() as u64, live)? > budgets.cost_micros {
                    return Err(CoreError::Invalid("cost budget exhausted".into()));
                }
                continue;
            }
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

pub(super) fn fold(
    tx: &Transaction<'_>,
    flow: &str,
    id: &str,
    state: &State,
    budgets: &Budgets,
    now: i64,
) -> Result<()> {
    for call in state.calls.values() {
        for intent in call.prior_intents.iter().chain(call.intent.iter()) {
            if let Some(count) = &intent.dispatch_count_window {
                tx.execute("INSERT INTO rate_windows(flow_id,window_ms,window_start,dispatches) VALUES(?1,?2,?3,?4) ON CONFLICT(flow_id,window_ms,window_start) DO UPDATE SET dispatches=dispatches+excluded.dispatches",
                    params![flow, budgets.dispatch_window_ms, count.window_start, integer(count.count)?])?;
            }
        }
        let Some(intent) = &call.intent else {
            continue;
        };
        let Some(reservation) = &intent.reservation else {
            continue;
        };
        let reserved = add(reservation.input_tokens, reservation.output_tokens)?;
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
        tx.execute("INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,input_tokens,output_tokens,unreported_tokens,reserved_at,settled_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![key, flow, id, integer(call.identity.position)?, budgets.token_window_ms, reservation.window_start, integer(reserved)?, if settled {"settled"} else {"reserved"}, integer(input)?, integer(output)?, integer(unreported)?, intent.audit_at, settled.then_some(now)])?;
        tx.execute("INSERT INTO token_windows(flow_id,window_ms,window_start,reserved,input_tokens,output_tokens,unreported_tokens) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(flow_id,window_ms,window_start) DO UPDATE SET reserved=reserved+excluded.reserved,input_tokens=input_tokens+excluded.input_tokens,output_tokens=output_tokens+excluded.output_tokens,unreported_tokens=unreported_tokens+excluded.unreported_tokens",
            params![flow, budgets.token_window_ms, reservation.window_start, integer(pending)?, integer(input)?, integer(output)?, integer(unreported)?])?;
        fold_cost(
            tx,
            flow,
            id,
            intent,
            budgets,
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
    budgets: &Budgets,
    cost: Option<u64>,
    settled: bool,
) -> Result<()> {
    let Some(reservation) = &intent.reservation else {
        return Ok(());
    };
    if reservation.cost_micros == 0 && cost.is_none() {
        return Ok(());
    }
    let _ = budgets;
    tx.execute("INSERT INTO classify_costs(kind,flow_id,run_id,position,created_at,reserved,charged,args) VALUES('flow',?1,?2,?3,?4,?5,?6,?7)",
        params![flow, id, integer(intent.call.position)?, intent.audit_at, reservation.cost_micros as f64 / 1_000_000.0, if settled { Some(cost.unwrap_or(reservation.cost_micros) as f64 / 1_000_000.0) } else { None }, intent.frozen_request.as_str()])?;
    Ok(())
}
