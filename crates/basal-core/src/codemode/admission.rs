//! Ordered admission checks for one-attempt codemode runs.
//!
//! Only a newly committed, running row grants a start token. Repeated ids,
//! refusals and runs already at their deadline cannot register a scope or
//! start a worker. The caller consumes the token after this transaction ends.

use std::collections::BTreeMap;

use basal_host::flow_scope::{FlowScope, RegisteredScope};
use basal_host::{Host, ScopeStatus};
use basal_proto::{MAX_NAME_BYTES, MAX_SCRIPT_BYTES};
use jsonschema::Validator;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subc_protocol::Principal;

use super::catalog::{catalog_digest, compile_input_schema};
use super::store::{self, Budget, Lookup, NewRun, RunError, RunRecord, Status, Terminal};
use crate::clock::Clock;
use crate::error::{CoreError, Result};
use crate::store::Store;

pub const MAX_DESCRIPTION_BYTES: usize = 1_024;
pub const MAX_AGENT_RUNS: u64 = 2;
pub const MAX_BASAL_RUNS: u64 = 16;

/// Injectable so a Linux or macOS test can verify that Windows is refused
/// without requiring a Windows worker or sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Unix,
    Windows,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// Budgets for one codemode run's wall time, tool calls and captured output.
/// These never change the budgets used by approved flow activations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<i64>,
    pub tool_calls: u64,
    pub output_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_ms: None,
            tool_calls: 200,
            output_bytes: 65_536,
        }
    }
}

/// A catalog tool with its compiled input validator. Compiling before the run
/// row commits refuses invalid schemas before any worker can call the tool.
#[derive(Debug)]
pub struct Tool {
    pub module: String,
    pub op: String,
    pub input_schema: Validator,
}

/// Permission to register `codemode:<run_id>` and start its fresh worker.
/// This is deliberately not cloneable: duplicate admits never get a token.
#[derive(Debug)]
pub struct Start {
    pub scope: RegisteredScope,
    pub tools: BTreeMap<String, Tool>,
    pub limits: Limits,
    pub wall_deadline_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
pub enum Admission {
    Existing(Box<RunRecord>),
    Admitted {
        run: Box<RunRecord>,
        /// Absent when the absolute deadline has elapsed at admission: the row
        /// is already terminal and must not register a scope or start a worker.
        start: Option<Box<Start>>,
    },
    Refused(Refusal),
}

type Checked<T> = std::result::Result<T, Refusal>;

/// Accepts raw operation arguments so earlier checks precede decoding later
/// fields, including on a repeat whose new arguments are entirely invalid.
/// Known-id lookup, concurrency counts and insertion share one immediate
/// transaction. Only its winner returns a start token, and only after commit.
pub fn admit(
    store: &Store,
    host: &dyn Host,
    clock: &Clock,
    platform: Platform,
    request: &Value,
) -> Result<Admission> {
    store.write(|tx| {
        if let Some(run_id) = request.get("run_id").and_then(Value::as_str) {
            match store::lookup(tx, run_id)? {
                Lookup::Found(run) => return Ok(Admission::Existing(run)),
                Lookup::Pruned => {
                    return Ok(Admission::Refused(Refusal::new(
                        "unknown_run",
                        "the run was pruned",
                    )));
                }
                Lookup::Unknown => {}
            }
        }
        if platform == Platform::Windows {
            return Ok(Admission::Refused(Refusal::new(
                "unsupported_platform",
                "codemode is not supported on Windows",
            )));
        }
        let checked = (|| {
            let scope = scope_shape(request)?;
            let agent_id = attest(host, &scope, request)?;
            let fields = request_fields(request)?;
            let limits = limits(request.get("limits"))?;
            let tools = catalog(fields.catalog)?;
            Ok((scope, agent_id, fields, limits, tools))
        })();
        let (scope, agent_id, fields, limits, tools) = match checked {
            Ok(checked) => checked,
            Err(refusal) => return Ok(Admission::Refused(refusal)),
        };
        let (agent_runs, basal_runs) = store::running_counts(tx, &agent_id)?;
        if agent_runs >= MAX_AGENT_RUNS || basal_runs >= MAX_BASAL_RUNS {
            return Ok(Admission::Refused(Refusal::new(
                "busy",
                "codemode concurrency limit reached",
            )));
        }
        let admitted_at = clock.now_ms();
        let wall_deadline_ms = limits.wall_ms.map_or(fields.deadline_ms, |wall| {
            fields.deadline_ms.min(admitted_at.saturating_add(wall))
        });
        let ended = (admitted_at >= fields.deadline_ms).then(|| Terminal {
            status: Status::BudgetExhausted(Budget::Wall),
            value: None,
            error: Some(RunError {
                code: "budget_exhausted:wall".into(),
                message: "the run's deadline has already elapsed".into(),
            }),
            output: String::new(),
            warnings: "[]".into(),
        });
        let catalog_text = fields.catalog.to_string();
        let digest =
            catalog_digest(fields.catalog).map_err(|e| CoreError::Invalid(e.to_string()))?;
        let limits_text =
            serde_json::to_string(&limits).map_err(|e| CoreError::Invalid(e.to_string()))?;
        let scope_text =
            serde_json::to_string(&scope).map_err(|e| CoreError::Invalid(e.to_string()))?;
        let new = NewRun {
            run_id: fields.run_id,
            agent_id: &agent_id,
            program: fields.program,
            catalog: &catalog_text,
            catalog_digest: &digest,
            description: fields.description,
            limits: &limits_text,
            scope: &scope_text,
            deadline_ms: fields.deadline_ms,
            admitted_at,
        };
        if !store::insert_run(tx, &new, ended.as_ref())? {
            return Err(CoreError::Corrupt(
                "admission lost its transaction's run id".into(),
            ));
        }
        let Lookup::Found(run) = store::lookup(tx, fields.run_id)? else {
            return Err(CoreError::Corrupt("admitted run is missing".into()));
        };
        let start = ended.is_none().then(|| {
            Box::new(Start {
                scope: RegisteredScope {
                    selector: scope,
                    targets: tools.values().map(|tool| tool.module.clone()).collect(),
                },
                tools,
                limits,
                wall_deadline_ms,
            })
        });
        Ok(Admission::Admitted { run, start })
    })
}

fn scope_shape(request: &Value) -> Checked<FlowScope> {
    let scope: FlowScope =
        serde_json::from_value(request.get("scope").cloned().unwrap_or(Value::Null))
            .map_err(|e| Refusal::new("no_scope", format!("invalid scope selector: {e}")))?;
    if !matches!(scope.owner, Principal::Reserved { .. }) || scope.scope_ref.trim().is_empty() {
        return Err(Refusal::new("no_scope", "invalid scope selector"));
    }
    Ok(scope)
}

fn attest(host: &dyn Host, scope: &FlowScope, request: &Value) -> Checked<String> {
    let description = host
        .scope_describe(&scope.owner, &scope.scope_ref)
        .map_err(|e| Refusal::new("no_scope", format!("scope cannot be attested: {e}")))?;
    if description.status != ScopeStatus::Live || description.scope_epoch != Some(scope.epoch) {
        return Err(Refusal::new(
            "no_scope",
            "scope is not live at the requested epoch",
        ));
    }
    let stamp = description
        .scope
        .ok_or_else(|| Refusal::new("no_scope", "scope description has no live stamp"))?;
    let agent_id = stamp
        .attributes
        .agent_id
        .ok_or_else(|| Refusal::new("scope_mismatch", "scope has no attested agent identity"))?;
    // Missing or non-string agent ids are invalid_request in request_fields.
    // Compare only string identities here so a shape error is not mislabeled
    // as an identity mismatch. A mismatched string refuses before other fields.
    if request
        .get("agent_id")
        .and_then(Value::as_str)
        .is_some_and(|id| id != agent_id)
    {
        return Err(Refusal::new(
            "scope_mismatch",
            "agent identity does not match the scope",
        ));
    }
    Ok(agent_id)
}

struct Fields<'a> {
    run_id: &'a str,
    program: &'a str,
    catalog: &'a Value,
    deadline_ms: i64,
    description: Option<&'a str>,
}

fn request_fields(request: &Value) -> Checked<Fields<'_>> {
    let invalid = || Refusal::new("invalid_request", "invalid codemode request fields");
    let run_id = request
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    request
        .get("agent_id")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let program = request
        .get("program")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let catalog = request.get("catalog").ok_or_else(invalid)?;
    let deadline_ms = request
        .get("deadline_ms")
        .and_then(Value::as_i64)
        .ok_or_else(invalid)?;
    let description = request
        .get("description")
        .map(|v| v.as_str().ok_or_else(invalid))
        .transpose()?;
    if program.len() > MAX_SCRIPT_BYTES
        || description.is_some_and(|s| {
            s.len() > MAX_DESCRIPTION_BYTES || s.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
        })
    {
        return Err(invalid());
    }
    Ok(Fields {
        run_id,
        program,
        catalog,
        deadline_ms,
        description,
    })
}

fn limits(value: Option<&Value>) -> Checked<Limits> {
    let mut limits = Limits::default();
    let Some(value) = value else {
        return Ok(limits);
    };
    let invalid = || Refusal::new("invalid_limits", "invalid codemode limits");
    let fields = value.as_object().ok_or_else(invalid)?;
    for (key, value) in fields {
        let n = value.as_i64().filter(|n| *n >= 1).ok_or_else(invalid)?;
        match key.as_str() {
            "wall_ms" => limits.wall_ms = Some(n),
            "tool_calls" if n <= 200 => limits.tool_calls = n as u64,
            "output_bytes" if n <= 65_536 => limits.output_bytes = n as u64,
            _ => return Err(invalid()),
        }
    }
    Ok(limits)
}

fn catalog(value: &Value) -> Checked<BTreeMap<String, Tool>> {
    let entries = value
        .as_array()
        .ok_or_else(|| Refusal::new("invalid_catalog", "catalog must be an array"))?;
    let mut tools = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        let name = entry.get("name").and_then(Value::as_str);
        let invalid = |reason: &str| {
            Refusal::new(
                "invalid_catalog",
                format!(
                    "catalog entry {}: {reason}",
                    name.map_or_else(|| index.to_string(), |n| format!("{n:?}"))
                ),
            )
        };
        let fields = entry
            .as_object()
            .ok_or_else(|| invalid("must be an object"))?;
        if fields.len() != 4
            || !["name", "input_schema", "module", "op"]
                .iter()
                .all(|k| fields.contains_key(*k))
        {
            return Err(invalid(
                "expected exactly name, input_schema, module and op",
            ));
        }
        let name = name.ok_or_else(|| invalid("name must be a string"))?;
        if !valid_name(name) {
            return Err(invalid("invalid tool name"));
        }
        if matches!(
            name,
            "__proto__" | "constructor" | "prototype" | "tools" | "console"
        ) {
            return Err(invalid("reserved tool name"));
        }
        if tools.contains_key(name) {
            return Err(invalid("duplicate tool name"));
        }
        let module = fields["module"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= MAX_NAME_BYTES)
            .ok_or_else(|| invalid("invalid module"))?;
        let op = fields["op"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= MAX_NAME_BYTES)
            .ok_or_else(|| invalid("invalid op"))?;
        let input_schema =
            compile_input_schema(&fields["input_schema"]).map_err(|e| invalid(&e.to_string()))?;
        tools.insert(
            name.to_owned(),
            Tool {
                module: module.to_owned(),
                op: op.to_owned(),
                input_schema,
            },
        );
    }
    Ok(tools)
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= MAX_NAME_BYTES
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
