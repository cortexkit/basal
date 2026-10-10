//! Basal-owned model tools share flow completion and classification handling.
//! Each run reserves its calls and tokens before routing, then freezes the
//! routing decision in SQLite before Broca can receive the request.

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
            "required":["prompt"],"additionalProperties":false}},
        {"name":"classify","module":"basal","op":"classify","input_schema":{
            "type":"object","properties":{"text":{"type":"string"},
                "labels":{"type":"array","minItems":1,"items":{"type":"string"}},
                "iq":iq,"eq":eq},
            "required":["text","labels"],"additionalProperties":false}}
    ])
}

pub fn is_local(module: &str, op: &str) -> bool {
    module == "basal" && matches!(op, "model" | "classify")
}

pub fn add_catalog(catalog: &Value) -> Result<Value, &'static str> {
    let mut entries = catalog
        .as_array()
        .ok_or("catalog must be an array")?
        .clone();
    if entries
        .iter()
        .any(|t| matches!(t["name"].as_str(), Some("model" | "classify")))
    {
        return Err("model and classify are reserved tool names");
    }
    entries.extend(self::catalog().as_array().unwrap().iter().cloned());
    Ok(Value::Array(entries))
}

pub fn provider_ready(transport: &dyn Transport) -> Result<(), WireError> {
    let catalog = transport.catalog()?;
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

/// Repeated dispatches read the saved envelope rather than consulting routing.
pub fn prepare(
    store: &Store,
    selector: &dyn ModelSelector,
    run: &RunRecord,
    call: &HostCall,
) -> Result<CallRequest, WireError> {
    let primitive = match &call.kind {
        CallKind::Tool { name } if name == "model" => Primitive::Llm,
        CallKind::Tool { name } if name == "classify" => Primitive::Classify,
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
        let max_output = if primitive == Primitive::Classify {
            crate::tokens::CLASSIFY_MAX_OUTPUT
        } else {
            input["max_output"]
                .as_u64()
                .map(|n| n as u32)
                .unwrap_or(DEFAULT_MAX_OUTPUT)
        };
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
            caller_class: "codemode".into(),
            iq: input["iq"].as_f64().map(|n| n as u32).unwrap_or(50),
            eq: input["eq"].as_f64().map(|n| n as u32).unwrap_or(0),
            flow_id: flow_id.clone(),
            run_id: run.run_id.clone(),
            send_id: send_id.clone(),
        });
        let selection = match selection {
            Ok(selection) => selection,
            Err(error) => {
                store
                    .write(|tx| settle(tx, &run.run_id, call.position, None, true))
                    .map_err(storage)?;
                return Err(WireError::Refused {
                    code: "route_unavailable".into(),
                    message: error.to_string(),
                });
            }
        };
        envelope["selection"] = serde_json::to_value(selection).unwrap();
        let envelope = envelope.to_string();
        store.write(|tx| {
            tx.execute("UPDATE codemode_models SET envelope=?3 WHERE run_id=?1 AND position=?2 AND envelope IS NULL",params![run.run_id,call.position,envelope])?;
            Ok(())
        }).map_err(storage)?;
        envelope
    };
    Ok(CallRequest {
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
    let reserved: Option<u64> = tx.query_row("SELECT reserved FROM codemode_models WHERE run_id=?1 AND position=?2 AND charged IS NULL",params![run,position],|r|r.get(0)).optional()?;
    let Some(reserved) = reserved else {
        return Ok(());
    };
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
