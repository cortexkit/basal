//! prefrontal-core authorizes run scopes. The invoking tool route's daemon stamp
//! records the verified principal, scope owner, reference, epoch and agent_id.
//! Basal sends that agent and scope identity with a fixed expiry to core over its
//! own route stamped reserved:basal, the daemon-verified basal module principal.
//! The confined worker never makes these authority-bearing control requests.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use cortexkit_role_tool_provider::catalog::{CatalogAnswer, check_modes, serves_call_mode};
use cortexkit_role_tool_provider::{CALL_MODES_CAPABILITY, CallMode};

use crate::subc_catalog::{CORE, read_tool_catalog};
use crate::transport::{Transport, WireError};

pub const WALL_MS: i64 = 30 * 60 * 1_000;
pub const PERSON_WAIT_MS: i64 = 10 * 60 * 1_000;
pub const SCOPE_LIFETIME_MS: i64 = WALL_MS + PERSON_WAIT_MS + 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    #[serde(rename = "ref")]
    pub reference: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Open {
    pub agent_id: String,
    pub invoking_scope: Scope,
    pub run_id: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub tool: String,
    pub module: String,
    pub op: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opened {
    pub scope: Scope,
    pub expires_at_ms: i64,
    pub catalog: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Close {
    pub run_id: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closed {
    pub closed: bool,
    pub already_closed: bool,
}

pub fn open(transport: &dyn Transport, request: &Open) -> Result<Opened, WireError> {
    call(transport, "codemode.run_scope.open", request)
}

pub fn close(transport: &dyn Transport, request: &Close) -> Result<Closed, WireError> {
    call(transport, "codemode.run_scope.close", request)
}

fn call<T: Serialize, R: for<'de> Deserialize<'de>>(
    transport: &dyn Transport,
    op: &str,
    request: &T,
) -> Result<R, WireError> {
    let params = serde_json::to_value(request).map_err(|e| WireError::NeverSent(e.to_string()))?;
    let value: Value = transport.management(CORE, op, params)?;
    serde_json::from_value(value).map_err(|e| WireError::Unreadable(e.to_string()))
}

/// The legacy module capability: every tool of a provider declaring it may be
/// called from a codemode run, without reading its tool catalog. It stays
/// honoured until the providers still declaring it have moved to
/// `call-modes/v1`.
pub const LEGACY_RUN_SCOPES_CAPABILITY: &str = "agent-run-scopes/v1";

/// Why basal refused to put one catalog entry in a codemode run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// The provider declares neither `agent-run-scopes/v1` nor `call-modes/v1`.
    NotOptedIn,
    /// The provider's own `tool.catalog` does not list `codemode` in the
    /// tool's `modes` (absent `modes` means classic only), or does not list
    /// the tool at all.
    ModeNotServed,
    /// The provider's `role.describe` or `tool.catalog` could not be read.
    CatalogUnreadable,
    /// The provider's `tool.catalog` answer has a `modes` list that fails
    /// `check_modes` (empty, duplicate or unknown mode), or lists the tool
    /// twice.
    InvalidAnswer,
}

impl RefusalReason {
    /// The error code a refused run reports.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotOptedIn => "provider_not_opted_in",
            Self::ModeNotServed => "call_mode_unsupported",
            Self::CatalogUnreadable => "catalog_unreadable",
            Self::InvalidAnswer => "invalid_answer",
        }
    }
}

/// One catalog entry basal will not let a codemode run call, naming the
/// provider module, the run's tool name and the provider's op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRefusal {
    pub reason: RefusalReason,
    pub module: String,
    pub tool: String,
    pub op: String,
    /// Plain-language detail, such as the read error or the failing check.
    pub problem: String,
}

impl ToolRefusal {
    pub fn message(&self) -> String {
        format!(
            "codemode can't use tool {} ({}.{}): {}",
            self.tool, self.module, self.op, self.problem
        )
    }
}

/// Why the run's tool catalog could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// The daemon catalog or core's entries could not be used.
    Wire(WireError),
    /// A provider does not serve one of the entries in codemode.
    Tool(ToolRefusal),
}

impl From<WireError> for CatalogError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

/// Whether the provider serves `entry` in codemode, as it declares it. A
/// provider declaring the legacy capability serves every tool. A provider
/// declaring `call-modes/v1` serves the tools whose `modes`, in its own
/// `tool.catalog` answer, include `codemode`. `answers` holds each module's
/// answer once read, so a provider listed for several entries is read once.
fn codemode_served(
    transport: &dyn Transport,
    module: &Value,
    entry: &CatalogEntry,
    answers: &mut BTreeMap<String, Result<CatalogAnswer, WireError>>,
) -> Result<Option<cortexkit_role_tool_provider::result::ResultFormat>, ToolRefusal> {
    let refuse = |reason, problem: String| ToolRefusal {
        reason,
        module: entry.module.clone(),
        tool: entry.tool.clone(),
        op: entry.op.clone(),
        problem,
    };
    let provides = |token: &str| {
        module["capabilities"]["provides"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|cap| cap == token))
    };
    let legacy = provides(LEGACY_RUN_SCOPES_CAPABILITY);
    if !legacy && !provides(CALL_MODES_CAPABILITY) {
        return Err(refuse(
            RefusalReason::NotOptedIn,
            format!(
                "provider {} declares neither {CALL_MODES_CAPABILITY} nor {LEGACY_RUN_SCOPES_CAPABILITY}",
                entry.module
            ),
        ));
    }
    let answer = answers
        .entry(entry.module.clone())
        .or_insert_with(|| read_tool_catalog(transport, &entry.module));
    let answer = answer.as_ref().map_err(|error| {
        refuse(
            RefusalReason::CatalogUnreadable,
            format!("its tool.catalog could not be read: {error:?}; retrying the run may succeed"),
        )
    })?;
    // An invalid modes list on any tool makes the whole answer invalid, not
    // only that tool's entry.
    for tool in &answer.tools {
        check_modes(tool).map_err(|problem| {
            refuse(
                RefusalReason::InvalidAnswer,
                format!("its tool.catalog answer is invalid: {problem}"),
            )
        })?;
    }
    let mut listed = answer.tools.iter().filter(|tool| tool.name == entry.op);
    let Some(tool) = listed.next() else {
        return Err(refuse(
            RefusalReason::ModeNotServed,
            "its tool.catalog does not list the tool".into(),
        ));
    };
    if listed.next().is_some() {
        return Err(refuse(
            RefusalReason::InvalidAnswer,
            "its tool.catalog lists the tool more than once".into(),
        ));
    }
    if !legacy && !serves_call_mode(tool, &CallMode::Codemode) {
        return Err(refuse(
            RefusalReason::ModeNotServed,
            "the tool's modes do not include codemode".into(),
        ));
    }
    Ok(tool.result_format.clone())
}

/// Resolve only tools declared by the daemon, not schemas carried by core or
/// script input. Basal inserts its own model tools separately.
///
/// Every entry must be served in codemode by its provider (see
/// `codemode_served`); one entry that is not refuses the whole catalog.
/// Every provider catalog is read to freeze result formats, including legacy
/// providers whose capability admits codemode without per-tool mode opt-in.
/// Each module is read once, with requests bounded by the transport timeout;
/// callers must not hold a store lock across this function.
pub fn catalog(
    transport: &dyn Transport,
    daemon: &Value,
    entries: &[CatalogEntry],
) -> Result<Value, CatalogError> {
    let modules = daemon["modules"]
        .as_array()
        .ok_or_else(|| WireError::Unreadable("daemon catalog has no modules".into()))?;
    let mut answers = BTreeMap::new();
    let mut resolved = Vec::new();
    for entry in entries {
        if entry.tool == "codemode" || (entry.module == "basal" && entry.op == "codemode") {
            return Err(WireError::Unreadable("recursive codemode catalog entry".into()).into());
        }
        let module = modules
            .iter()
            .find(|module| module["module_id"] == entry.module)
            .ok_or_else(|| WireError::NeverSent(format!("provider {} is absent", entry.module)))?;
        let result_format =
            codemode_served(transport, module, entry, &mut answers).map_err(CatalogError::Tool)?;
        let roles: Vec<subc_protocol::manifest::ProviderRole> =
            serde_json::from_value(module["roles"].clone())
                .map_err(|e| WireError::Unreadable(e.to_string()))?;
        let mut schemas = roles
            .into_iter()
            .filter_map(|role| match role {
                subc_protocol::manifest::ProviderRole::ToolProvider { tools, .. } => Some(tools),
                _ => None,
            })
            .flatten()
            .filter(|tool| tool.name == entry.op)
            .map(|tool| tool.schema);
        let schema = schemas.next().ok_or_else(|| {
            WireError::NeverSent(format!("tool {}.{} is absent", entry.module, entry.op))
        })?;
        if schemas.next().is_some() {
            return Err(WireError::Unreadable("ambiguous tool declaration".into()).into());
        }
        let mut resolved_tool = serde_json::json!({"name":entry.tool,"module":entry.module,"op":entry.op,"input_schema":schema});
        if let Some(format) = result_format {
            resolved_tool["result_format"] = serde_json::to_value(format).unwrap();
        }
        resolved.push(resolved_tool);
    }
    Ok(Value::Array(resolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_run_scope_wire_shapes() {
        let request = Open {
            agent_id: "agent-1".into(),
            invoking_scope: Scope {
                reference: "agent:1".into(),
                epoch: 7,
            },
            run_id: "run-1".into(),
            expires_at_ms: 2_460_123,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({"agent_id":"agent-1","invoking_scope":{"ref":"agent:1","epoch":7},"run_id":"run-1","expires_at_ms":2_460_123})
        );
        let reply = json!({"scope":{"ref":"run:1","epoch":9},"expires_at_ms":2_460_123,"catalog":[{"tool":"read","module":"aft","op":"read"}]});
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<Opened>(reply.clone()).unwrap()).unwrap(),
            reply
        );
        assert_eq!(
            serde_json::to_value(Close {
                run_id: "run-1".into(),
                epoch: 9
            })
            .unwrap(),
            json!({"run_id":"run-1","epoch":9})
        );
        let reply = json!({"closed":false,"already_closed":true});
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<Closed>(reply.clone()).unwrap()).unwrap(),
            reply
        );
    }
}

#[cfg(test)]
mod call_mode_tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// A provider `mock` whose `tool.catalog` answer is set by each test.
    /// The daemon catalog is passed to `catalog` directly, so this transport
    /// only answers the provider's own role ops and records each one.
    struct Provider {
        catalog: Result<Value, WireError>,
        calls: Mutex<Vec<String>>,
    }

    impl Provider {
        fn answering(catalog: Result<Value, WireError>) -> Self {
            Self {
                catalog,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Transport for Provider {
        fn catalog(&self) -> Result<Value, WireError> {
            panic!("the daemon catalog is passed in, never read here")
        }
        fn management(&self, _: &str, op: &str, _: Value) -> Result<Value, WireError> {
            panic!("unexpected management op {op}")
        }
        fn tool(&self, module: &str, name: &str, _: Value, _: &str) -> Result<Value, WireError> {
            assert_eq!(module, "mock");
            self.calls.lock().unwrap().push(name.into());
            match name {
                "role.describe" => Ok(
                    json!({"majors":[{"version":"tool-provider/v1","ops":["role.describe","tool.catalog"],
                        "stability":"alpha"}],"implementation_version":"1"}),
                ),
                "tool.catalog" => self.catalog.clone(),
                other => panic!("unexpected role op {other}"),
            }
        }
    }

    /// A daemon catalog listing `mock`, declaring `provides`, with the tools
    /// `read` and `write`.
    fn daemon(provides: &[&str]) -> Value {
        json!({"modules":[{"module_id":"mock","capabilities":{"provides":provides},"roles":[
            {"role":"tool_provider","tools":[
                {"name":"read","execution_mode":"pure","schema":{"type":"object"}},
                {"name":"write","execution_mode":"mutating","schema":{"type":"object"}}
            ],"identity_scope":[],"concurrency":"serial","emits_push":false,"sub_supervises":false}
        ]}]})
    }

    /// One `tool.catalog` tool entry named `name`, with `modes` set to
    /// `modes`, or without the member when `modes` is `None`.
    fn tool(name: &str, modes: Option<Value>) -> Value {
        let mut tool = json!({"name":name,"schema_digest":"0".repeat(64),"semantics":1,
            "capabilities":[],"input_schema":{"type":"object"}});
        if let Some(modes) = modes {
            tool["modes"] = modes;
        }
        tool
    }

    fn answer(tools: Vec<Value>) -> Value {
        json!({"generation":"g","catalog_digest":"d","tools":tools})
    }

    fn entry(op: &str) -> CatalogEntry {
        CatalogEntry {
            tool: format!("mock_{op}"),
            module: "mock".into(),
            op: op.into(),
        }
    }

    fn refusal(result: Result<Value, CatalogError>) -> ToolRefusal {
        match result {
            Err(CatalogError::Tool(refusal)) => refusal,
            other => panic!("expected a tool refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_call_modes_provider_whose_tool_lists_codemode_is_admitted() {
        let provider = Provider::answering(Ok(answer(vec![
            tool("read", Some(json!(["classic", "codemode"]))),
            tool("write", Some(json!(["codemode"]))),
        ])));
        let resolved = catalog(
            &provider,
            &daemon(&[CALL_MODES_CAPABILITY]),
            &[entry("read"), entry("write")],
        )
        .expect("both tools serve codemode");
        let names: Vec<&str> = resolved
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["mock_read", "mock_write"]);
        assert_eq!(
            provider.calls(),
            ["role.describe", "tool.catalog"],
            "a provider listed for two entries is read once"
        );
    }

    #[test]
    fn a_call_modes_tool_that_does_not_list_codemode_is_refused_by_name() {
        for modes in [
            None,
            Some(json!(["classic"])),
            Some(json!(["classic", "flow"])),
        ] {
            let provider = Provider::answering(Ok(answer(vec![
                tool("read", Some(json!(["codemode"]))),
                tool("write", modes.clone()),
            ])));
            // One refused entry refuses the whole catalog, even after an
            // admitted one.
            let refused = refusal(catalog(
                &provider,
                &daemon(&[CALL_MODES_CAPABILITY]),
                &[entry("read"), entry("write")],
            ));
            assert_eq!(refused.reason, RefusalReason::ModeNotServed, "{modes:?}");
            assert_eq!(refused.reason.code(), "call_mode_unsupported");
            assert_eq!(
                (
                    refused.module.as_str(),
                    refused.tool.as_str(),
                    refused.op.as_str()
                ),
                ("mock", "mock_write", "write")
            );
            assert!(refused.message().contains("mock_write (mock.write)"));
        }
        let unlisted = Provider::answering(Ok(answer(vec![tool("read", None)])));
        let refused = refusal(catalog(
            &unlisted,
            &daemon(&[CALL_MODES_CAPABILITY]),
            &[entry("write")],
        ));
        assert_eq!(refused.reason, RefusalReason::ModeNotServed);
        assert_eq!(refused.op, "write");
    }

    #[test]
    fn result_format_is_frozen_from_each_provider_tool_catalog() {
        for capability in [CALL_MODES_CAPABILITY, LEGACY_RUN_SCOPES_CAPABILITY] {
            let mut read = tool("read", Some(json!(["codemode"])));
            read["result_format"] = json!("content/v1");
            let provider = Provider::answering(Ok(answer(vec![
                read,
                tool("write", Some(json!(["codemode"]))),
            ])));
            let resolved = catalog(
                &provider,
                &daemon(&[capability]),
                &[entry("read"), entry("write")],
            )
            .unwrap();
            assert_eq!(resolved[0]["result_format"], "content/v1");
            assert!(resolved[1].get("result_format").is_none());
            assert_eq!(provider.calls(), ["role.describe", "tool.catalog"]);
        }
    }

    #[test]
    fn an_unreadable_legacy_catalog_refuses_admission_without_guessing_format() {
        // Run routes advertise content/v1 and held-reply/v1, so even legacy
        // providers must disclose each result_format before the run starts.
        let provider = Provider::answering(Err(WireError::TimedOut("offline".into())));
        let refused = refusal(catalog(
            &provider,
            &daemon(&[LEGACY_RUN_SCOPES_CAPABILITY]),
            &[entry("read"), entry("write")],
        ));
        assert_eq!(refused.reason, RefusalReason::CatalogUnreadable);
        assert!(refused.message().contains("retrying the run may succeed"));
        assert_eq!(provider.calls(), ["role.describe", "tool.catalog"]);
    }

    #[test]
    fn a_readable_legacy_catalog_does_not_require_per_tool_mode_opt_in() {
        let provider =
            Provider::answering(Ok(answer(vec![tool("read", None), tool("write", None)])));
        let resolved = catalog(
            &provider,
            &daemon(&[LEGACY_RUN_SCOPES_CAPABILITY]),
            &[entry("read"), entry("write")],
        )
        .unwrap();
        assert_eq!(resolved.as_array().unwrap().len(), 2);
        assert!(
            resolved
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.get("result_format").is_none())
        );
    }

    #[test]
    fn a_provider_declaring_neither_capability_is_refused() {
        let provider =
            Provider::answering(Ok(answer(vec![tool("read", Some(json!(["codemode"])))])));
        let refused = refusal(catalog(
            &provider,
            &daemon(&["flow-scopes/v1"]),
            &[entry("read")],
        ));
        assert_eq!(refused.reason, RefusalReason::NotOptedIn);
        assert_eq!(refused.reason.code(), "provider_not_opted_in");
        assert_eq!(refused.tool, "mock_read");
        assert!(provider.calls().is_empty(), "no catalog read");
    }

    #[test]
    fn an_invalid_modes_list_refuses_the_answer_as_invalid() {
        for modes in [
            json!([]),
            json!(["codemode", "codemode"]),
            json!(["codemode", "someday"]),
        ] {
            // The invalid list is on another tool: it still invalidates the
            // whole answer.
            let provider = Provider::answering(Ok(answer(vec![
                tool("read", Some(json!(["codemode"]))),
                tool("write", Some(modes.clone())),
            ])));
            let refused = refusal(catalog(
                &provider,
                &daemon(&[CALL_MODES_CAPABILITY]),
                &[entry("read")],
            ));
            assert_eq!(refused.reason, RefusalReason::InvalidAnswer, "{modes}");
            assert_eq!(refused.reason.code(), "invalid_answer");
            assert!(
                refused.problem.contains("write modes"),
                "{}",
                refused.problem
            );
        }
        let twice = Provider::answering(Ok(answer(vec![
            tool("read", Some(json!(["codemode"]))),
            tool("read", Some(json!(["codemode"]))),
        ])));
        let refused = refusal(catalog(
            &twice,
            &daemon(&[CALL_MODES_CAPABILITY]),
            &[entry("read")],
        ));
        assert_eq!(refused.reason, RefusalReason::InvalidAnswer);
    }

    #[test]
    fn an_unreadable_catalog_is_refused_and_never_admitted() {
        for failure in [
            Err(WireError::TimedOut("no reply".into())),
            Err(WireError::Unknown("route closed".into())),
            // `modes` must be a list of strings; this does not decode.
            Ok(answer(vec![tool("read", Some(json!("codemode")))])),
        ] {
            let provider = Provider::answering(failure.clone());
            let refused = refusal(catalog(
                &provider,
                &daemon(&[CALL_MODES_CAPABILITY]),
                &[entry("read")],
            ));
            assert_eq!(
                refused.reason,
                RefusalReason::CatalogUnreadable,
                "{failure:?}"
            );
            assert_eq!(refused.reason.code(), "catalog_unreadable");
            assert_eq!(refused.tool, "mock_read");
        }
    }
}
