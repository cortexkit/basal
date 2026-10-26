//! Ordered admission checks for one-attempt codemode runs.
//!
//! Only a newly committed, running row grants a start token. Repeated ids,
//! refusals and runs already at their deadline cannot register a scope or
//! start a worker. The caller consumes the token after this transaction ends.

use std::collections::{BTreeMap, BTreeSet};

use basal_host::flow_scope::{FlowScope, RegisteredScope};
use basal_host::{Host, ScopeStatus};
use basal_proto::{MAX_NAME_BYTES, MAX_SCRIPT_BYTES};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subc_protocol::Principal;

use super::catalog::{InputSchema, catalog_digest, compile_input_schema};
use super::store::{self, Budget, Lookup, NewRun, RunError, RunRecord, Status, Terminal};
use crate::clock::Clock;
use crate::error::{CoreError, Result};
use crate::store::Store;

pub const MAX_DESCRIPTION_BYTES: usize = 1_024;
pub const MAX_AGENT_RUNS: u64 = 2;
pub const MAX_BASAL_RUNS: u64 = 16;

/// The admission caller's operating system. Scope and budget checks are the
/// same on every supported platform; worker confinement is checked at launch.
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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<i64>,
    pub tool_calls: u64,
    pub output_bytes: u64,
    #[serde(default = "default_model_cost")]
    pub model_cost_usd: f64,
}

fn default_model_cost() -> f64 {
    crate::classify_cost::DEFAULT_USD
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_ms: Some(basal_host::run_scope::WALL_MS),
            tool_calls: 200,
            output_bytes: 65_536,
            model_cost_usd: default_model_cost(),
        }
    }
}

/// A catalog tool with its compiled input validator. Invalid schemas are
/// dropped before any worker can call the tool.
#[derive(Debug)]
pub struct Tool {
    pub module: String,
    pub op: String,
    pub input_schema: InputSchema,
    pub result_format: Option<cortexkit_role_tool_provider::result::ResultFormat>,
    pub classify_cost: bool,
}

/// A tool core put in the run's catalog that basal dropped because its
/// provider does not serve it in codemode or its catalog entry could not be used.
/// The program still sees the name; calling it rejects with
/// `tool_unavailable`, naming the provider and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub module: String,
    /// A provider refusal code or the reason its catalog entry was dropped.
    pub reason: String,
    pub message: String,
    /// When set, a call rejects with this refusal's own code and message
    /// instead of `tool_unavailable`: the daemon refused to bind the provider
    /// on the run's scope, and the program should see that refusal as sent.
    pub refusal: Option<basal_host::run_scope::ProviderRefusal>,
}

/// Permission to register `codemode:<run_id>` and start its fresh worker.
/// This is deliberately not cloneable: duplicate admits never get a token.
#[derive(Debug)]
pub struct Start {
    pub scope: RegisteredScope,
    /// Core's catalog preset for the run, keyed by the provider modules whose
    /// tool calls carry it: only providers whose preset-less catalog answer
    /// lists that preset. A call to any other provider names none, which
    /// selects that provider's default variant. Empty when core chose no
    /// preset.
    pub presets: BTreeMap<String, String>,
    pub tools: BTreeMap<String, Tool>,
    /// Dropped tools by name, never also in `tools`.
    pub unavailable: BTreeMap<String, Unavailable>,
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
    _platform: Platform,
    request: &Value,
) -> Result<Admission> {
    if let Some(run_id) = request.get("run_id").and_then(Value::as_str)
        && let Some(known) = store.read(|conn| known(conn, run_id))?
    {
        return Ok(known);
    }
    let checked = (|| {
        let scope = scope_shape(request)?;
        let agent_id = attest(host, &scope, request, None)?;
        // This path admits agent runs only, so it needs the agent's id too.
        request
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Refusal::new("invalid_request", "invalid codemode request fields"))?;
        let fields = request_fields(request)?;
        let limits = limits(request.get("limits"))?;
        let resolved = super::models::add_catalog(fields.catalog)
            .map_err(|e| Refusal::new("invalid_catalog", e))?;
        let catalog = catalog(&resolved, &[])?;
        Ok((scope, agent_id, fields, limits, catalog))
    })();
    let (scope, agent_id, fields, limits, catalog) = match checked {
        Ok(checked) => checked,
        Err(refusal) => return Ok(Admission::Refused(refusal)),
    };
    let catalog_text = catalog.callable.to_string();
    let digest =
        catalog_digest(&catalog.callable).map_err(|e| CoreError::Invalid(e.to_string()))?;
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
        let (agent_runs, basal_runs) =
            store::running_counts(tx, store::RunOwner::Agent(&agent_id))?;
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
            warnings: serde_json::to_string(&catalog.warnings).unwrap(),
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
        if ended.is_none() {
            tx.execute(
                "UPDATE codemode_runs SET warnings=?2 WHERE run_id=?1",
                rusqlite::params![
                    fields.run_id,
                    serde_json::to_string(&catalog.warnings).unwrap()
                ],
            )?;
        }
        let Lookup::Found(run) = store::lookup(tx, fields.run_id)? else {
            return Err(CoreError::Corrupt("admitted run is missing".into()));
        };
        let start = ended.is_none().then(|| {
            Box::new(Start {
                scope: RegisteredScope {
                    selector: scope,
                    targets: catalog
                        .tools
                        .values()
                        .map(|tool| {
                            if super::models::is_local(&tool.module, &tool.op) {
                                "broca".into()
                            } else {
                                tool.module.clone()
                            }
                        })
                        .collect(),
                },
                presets: BTreeMap::new(),
                tools: catalog.tools,
                unavailable: catalog.unavailable,
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

fn attest(
    host: &dyn Host,
    scope: &FlowScope,
    request: &Value,
    deadline_bounds: Option<(i64, i64)>,
) -> Checked<String> {
    let stamp = described_stamp(host, scope, deadline_bounds)?;
    let agent_id = stamp
        .attributes
        .agent_id
        .ok_or_else(|| Refusal::new("scope_mismatch", "scope has no attested agent identity"))?;
    // A record carries at most one of the two; a stamp naming both is not an
    // agent run's, whatever else it says.
    if stamp.attributes.on_behalf_of_agent.is_some() {
        return Err(Refusal::new(
            "scope_mismatch",
            "an agent run's scope also names an agent it works on behalf of",
        ));
    }
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
    attest_run_id(stamp.attributes.run_id, request.get("run_id"))?;
    Ok(agent_id)
}

/// Attests a worker run's scope and returns the head it works for, which basal
/// records for display and audit only and never acts as. The scope must name
/// that head in `on_behalf_of_agent` and carry no `agent_id`, `delegates` or
/// `flow_id`; its parent must be the worker's own scope at the epoch the
/// worker called from; and its `run_id` must be this run's.
fn attest_worker(
    host: &dyn Host,
    scope: &FlowScope,
    run_id: &str,
    invoking: &basal_host::run_scope::Scope,
    deadline_bounds: Option<(i64, i64)>,
) -> Checked<String> {
    let stamp = described_stamp(host, scope, deadline_bounds)?;
    let attributes = &stamp.attributes;
    if attributes.agent_id.is_some() || attributes.delegates || attributes.flow_id.is_some() {
        return Err(Refusal::new(
            "scope_mismatch",
            "a worker run's scope carries an agent's identity",
        ));
    }
    let head = attributes
        .on_behalf_of_agent
        .clone()
        .filter(|head| !head.is_empty())
        .ok_or_else(|| {
            Refusal::new(
                "scope_mismatch",
                "a worker run's scope names no agent it works on behalf of",
            )
        })?;
    let core = Principal::Reserved {
        module_id: basal_host::subc_catalog::CORE.into(),
    };
    if !stamp.parent.as_ref().is_some_and(|parent| {
        parent.owner == core
            && parent.scope_ref == invoking.reference
            && parent.scope_epoch == invoking.epoch
    }) {
        return Err(Refusal::new(
            "scope_mismatch",
            "a worker run's scope is not a child of the scope that asked for it",
        ));
    }
    attest_run_id(attributes.run_id.clone(), Some(&Value::from(run_id)))?;
    Ok(head)
}

/// The live stamp the daemon describes for `scope`, refused unless it is the
/// requested owner, ref and epoch and its owner may set authority-bearing
/// attributes.
fn described_stamp(
    host: &dyn Host,
    scope: &FlowScope,
    deadline_bounds: Option<(i64, i64)>,
) -> Checked<subc_protocol::scope::ScopeStamp> {
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
    if let Some((requested, core_cap)) = deadline_bounds {
        log_stamp_deadline(stamp.expires_at_ms, requested, core_cap);
    }
    Ok(stamp)
}

/// The stamp must carry a `run_id`, equal to the requested one when the
/// request names it as a string.
fn attest_run_id(stamped: Option<String>, requested: Option<&Value>) -> Checked<()> {
    let run_id = stamped
        .ok_or_else(|| Refusal::new("scope_mismatch", "scope has no attested run identity"))?;
    if requested
        .and_then(Value::as_str)
        .is_some_and(|id| id != run_id)
    {
        return Err(Refusal::new(
            "scope_mismatch",
            "run identity does not match the scope",
        ));
    }
    Ok(())
}

fn log_stamp_deadline(expiry: Option<u64>, requested: i64, core_cap: i64) {
    match expiry {
        Some(expiry)
            if i128::from(expiry) < i128::from(requested)
                || i128::from(expiry) > i128::from(core_cap) =>
        {
            tracing::warn!(
                target: "codemode",
                stamp_expires_at_ms = expiry,
                requested_expires_at_ms = requested,
                core_expires_at_ms = core_cap,
                "run scope stamp deadline mismatch"
            );
        }
        None => {
            // SUBC daemons predating call modes omit ScopeStamp::expires_at_ms.
            // Basal still enforces the run's wall-time and person-wait limits.
            tracing::debug!(target: "codemode", "run scope stamp deadline absent");
        }
        Some(_) => {}
    }
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
        if key == "model_cost_usd" {
            limits.model_cost_usd = value
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0 && *n <= default_model_cost())
                .ok_or_else(invalid)?;
            continue;
        }
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

#[derive(Debug, Default)]
struct Catalog {
    tools: BTreeMap<String, Tool>,
    unavailable: BTreeMap<String, Unavailable>,
    warnings: Vec<Value>,
    callable: Value,
    warned_duplicates: BTreeSet<String>,
}

impl Catalog {
    fn drop_entry(&mut self, name: Option<&str>, module: Option<&str>, index: usize, reason: &str) {
        if reason == "duplicate tool name"
            && !self.warned_duplicates.insert(name.unwrap().to_owned())
        {
            return;
        }
        let tool = name.map_or_else(|| Value::from(index), Value::from);
        let label = name.map_or_else(|| index.to_string(), |name| format!("{name:?}"));
        let message = format!("Catalog entry {label} was dropped: {reason}.");
        let mut warning = serde_json::json!({
            "code":"tool_entry_invalid", "tool":tool, "reason":reason, "message":message
        });
        if let Some(module) = module {
            warning["module"] = Value::from(module);
        }
        self.warnings.push(warning);
        // Only safe identifiers reach the worker's namespace, including names
        // whose calls reject locally rather than routing to a provider.
        if let Some(name) = name.filter(|name| check_name(name).is_ok()) {
            self.unavailable.insert(
                name.to_owned(),
                Unavailable {
                    module: module.unwrap_or_default().to_owned(),
                    reason: reason.to_owned(),
                    message,
                    refusal: None,
                },
            );
        }
    }
}

fn catalog(value: &Value, gaps: &[basal_host::run_scope::CatalogGap]) -> Checked<Catalog> {
    let entries = value
        .as_array()
        .ok_or_else(|| Refusal::new("invalid_catalog", "catalog must be an array"))?;
    // Count claims before validating entries: even a malformed claim makes
    // routing that name ambiguous, including claims from unavailable providers.
    let mut claims = BTreeMap::<&str, usize>::new();
    for name in entries
        .iter()
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .chain(
            gaps.iter()
                .flat_map(|gap| gap.tools.iter().map(String::as_str)),
        )
    {
        *claims.entry(name).or_default() += 1;
    }
    let mut catalog = Catalog::default();
    let mut callable = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let name = entry.get("name").and_then(Value::as_str);
        let module = entry.get("module").and_then(Value::as_str);
        let checked = (|| -> std::result::Result<Tool, String> {
            if name.is_some_and(|name| claims[name] > 1) {
                return Err("duplicate tool name".into());
            }
            let fields = entry.as_object().ok_or("must be an object")?;
            if fields.len()
                != 4 + usize::from(fields.contains_key("result_format"))
                    + usize::from(fields.contains_key("capabilities"))
                || !["name", "input_schema", "module", "op"]
                    .iter()
                    .all(|k| fields.contains_key(*k))
            {
                return Err(
                    "expected name, input_schema, module, op and optional result_format".into(),
                );
            }
            let name = name.ok_or("name must be a string")?;
            check_name(name)?;
            let module = fields["module"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= MAX_NAME_BYTES)
                .ok_or("invalid module")?;
            let op = fields["op"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= MAX_NAME_BYTES)
                .ok_or("invalid op")?;
            let input_schema =
                compile_input_schema(&fields["input_schema"]).map_err(|e| e.to_string())?;
            let result_format = fields
                .get("result_format")
                .map(|format| {
                    format
                        .as_str()
                        .map(|format| {
                            cortexkit_role_tool_provider::result::ResultFormat::from(
                                format.to_owned(),
                            )
                        })
                        .ok_or("result_format must be a string")
                })
                .transpose()?;
            Ok(Tool {
                module: module.to_owned(),
                op: op.to_owned(),
                input_schema,
                result_format,
                classify_cost: crate::classify_cost::matches(entry),
            })
        })();
        match checked {
            Ok(tool) => {
                catalog.tools.insert(name.unwrap().to_owned(), tool);
                callable.push(entry.clone());
            }
            Err(reason) => catalog.drop_entry(name, module, index, &reason),
        }
    }
    unavailable_tools(gaps, &claims, &mut catalog);
    catalog.callable = Value::Array(callable);
    Ok(catalog)
}

/// The names a program may use for a tool: a JavaScript identifier that is
/// not one of the names the codemode prelude reserves.
fn check_name(name: &str) -> std::result::Result<(), &'static str> {
    if !valid_name(name) {
        return Err("invalid tool name");
    }
    if matches!(
        name,
        "__proto__" | "constructor" | "prototype" | "tools" | "console"
    ) {
        return Err("reserved tool name");
    }
    Ok(())
}

/// Unavailable providers' names must be safe and unambiguous too; a bad name
/// must not prevent unrelated tools from running.
fn unavailable_tools(
    gaps: &[basal_host::run_scope::CatalogGap],
    claims: &BTreeMap<&str, usize>,
    catalog: &mut Catalog,
) {
    for gap in gaps {
        for name in &gap.tools {
            let reason = if claims[name.as_str()] > 1 {
                Some("duplicate tool name")
            } else {
                check_name(name).err()
            };
            if let Some(reason) = reason {
                catalog.drop_entry(Some(name), Some(&gap.module), 0, reason);
                continue;
            }
            catalog.unavailable.insert(
                name.clone(),
                Unavailable {
                    module: gap.module.clone(),
                    reason: gap.reason.code().into(),
                    message: gap.message(),
                    refusal: gap.refusal.clone(),
                },
            );
        }
    }
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

/// basal-module builds this invocation from the principal and scope the daemon
/// records when binding the caller's codemode tool route. The caller's identity
/// and the invoking scope come only from that record; bind identity configures
/// provider workspaces and sessions, not authority. Program arguments supply
/// only code, a description and lower limits.
pub struct ToolInvocation<'a> {
    pub run_id: &'a str,
    pub caller: RunCaller<'a>,
    pub invoking_scope: basal_host::run_scope::Scope,
    pub carrier: &'a str,
    pub call_key: Option<&'a str>,
    pub record_scope: &'a str,
    pub custodian: &'a str,
    pub bind_identity: &'a subc_protocol::BindIdentity,
}

/// Who asked for a codemode run, from the daemon's stamp on the invoking route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCaller<'a> {
    /// A head, by the `agent_id` its session scope carries. The run is an
    /// agent run and counts against that agent's cap.
    Agent(&'a str),
    /// A worker session, such as a mason or a sidekick session working for one,
    /// identified by its own scope (`ToolInvocation::invoking_scope`). Core
    /// decides whether it may open a run and which head the run works for. The
    /// run is a worker run: it counts against the worker's own cap, never the
    /// head's, and basal never acts as that head.
    Worker,
}

pub fn admit_tool(
    store: &Store,
    host: &dyn Host,
    transport: &dyn basal_host::transport::Transport,
    clock: &Clock,
    _platform: Platform,
    invocation: &ToolInvocation<'_>,
    arguments: &Value,
) -> Result<Admission> {
    use basal_host::run_scope;
    use serde_json::json;
    if let Some(known) = store.read(|conn| known(conn, invocation.run_id))? {
        return Ok(known);
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
    let agent_id = match invocation.caller {
        RunCaller::Agent(agent_id) => Some(agent_id),
        RunCaller::Worker => None,
    };
    let mut request = json!({"run_id":invocation.run_id,
        "program":arguments["code"],"catalog":[],"deadline_ms":expires_at_ms});
    if let Some(agent_id) = agent_id {
        request["agent_id"] = agent_id.into();
    }
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
        agent_id: agent_id.map(str::to_owned),
        invoking_scope: invocation.invoking_scope.clone(),
        run_id: invocation.run_id.into(),
        expires_at_ms,
    };
    // A worker run's cap is the worker's own, keyed by its scope ref; it has
    // no agent of its own, so its row's agent_id is left empty.
    let (owner, worker_scope) = match agent_id {
        Some(agent_id) => (store::RunOwner::Agent(agent_id), None),
        None => {
            let reference = invocation.invoking_scope.reference.as_str();
            (store::RunOwner::Worker(reference), Some(reference))
        }
    };
    let limits_text = serde_json::to_string(&limits).unwrap();
    let empty_digest = catalog_digest(&json!([])).unwrap();
    let won = store.write(|tx| {
        if let Some(known) = known(tx, invocation.run_id)? { return Ok(Err(known)); }
        let (own,total) = store::running_counts(tx,owner)?;
        if own >= MAX_AGENT_RUNS || total >= MAX_BASAL_RUNS {
            return Ok(Err(Admission::Refused(Refusal::new("busy","codemode concurrency limit reached"))));
        }
        store::insert_run(tx,&NewRun {
            run_id:invocation.run_id,agent_id:agent_id.unwrap_or_default(),program:fields.program,
            catalog:"[]",catalog_digest:&empty_digest,description:fields.description,
            limits:&limits_text,scope:"{}",deadline_ms:expires_at_ms,admitted_at:started,
        },None)?;
        tx.execute("UPDATE codemode_runs SET keyless=?2,program_digest=?3,bind_identity=?4,worker_scope=?5 WHERE run_id=?1",rusqlite::params![invocation.run_id,invocation.call_key.is_none(),blake3::hash(fields.program.as_bytes()).to_hex().to_string(),serde_json::to_string(invocation.bind_identity).unwrap(),worker_scope])?;
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
        let bounds = Some((expires_at_ms, opened.expires_at_ms));
        // For a worker run, the head it works for: recorded for display and
        // audit, never used as the run's identity.
        let on_behalf_of_agent = match agent_id {
            Some(_) => attest(host, &selector, &request, bounds).map(|_| None),
            None => attest_worker(
                host,
                &selector,
                invocation.run_id,
                &invocation.invoking_scope,
                bounds,
            )
            .map(Some),
        }
        .map_err(|e| json!({"code":e.code,"message":e.message}))?;
        if let Some(head) = &on_behalf_of_agent {
            store
                .write(|tx| {
                    tx.execute(
                        "UPDATE codemode_runs SET on_behalf_of_agent=?2 WHERE run_id=?1 AND status='running'",
                        rusqlite::params![invocation.run_id, head],
                    )?;
                    Ok(())
                })
                .map_err(|e| json!({"code":"storage_unavailable","message":e.to_string()}))?;
        }
        let daemon = transport.catalog().map_err(super::scope::error_value)?;
        // Reading each opted-in provider's own tool catalog makes live calls,
        // so it runs here, outside every store write. The reads go over the
        // run's own scope, which core registered before answering the open:
        // the daemon stamps a run scope's binds with the codemode call mode,
        // and providers such as cerebellum admit basal only there. They bind
        // as a headless runner in the caller's project root, which providers
        // such as AFT accept; the program's own tool calls later use the
        // caller's forwarded identity on the same scope.
        let read_identity = run_scope::catalog_read_identity(
            &invocation.bind_identity.project_root,
            invocation.run_id,
        );
        let run_scope_selector = selector.selector();
        let read = run_scope::CatalogRead {
            scope: &run_scope_selector,
            identity: &read_identity,
        };
        let resolved = run_scope::catalog(
            transport,
            &read,
            &daemon,
            &opened.catalog,
            opened.preset.as_deref(),
        )
        .map_err(super::scope::error_value)?;
        let gaps = resolved.gaps;
        let presets: BTreeMap<String, String> = match &opened.preset {
            Some(preset) => resolved
                .preset_modules
                .into_iter()
                .map(|module| (module, preset.clone()))
                .collect(),
            None => BTreeMap::new(),
        };
        let resolved = super::models::add_catalog(&resolved.catalog)
            .map_err(|e| json!({"code":"invalid_catalog","message":e}))?;
        let catalog =
            catalog(&resolved, &gaps).map_err(|e| json!({"code":e.code,"message":e.message}))?;
        let mut warnings: Vec<Value> = gaps
            .iter()
            .map(|gap| {
                json!({"code":"tools_unavailable","module":gap.module,"reason":gap.reason.code(),
                    "tools":gap.tools,"message":gap.message()})
            })
            .collect();
        warnings.extend(catalog.warnings);
        // The digest covers only the tools the program can call; dropped
        // tools are reported in the warnings instead.
        let digest = catalog_digest(&catalog.callable).unwrap();
        store.write(|tx| {
            tx.execute("UPDATE codemode_runs SET catalog=?2,catalog_digest=?3,scope=?4,warnings=?5 WHERE run_id=?1 AND status='running'",
                rusqlite::params![invocation.run_id,catalog.callable.to_string(),digest,serde_json::to_string(&selector).unwrap(),serde_json::to_string(&warnings).unwrap()])?;
            Ok(())
        }).map_err(|e| json!({"code":"storage_unavailable","message":e.to_string()}))?;
        Ok(Start {
            scope: RegisteredScope {
                selector,
                targets: catalog
                    .tools
                    .values()
                    .map(|t| {
                        if super::models::is_local(&t.module, &t.op) {
                            "broca".into()
                        } else {
                            t.module.clone()
                        }
                    })
                    .collect(),
            },
            presets,
            tools: catalog.tools,
            unavailable: catalog.unavailable,
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

#[cfg(test)]
#[path = "deadline_tests.rs"]
mod deadline_tests;

#[cfg(test)]
#[path = "worker_run_tests.rs"]
mod worker_run_tests;
