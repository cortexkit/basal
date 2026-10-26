//! Dollar accounting shared by program and flow calls to the classifier tool.

use basal_host::transport::WireError;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

/// Recognizes the contract rather than a particular provider or tool alias.
pub fn matches(entry: &Value) -> bool {
    // The contract fixes only the tag, not a tool name. Untagged program tools remain
    // ordinary calls with no Basal dollar accounting; Broca still enforces their
    // required per-call max_cost_usd until its catalog advertises this tag.
    entry["capabilities"].as_array().is_some_and(|tags| {
        tags.iter()
            .any(|tag| tag == cortexkit_llm_tools::classify::TAG)
    })
}

pub fn requires_ceiling(entry: &Value) -> bool {
    entry["input_schema"]["required"]
        .as_array()
        .is_some_and(|required| required.iter().any(|field| field == "max_cost_usd"))
}

pub const DEFAULT_USD: f64 = 0.50;

/// Leaves invalid caller ceilings untouched so schema validation names the field.
pub fn clamp(input: &mut Value, remaining: f64) {
    if input.get("max_cost_usd").is_none() {
        input["max_cost_usd"] = remaining.into();
    } else if let Some(wanted) = input["max_cost_usd"].as_f64().filter(|v| *v >= 0.0) {
        input["max_cost_usd"] = wanted.min(remaining).into();
    }
}

fn exhausted() -> WireError {
    WireError::RefusedDetails {
        code: "budget_exhausted".into(),
        message: "model_cost_usd budget exhausted".into(),
        detail: serde_json::json!({"budget":"model_cost_usd","reason":"model_cost_usd"}),
    }
}

pub fn spent(conn: &Connection, kind: &str, run: &str) -> crate::Result<f64> {
    Ok(conn.query_row("SELECT COALESCE(SUM(COALESCE(charged,reserved)),0) FROM classify_costs WHERE kind=?1 AND run_id=?2",
        params![kind,run], |r| r.get(0))?)
}

/// Reserves the whole ceiling atomically, including concurrent in-flight calls.
#[allow(clippy::too_many_arguments)]
pub fn reserve(
    tx: &Transaction<'_>,
    kind: &str,
    run: &str,
    position: u64,
    flow: Option<&str>,
    now: i64,
    per_run: f64,
    window: Option<(i64, f64)>,
    input: &mut Value,
) -> crate::Result<Result<(), WireError>> {
    if !input.is_object() {
        return Ok(Err(WireError::Refused {
            code: "invalid_params".into(),
            message: "classifier arguments must be an object".into(),
        }));
    }
    let saved: Option<String> = tx
        .query_row(
            "SELECT args FROM classify_costs WHERE kind=?1 AND run_id=?2 AND position=?3",
            params![kind, run, position],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(saved) = saved {
        *input =
            serde_json::from_str(&saved).map_err(|e| crate::CoreError::Corrupt(e.to_string()))?;
        return Ok(Ok(()));
    }
    let mut remaining = per_run - spent(tx, kind, run)?;
    if let Some((window_ms, cap)) = window {
        let used: f64 = tx.query_row("SELECT COALESCE(SUM(COALESCE(charged,reserved)),0) FROM classify_costs WHERE kind='flow' AND flow_id=?1 AND created_at>?2",
            params![flow, now.saturating_sub(window_ms)], |r| r.get(0))?;
        remaining = remaining.min(cap - used);
    }
    if remaining <= 0.0 {
        return Ok(Err(exhausted()));
    }
    clamp(input, remaining);
    let Some(reserved) = input["max_cost_usd"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
    else {
        return Ok(Err(WireError::RefusedDetails {
            code: "invalid_params".into(),
            message: "max_cost_usd must be non-negative".into(),
            detail: serde_json::json!({"field":"max_cost_usd"}),
        }));
    };
    tx.execute("INSERT INTO classify_costs(kind,run_id,position,flow_id,created_at,reserved,args) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![kind,run,position,flow,now,reserved,input.to_string()])?;
    Ok(Ok(()))
}

/// An unknown outcome keeps its reservation; a known pre-model refusal is free.
pub fn settle(
    tx: &Transaction<'_>,
    kind: &str,
    run: &str,
    position: u64,
    value: Option<&Value>,
    code: Option<&str>,
) -> crate::Result<()> {
    let reserved: Option<f64> = tx.query_row("SELECT reserved FROM classify_costs WHERE kind=?1 AND run_id=?2 AND position=?3 AND charged IS NULL",
        params![kind,run,position], |r| r.get(0)).optional()?;
    let Some(reserved) = reserved else {
        return Ok(());
    };
    let code = code.or_else(|| value.and_then(|v| v["errorCode"].as_str()));
    let value = value.map(|v| v.get("structuredContent").unwrap_or(v));
    let charged = value
        .and_then(|v| v["cost_usd"].as_f64())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or_else(|| match code {
            Some(
                "invalid_params"
                | "scope_required"
                | "flow_scope_required"
                | "cost_unpriceable"
                | "cost_exceeded"
                | "provider_not_allowed"
                | "not_started"
                | "tool_unavailable"
                | "denied"
                | "cancelled",
            ) => 0.0,
            _ => reserved,
        });
    tx.execute("UPDATE classify_costs SET charged=?4 WHERE kind=?1 AND run_id=?2 AND position=?3 AND charged IS NULL",
        params![kind,run,position,charged])?;
    Ok(())
}
