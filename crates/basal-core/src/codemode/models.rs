//! The `model` tool that basal adds to every codemode run.
//! It sends requests through the same Broca path as a flow's `llm`
//! calls. Each call reserves its share of the run's call and token
//! budget before routing, then saves the routing decision in SQLite before
//! Broca can receive the request, so a resend reuses the same model.

use basal_host::selector::{ModelSelector, SelectionRequest};
use basal_host::transport::{Transport, WireError};
use basal_host::{CallRequest, TokenUsage};
use basal_proto::{CallKind, HostCall, JsonText, Primitive};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use super::store::RunRecord;
use crate::{CoreError, Store};

pub const CALLS: u64 = 20;
pub const TOKENS: u64 = 200_000;
pub const DEFAULT_MAX_OUTPUT: u32 = 4_096;

pub fn catalog() -> Value {
    let iq = json!({"type":"integer","minimum":0,"maximum":100,"default":50,"description":"Required iq score for model selection (0–100). Defaults to 50."});
    let eq = json!({"type":"integer","minimum":0,"maximum":100,"default":0,"description":"Required eq score for model selection (0–100). Defaults to 0."});
    json!([
        {"name":"model","module":"basal","op":"model","input_schema":{
            "type":"object","properties":{"prompt":{"type":"string"},"system":{"type":"string"},
                "max_output":{"type":"integer","minimum":1,"maximum":200_000},
                "iq":iq,"eq":eq},
            "required":["prompt"],"additionalProperties":false}}
    ])
}

pub fn is_local(module: &str, op: &str) -> bool {
    module == "basal" && op == "model"
}

pub fn add_catalog(catalog: &Value) -> Result<Value, &'static str> {
    let mut entries = catalog
        .as_array()
        .ok_or("catalog must be an array")?
        .clone();
    entries.extend(self::catalog().as_array().unwrap().iter().cloned());
    Ok(Value::Array(entries))
}

pub fn provider_ready(transport: &dyn Transport) -> Result<(), WireError> {
    let catalog = transport.catalog()?;
    // Broca is gated on the legacy run-scope capability, not on per-tool
    // call modes: a codemode run reaches Broca to open model routes, not to
    // call one of Broca's tools, so there is no tool whose `modes` could say
    // codemode. Broca declares this capability for that purpose.
    let opted_in = catalog["modules"].as_array().is_some_and(|modules| {
        modules.iter().any(|m| {
            m["module_id"] == "broca"
                && m["capabilities"]["provides"]
                    .as_array()
                    .is_some_and(|caps| caps.iter().any(|cap| cap == "agent-run-scopes/v1"))
        })
    });
    if !opted_in {
        return Err(WireError::RefusedDetails {
            code: "tool_unavailable".into(),
            message: "Broca does not accept run scopes".into(),
            detail: json!({"reason":"provider_not_opted_in"}),
        });
    }
    Ok(())
}

fn storage(error: CoreError) -> WireError {
    WireError::NeverSent(error.to_string())
}
fn budget(reason: &str) -> WireError {
    WireError::RefusedDetails {
        code: "budget_exhausted".into(),
        message: "run model budget exhausted".into(),
        detail: json!({"reason":reason}),
    }
}

/// Builds the Broca request for one model call. The first dispatch asks
/// the routing provider for a model and saves the whole request, including that
/// choice, in `codemode_models`; a resend reads the saved request instead of
/// asking routing again, so it can never switch models.
pub fn prepare(
    store: &Store,
    selector: &dyn ModelSelector,
    run: &RunRecord,
    call: &HostCall,
) -> Result<CallRequest, WireError> {
    let primitive = match &call.kind {
        CallKind::Tool { name } if name == "model" => Primitive::Llm,
        _ => return Err(WireError::NeverSent("not a model tool".into())),
    };
    let saved: Option<String> = store
        .read(|c| {
            Ok(c.query_row(
                "SELECT envelope FROM codemode_models WHERE run_id=?1 AND position=?2",
                params![run.run_id, call.position],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
        })
        .map_err(storage)?;
    let send_id = super::store::call_key(&run.run_id, call.position);
    let flow_id = format!("codemode:{}", run.run_id);
    let envelope = if let Some(saved) = saved {
        saved
    } else {
        let input: Value = serde_json::from_str(call.args.as_str())
            .map_err(|e| WireError::NeverSent(e.to_string()))?;
        let max_output = input["max_output"]
            .as_u64()
            .map(|n| n as u32)
            .unwrap_or(DEFAULT_MAX_OUTPUT);
        let mut envelope = json!({"send_id":send_id,"work_class":flow_id,
            "session":basal_host::broca::codemode_session(&run.run_id,call.position),
            "op":primitive.name(),"max_output":max_output,"request":input});
        let reserve = crate::tokens::estimate_input(envelope.to_string().len())
            .saturating_add(u64::from(max_output));
        // Any single charge above the limit already forbids another call. Cap
        // each term of the sum so oversized provider measurements cannot overflow SQLite.
        let reserved = store.write(|tx| {
            let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM codemode_models WHERE run_id=?1 AND position=?2)",params![run.run_id,call.position],|r|r.get(0))?;
            if exists { return Ok(Ok(())); }
            let (count, used): (u64,u64) = tx.query_row("SELECT COUNT(*), COALESCE(SUM(MIN(COALESCE(charged,reserved),?2)),0) FROM codemode_models WHERE run_id=?1",params![run.run_id,TOKENS+1],|r| Ok((r.get(0)?,r.get(1)?)))?;
            if count >= CALLS { return Ok(Err(budget("model_calls"))); }
            if used.saturating_add(reserve) > TOKENS { return Ok(Err(budget("model_tokens"))); }
            tx.execute("INSERT INTO codemode_models(run_id,position,reserved) VALUES (?1,?2,?3)",params![run.run_id,call.position,reserve])?;
            Ok(Ok(()))
        }).map_err(storage)?;
        reserved?;
        let selection = selector.select(&SelectionRequest {
            input_tokens: reserve.saturating_sub(u64::from(max_output)),
            max_output_tokens: u64::from(max_output),
            caller_class: "codemode".into(),
            iq: input["iq"].as_f64().map(|n| n as u32).unwrap_or(50),
            eq: input["eq"].as_f64().map(|n| n as u32).unwrap_or(0),
            flow_id: flow_id.clone(),
            run_id: run.run_id.clone(),
            send_id: send_id.clone(),
        });
        let selection = selection.and_then(|selection| {
            let price = selection
                .price
                .as_ref()
                .ok_or_else(basal_host::model_cost::price_unknown)?;
            let dollars = basal_host::model_cost::reserve(
                price,
                reserve.saturating_sub(u64::from(max_output)),
                u64::from(max_output),
            )?;
            Ok((selection, dollars))
        });
        let (selection, dollars) = match selection {
            Ok(selection) => selection,
            Err(basal_host::selector::SelectionError::Wait(refusal)) => {
                // Selection has not succeeded or sent anything. Keep the token
                // reservation pending until this same position succeeds or expires.
                return Err(WireError::Typed(*refusal));
            }
            Err(error) => {
                store
                    .write(|tx| settle(tx, &run.run_id, call.position, None, true))
                    .map_err(storage)?;
                return Err(match error {
                    basal_host::selector::SelectionError::Refused { code, detail }
                        if code == "price_unknown" =>
                    {
                        WireError::Refused {
                            code,
                            message: detail,
                        }
                    }
                    other => WireError::Refused {
                        code: "route_unavailable".into(),
                        message: other.to_string(),
                    },
                });
            }
        };
        envelope["selection"] = serde_json::to_value(selection).unwrap();
        let envelope = envelope.to_string();
        let limits: super::admission::Limits =
            serde_json::from_str(&run.limits).map_err(|e| WireError::NeverSent(e.to_string()))?;
        let saved = store.write(|tx| {
            let used = crate::classify_cost::spent(tx, "program", &run.run_id)?;
            if used + dollars as f64 / 1_000_000.0 > limits.model_cost_usd {
                settle(tx, &run.run_id, call.position, None, true)?;
                return Ok(Err(budget("model_cost_usd")));
            }
            tx.execute("INSERT INTO classify_costs(kind,run_id,position,created_at,reserved,args) VALUES('program',?1,?2,?3,?4,?5)",params![run.run_id,call.position,run.admitted_at,dollars as f64 / 1_000_000.0,envelope])?;
            tx.execute("UPDATE codemode_models SET envelope=?3 WHERE run_id=?1 AND position=?2 AND envelope IS NULL",params![run.run_id,call.position,envelope])?;
            Ok(Ok(()))
        }).map_err(storage)?;
        saved?;
        envelope
    };
    Ok(CallRequest {
        flow_version: None,
        flow_id,
        run_id: run.run_id.clone(),
        position: call.position,
        kind: CallKind::Primitive(primitive),
        args: JsonText::new(envelope).map_err(|e| WireError::NeverSent(e.to_string()))?,
        idempotency_key: send_id,
        attempt: 1,
    })
}

/// Fresh input, cache writes and output count against the token limit. When
/// any measurement is missing, charge at least the reservation rather than
/// treating unreported usage as free; cached input does not count.
pub fn settle(
    tx: &Transaction<'_>,
    run: &str,
    position: u64,
    usage: Option<TokenUsage>,
    no_effect: bool,
) -> crate::Result<()> {
    let saved: Option<(u64, Option<String>)> = tx.query_row("SELECT reserved,envelope FROM codemode_models WHERE run_id=?1 AND position=?2 AND charged IS NULL",params![run,position],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((reserved, envelope)) = saved else {
        return Ok(());
    };
    if let Some(envelope) = envelope {
        let dollars: Option<f64> = tx.query_row("SELECT reserved FROM classify_costs WHERE kind='program' AND run_id=?1 AND position=?2 AND charged IS NULL", params![run,position], |r| r.get(0)).optional()?;
        if let Some(dollars) = dollars
            && let Some(cost) = basal_host::model_cost::recorded_charge(
                &envelope,
                usage,
                (dollars * 1_000_000.0).ceil() as u64,
                no_effect,
            )
        {
            tx.execute("UPDATE classify_costs SET charged=?3 WHERE kind='program' AND run_id=?1 AND position=?2 AND charged IS NULL",params![run,position,cost as f64 / 1_000_000.0])?;
        }
    }
    let charged = if no_effect {
        0
    } else if let Some(u) = usage {
        let reported = u
            .input_tokens
            .unwrap_or(0)
            .saturating_add(u.cache_write_tokens.unwrap_or(0))
            .saturating_add(u.output_tokens.unwrap_or(0));
        if u.input_tokens.is_none() || u.cache_write_tokens.is_none() || u.output_tokens.is_none() {
            reported.max(reserved)
        } else {
            reported
        }
    } else {
        reserved
    };
    tx.execute(
        "UPDATE codemode_models SET charged=?3 WHERE run_id=?1 AND position=?2 AND charged IS NULL",
        params![run, position, charged.min(i64::MAX as u64)],
    )?;
    Ok(())
}
