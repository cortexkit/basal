//! Dollar accounting shared by program and flow calls to the classifier tool.

use basal_host::transport::WireError;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicy {
    Ordinary,
    Classify,
    LegacyOrdinary,
}

impl ToolPolicy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ordinary => "ordinary",
            Self::Classify => "classify",
            Self::LegacyOrdinary => "legacy_ordinary",
        }
    }
}

pub type ToolPolicies = BTreeMap<(String, String), ToolPolicy>;

/// Reads provider metadata only while validating an install or a dry run.
pub fn snapshot(
    manifest: &crate::Manifest,
    catalog: &dyn basal_host::Catalog,
) -> Result<ToolPolicies, crate::InstallError> {
    let mut policies = ToolPolicies::new();
    for grant in &manifest.ops {
        let policy = policy_for(manifest, catalog, &grant.module, &grant.op)?;
        policies.insert((grant.module.clone(), grant.op.clone()), policy);
    }
    Ok(policies)
}

pub fn policy_for(
    manifest: &crate::Manifest,
    catalog: &dyn basal_host::Catalog,
    module: &str,
    op: &str,
) -> Result<ToolPolicy, crate::InstallError> {
    let entry = catalog.tool_contract(module, op).map_err(|error| {
        crate::InstallError::ToolContractUnreadable {
            module: module.into(),
            op: op.into(),
            reason: format!("{error:?}"),
        }
    })?;
    crate::flow_ops::validate(catalog, module, op, entry.as_ref())?;
    policy_for_entry(manifest, module, op, entry.as_ref())
}

pub(crate) fn policy_for_entry(
    manifest: &crate::Manifest,
    module: &str,
    op: &str,
    entry: Option<&Value>,
) -> Result<ToolPolicy, crate::InstallError> {
    if entry.is_some_and(matches) {
        if manifest.model_cost_usd.is_none() {
            return Err(crate::InstallError::ModelCostCapRequired {
                module: module.into(),
                op: op.into(),
            });
        }
        Ok(ToolPolicy::Classify)
    } else {
        if let Some(entry) = entry
            && requires_ceiling(entry)
        {
            return Err(crate::InstallError::ModelToolUntagged {
                module: module.into(),
                op: op.into(),
            });
        }
        Ok(ToolPolicy::Ordinary)
    }
}

pub fn record_install(
    tx: &Transaction<'_>,
    flow: &str,
    version: u32,
    policies: &ToolPolicies,
) -> crate::Result<()> {
    for ((module, op), policy) in policies {
        tx.execute("INSERT INTO install_tool_policies(flow_id,version,module,op,policy) VALUES (?1,?2,?3,?4,?5)",
            params![flow,version,module,op,policy.as_str()])?;
    }
    Ok(())
}

pub fn record_package(
    tx: &Transaction<'_>,
    package: &str,
    version: u32,
    policies: &ToolPolicies,
) -> crate::Result<()> {
    for ((module, op), policy) in policies {
        tx.execute("INSERT INTO package_tool_policies(package,version,module,op,policy) VALUES (?1,?2,?3,?4,?5)",
            params![package,version,module,op,policy.as_str()])?;
    }
    Ok(())
}

/// Loads the immutable version used by this run, not the flow's current version.
pub fn load(conn: &Connection, flow: &str, version: u32) -> crate::Result<ToolPolicies> {
    let mut stmt = conn.prepare("SELECT module,op,policy FROM install_tool_policies WHERE flow_id=?1 AND version=?2
        UNION ALL SELECT p.module,p.op,p.policy FROM package_tool_policies p JOIN flows f ON f.package=p.package WHERE f.flow_id=?1 AND p.version=?2")?;
    let rows = stmt.query_map(params![flow, version], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut policies = ToolPolicies::new();
    for row in rows {
        let (module, op, policy) = row?;
        let policy = match policy.as_str() {
            "ordinary" => ToolPolicy::Ordinary,
            "classify" => ToolPolicy::Classify,
            "legacy_ordinary" => ToolPolicy::LegacyOrdinary,
            _ => {
                return Err(crate::CoreError::Corrupt(format!(
                    "unknown tool policy {policy}"
                )));
            }
        };
        policies.insert((module, op), policy);
    }
    Ok(policies)
}

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

/// Refusals where no provider call was made, so the call is free.
/// Anything else is charged the reservation because basal can't prove it was
/// free. A `cost_usd` in the reply always takes precedence over this list.
const FREE_REFUSAL_CODES: &[&str] = &[
    "invalid_params",
    "scope_required",
    "flow_scope_required",
    "cost_unpriceable",
    "cost_exceeded",
    "provider_not_allowed",
    "not_started",
    "tool_unavailable",
    "denied",
    "cancelled",
    "parent_scope_pending",
    "invalid_request",
    "tool_schema_changed",
    "tool_semantics_changed",
    "model_unknown",
    "model_not_classifier",
    "image_unavailable",
    "image_source_unavailable",
    "flow_not_approved",
    "unknown_method",
];

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
        .unwrap_or(match code {
            Some(code) if FREE_REFUSAL_CODES.contains(&code) => 0.0,
            _ => reserved,
        });
    tx.execute("UPDATE classify_costs SET charged=?4 WHERE kind=?1 AND run_id=?2 AND position=?3 AND charged IS NULL",
        params![kind,run,position,charged])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrated() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for migration in crate::schema::MIGRATIONS {
            conn.execute_batch(migration.statements).unwrap();
        }
        conn
    }

    fn charged_after_settle(conn: &mut Connection, position: u64, code: &str) -> f64 {
        conn.execute(
            "INSERT INTO classify_costs(kind,run_id,position,flow_id,created_at,reserved,args) VALUES ('program','r',?1,NULL,0,0.10,'{}')",
            params![position],
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        settle(&tx, "program", "r", position, None, Some(code)).unwrap();
        tx.commit().unwrap();
        conn.query_row(
            "SELECT charged FROM classify_costs WHERE kind='program' AND run_id='r' AND position=?1",
            params![position],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn pre_provider_refusals_are_free_while_uncertain_ones_keep_the_reservation() {
        let mut conn = migrated();
        // A couple of the newly listed Broca codes: no provider call was made.
        assert_eq!(charged_after_settle(&mut conn, 0, "parent_scope_pending"), 0.0);
        assert_eq!(charged_after_settle(&mut conn, 1, "tool_schema_changed"), 0.0);
        // These may have reached the provider, so the reservation is kept.
        assert_eq!(charged_after_settle(&mut conn, 2, "provider_error"), 0.10);
        assert_eq!(charged_after_settle(&mut conn, 3, "batch_in_progress"), 0.10);
    }
}
