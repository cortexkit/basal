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
use rusqlite::Connection;
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
            wall_ms: Some(basal_host::run_scope::WALL_MS),
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
/// Known ids return through a read-only lookup. Scope attestation and schema
/// compilation run without a write lock so they cannot stall flow writes.
/// An immediate transaction rechecks the id, counts concurrency and inserts;
/// only its winner returns a start token, and only after commit.
pub fn admit(
    store: &Store,
    host: &dyn Host,
    clock: &Clock,
    platform: Platform,
    request: &Value,
) -> Result<Admission> {
    if let Some(run_id) = request.get("run_id").and_then(Value::as_str)
        && let Some(known) = store.read(|conn| known(conn, run_id))?
    {
        return Ok(known);
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
    let catalog_text = fields.catalog.to_string();
    let digest = catalog_digest(fields.catalog).map_err(|e| CoreError::Invalid(e.to_string()))?;
    let limits_text =
        serde_json::to_string(&limits).map_err(|e| CoreError::Invalid(e.to_string()))?;
    let scope_text =
        serde_json::to_string(&scope).map_err(|e| CoreError::Invalid(e.to_string()))?;
    store.write(|tx| {
        // Another caller may have admitted or pruned this id while the daemon
        // was describing its scope. That caller's stored payload takes priority.
        if let Some(known) = known(tx, fields.run_id)? {
            return Ok(known);
        }
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

fn known(conn: &Connection, run_id: &str) -> Result<Option<Admission>> {
    Ok(match store::lookup(conn, run_id)? {
        Lookup::Found(run) => Some(Admission::Existing(run)),
        Lookup::Pruned => Some(Admission::Refused(Refusal::new(
            "unknown_run",
            "the run was pruned",
        ))),
        Lookup::Unknown => None,
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
    if stamp.owner != scope.owner
        || stamp.scope_ref != scope.scope_ref
        || stamp.scope_epoch != scope.epoch
        || !stamp.owner_authorized
    {
        return Err(Refusal::new(
            "scope_mismatch",
            "the daemon stamp does not attest the requested scope",
        ));
    }
    let agent_id = stamp
        .attributes
        .agent_id
        .ok_or_else(|| Refusal::new("scope_mismatch", "scope has no attested agent identity"))?;
    // Missing or non-string agent_id and run_id request fields are rejected as
    // invalid_request by request_fields. Compare only string identities here
    // so invalid field types are not mislabeled as scope_mismatch. Mismatched
    // strings refuse before request-field, limit and catalog validation.
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
    let run_id = stamp
        .attributes
        .run_id
        .ok_or_else(|| Refusal::new("scope_mismatch", "scope has no attested run identity"))?;
    if request
        .get("run_id")
        .and_then(Value::as_str)
        .is_some_and(|id| id != run_id)
    {
        return Err(Refusal::new(
            "scope_mismatch",
            "run identity does not match the scope",
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
            "wall_ms" if n <= basal_host::run_scope::WALL_MS => limits.wall_ms = Some(n),
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

/// All authority-bearing fields are copied by the module from its bound route.
/// Program arguments have no identity, scope, expiry or catalog fields.
pub struct ToolInvocation<'a> {
    pub run_id: &'a str,
    pub agent_id: &'a str,
    pub invoking_scope: basal_host::run_scope::Scope,
    pub carrier: &'a str,
    pub call_key: Option<&'a str>,
    pub record_scope: &'a str,
    pub custodian: &'a str,
    pub bind_identity: &'a subc_protocol::BindIdentity,
}

pub fn admit_tool(
    store: &Store,
    host: &dyn Host,
    transport: &dyn basal_host::transport::Transport,
    clock: &Clock,
    platform: Platform,
    invocation: &ToolInvocation<'_>,
    arguments: &Value,
) -> Result<Admission> {
    use basal_host::run_scope;
    use serde_json::json;
    if let Some(known) = store.read(|conn| known(conn, invocation.run_id))? {
        return Ok(known);
    }
    if platform == Platform::Windows {
        return Ok(Admission::Refused(Refusal::new(
            "unsupported_platform",
            "codemode is not supported on Windows",
        )));
    }
    let Some(fields_in) = arguments.as_object() else {
        return Ok(Admission::Refused(Refusal::new(
            "invalid_request",
            "codemode arguments must be an object",
        )));
    };
    if fields_in
        .keys()
        .any(|key| !matches!(key.as_str(), "code" | "description" | "limits"))
    {
        return Ok(Admission::Refused(Refusal::new(
            "invalid_request",
            "unknown codemode argument",
        )));
    }
    let started = clock.now_ms();
    let expires_at_ms = started.saturating_add(run_scope::SCOPE_LIFETIME_MS);
    let mut request = json!({"run_id":invocation.run_id,"agent_id":invocation.agent_id,
        "program":arguments["code"],"catalog":[],"deadline_ms":expires_at_ms});
    if let Some(description) = arguments.get("description") {
        request["description"] = description.clone();
    }
    let fields = match request_fields(&request) {
        Ok(fields) => fields,
        Err(refusal) => return Ok(Admission::Refused(refusal)),
    };
    let limits = match limits(arguments.get("limits")) {
        Ok(limits) => limits,
        Err(refusal) => return Ok(Admission::Refused(refusal)),
    };
    let open = run_scope::Open {
        agent_id: invocation.agent_id.into(),
        invoking_scope: invocation.invoking_scope.clone(),
        run_id: invocation.run_id.into(),
        expires_at_ms,
    };
    let limits_text = serde_json::to_string(&limits).unwrap();
    let empty_digest = catalog_digest(&json!([])).unwrap();
    let won = store.write(|tx| {
        if let Some(known) = known(tx, invocation.run_id)? { return Ok(Err(known)); }
        let (agent,total) = store::running_counts(tx,invocation.agent_id)?;
        if agent >= MAX_AGENT_RUNS || total >= MAX_BASAL_RUNS {
            return Ok(Err(Admission::Refused(Refusal::new("busy","codemode concurrency limit reached"))));
        }
        store::insert_run(tx,&NewRun {
            run_id:invocation.run_id,agent_id:invocation.agent_id,program:fields.program,
            catalog:"[]",catalog_digest:&empty_digest,description:fields.description,
            limits:&limits_text,scope:"{}",deadline_ms:expires_at_ms,admitted_at:started,
        },None)?;
        tx.execute("UPDATE codemode_runs SET keyless=?2,program_digest=?3,bind_identity=?4 WHERE run_id=?1",rusqlite::params![invocation.run_id,invocation.call_key.is_none(),blake3::hash(fields.program.as_bytes()).to_hex().to_string(),serde_json::to_string(invocation.bind_identity).unwrap()])?;
        tx.execute("INSERT INTO codemode_scopes(run_id,request) VALUES (?1,?2)",rusqlite::params![invocation.run_id,serde_json::to_string(&open).unwrap()])?;
        if let Some(key) = invocation.call_key {
            tx.execute("INSERT INTO codemode_tool_calls(carrier,call_key,run_id,scope,custodian) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![invocation.carrier,key,invocation.run_id,invocation.record_scope,invocation.custodian])?;
        }
        Ok(Ok(()))
    })?;
    if let Err(known) = won {
        return Ok(known);
    }
    let prepared = (|| -> std::result::Result<Start, Value> {
        let opened = match run_scope::open(transport, &open) {
            Ok(opened) => opened,
            Err(error) => {
                if matches!(
                    error,
                    basal_host::transport::WireError::Refused { .. }
                        | basal_host::transport::WireError::RefusedDetails { .. }
                ) {
                    store
                        .write(|tx| {
                            tx.execute(
                                "UPDATE codemode_scopes SET refused=1 WHERE run_id=?1",
                                [invocation.run_id],
                            )?;
                            Ok(())
                        })
                        .map_err(
                            |e| json!({"code":"storage_unavailable","message":e.to_string()}),
                        )?;
                }
                return Err(super::scope::error_value(error));
            }
        };
        store
            .write(|tx| {
                tx.execute(
                    "UPDATE codemode_scopes SET opened=?2 WHERE run_id=?1",
                    rusqlite::params![invocation.run_id, serde_json::to_string(&opened).unwrap()],
                )?;
                Ok(())
            })
            .map_err(|e| json!({"code":"storage_unavailable","message":e.to_string()}))?;
        let selector = FlowScope {
            owner: Principal::Reserved {
                module_id: basal_host::subc_catalog::CORE.into(),
            },
            scope_ref: opened.scope.reference,
            epoch: opened.scope.epoch,
        };
        if opened.expires_at_ms != expires_at_ms {
            return Err(
                json!({"code":"scope_mismatch","message":"core changed the immutable run expiry"}),
            );
        }
        attest(host, &selector, &request)
            .map_err(|e| json!({"code":e.code,"message":e.message}))?;
        let daemon = transport.catalog().map_err(super::scope::error_value)?;
        let resolved =
            run_scope::catalog(&daemon, &opened.catalog).map_err(super::scope::error_value)?;
        let tools = catalog(&resolved).map_err(|e| json!({"code":e.code,"message":e.message}))?;
        let digest = catalog_digest(&resolved).unwrap();
        store.write(|tx| {
            tx.execute("UPDATE codemode_runs SET catalog=?2,catalog_digest=?3,scope=?4 WHERE run_id=?1 AND status='running'",
                rusqlite::params![invocation.run_id,resolved.to_string(),digest,serde_json::to_string(&selector).unwrap()])?;
            Ok(())
        }).map_err(|e| json!({"code":"storage_unavailable","message":e.to_string()}))?;
        Ok(Start {
            scope: RegisteredScope {
                selector,
                targets: tools.values().map(|t| t.module.clone()).collect(),
            },
            tools,
            limits,
            wall_deadline_ms: started.saturating_add(limits.wall_ms.unwrap_or(run_scope::WALL_MS)),
        })
    })();
    let start = match prepared {
        Ok(start) => Some(Box::new(start)),
        Err(error) => {
            let mut warnings = Vec::new();
            if let Err(e) = super::scope::close(store, transport, invocation.run_id) {
                warnings.push(json!({"code":"scope_close_failed","message":e.to_string()}));
            }
            if let Some(detail) = error.get("detail") {
                warnings.push(json!({"code":"core_refusal_detail","detail":detail}));
            }
            store.write(|tx| {
                tx.execute(
                    "UPDATE codemode_scopes SET refusal=?2 WHERE run_id=?1 AND EXISTS (SELECT 1 FROM codemode_runs WHERE run_id=?1 AND status='running')",
                    rusqlite::params![invocation.run_id, error.to_string()],
                )?;
                store::commit_terminal(
                    tx,
                    invocation.run_id,
                    &Terminal {
                        status: Status::Failed,
                        value: None,
                        error: Some(RunError {
                            code: error["code"]
                                .as_str()
                                .unwrap_or("codemode_unavailable")
                                .into(),
                            message: error["message"]
                                .as_str()
                                .unwrap_or("run scope could not open")
                                .into(),
                        }),
                        output: String::new(),
                        warnings: serde_json::to_string(&warnings).unwrap(),
                    },
                    clock.now_ms(),
                )?;
                Ok(())
            })?;
            None
        }
    };
    let Lookup::Found(run) = store.read(|conn| store::lookup(conn, invocation.run_id))? else {
        return Err(CoreError::Corrupt("admitted tool run disappeared".into()));
    };
    let start = if run.status == Status::Running {
        start
    } else {
        None
    };
    Ok(Admission::Admitted { run, start })
}
