//! prefrontal-core authorizes run scopes. The invoking tool route's daemon stamp
//! records the verified principal, scope owner, reference, epoch and agent_id.
//! Basal sends that agent and scope identity with a fixed expiry to core over its
//! own route stamped reserved:basal, the daemon-verified basal module principal.
//! The confined worker never makes these authority-bearing control requests.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use cortexkit_role_tool_provider::catalog::{CatalogAnswer, check_modes, serves_call_mode};
use cortexkit_role_tool_provider::{CALL_MODES_CAPABILITY, CallMode};

use crate::subc_catalog::{CORE, read_tool_catalog_on_run};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
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

/// The harness label on the routes basal binds to read a provider's
/// `role.describe` and `tool.catalog` at codemode admission. AFT accepts only
/// host harness names in a bind (`opencode`, `pi`, `runner`, `mcp:<client>`,
/// `fed:<fingerprint>`) and refuses basal's own generic `basal` label;
/// `runner` is AFT's label for headless executors. The harness is bind
/// configuration only: the daemon still stamps the route as basal's, under
/// the run's scope.
pub const CATALOG_READ_HARNESS: &str = "runner";

/// The bind identity for a codemode run's admission-time catalog reads: the
/// invoking caller's project root, the `runner` harness and a session naming
/// the run. Tool calls inside the run use the caller's forwarded identity.
pub fn catalog_read_identity(
    project_root: &std::path::Path,
    run_id: &str,
) -> subc_protocol::BindIdentity {
    subc_protocol::BindIdentity::new(
        project_root,
        CATALOG_READ_HARNESS,
        format!("basal:codemode:{run_id}"),
    )
}

/// The scope and bind identity a codemode run's admission-time catalog reads
/// use: the run's own scope, which core has registered before admission reads
/// catalogs, and `catalog_read_identity`.
pub struct CatalogRead<'a> {
    pub scope: &'a subc_protocol::scope::ScopeSelector,
    pub identity: &'a subc_protocol::BindIdentity,
}

/// Whether a failed catalog read may succeed if the run is retried: the
/// connection was lost, the reply timed out, or the provider or daemon
/// refused with a code the tool-provider role marks transient (such as
/// `scope_not_synced` or `module_reloading`). A permanent refusal, such as
/// `principal_denied`, or an answer basal can't read fails the same way again.
///
/// `run_parent_unavailable` also counts as recoverable: a provider such as
/// cerebellum refuses a run's bind until the head has a live route to it, and
/// that changes as soon as the head calls the provider.
pub fn catalog_read_may_recover(error: &WireError) -> bool {
    match error {
        WireError::TimedOut(_) | WireError::Unknown(_) => true,
        WireError::Typed(refusal) => matches!(
            refusal.reason,
            crate::flow_refusal::RefusalReason::ScopeNotSynced
                | crate::flow_refusal::RefusalReason::ScopeChanged
        ),
        WireError::Refused { code, .. } | WireError::RefusedDetails { code, .. } => {
            code == "run_parent_unavailable"
                || cortexkit_role_tool_provider::errors::is_transient(code)
        }
        _ => false,
    }
}

/// Why basal left one catalog entry out of a codemode run.
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
    /// The reason code a dropped tool reports.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotOptedIn => "provider_not_opted_in",
            Self::ModeNotServed => "call_mode_unsupported",
            Self::CatalogUnreadable => "catalog_unreadable",
            Self::InvalidAnswer => "invalid_answer",
        }
    }

    /// Whether the reason is about the provider's whole answer rather than
    /// one tool. Such a provider has every one of its entries dropped, since
    /// no result format can be trusted from an answer basal could not read
    /// or that is invalid.
    fn whole_provider(self) -> bool {
        matches!(self, Self::CatalogUnreadable | Self::InvalidAnswer)
    }
}

/// Catalog entries basal dropped from a codemode run, all from one provider
/// and for one reason. The run still starts with the other entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogGap {
    pub reason: RefusalReason,
    pub module: String,
    /// The run's names for the dropped tools, in catalog order.
    pub tools: Vec<String>,
    /// Plain-language detail, such as the read error or the failing check.
    pub problem: String,
}

impl CatalogGap {
    pub fn message(&self) -> String {
        format!(
            "codemode can't use {} from provider {}: {}",
            self.tools.join(", "),
            self.module,
            self.problem
        )
    }
}

/// The tools a codemode run may call, and the entries left out of it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCatalog {
    /// The usable entries, each `{name, module, op, input_schema,
    /// result_format?}`, in the order core sent them.
    pub catalog: Value,
    pub gaps: Vec<CatalogGap>,
    /// The providers with usable entries whose preset-less catalog answer
    /// lists the run's preset in `presets` (see `provider_serves_preset`).
    /// Only tool calls to these providers carry the run's preset.
    pub preset_modules: BTreeSet<String>,
}

/// Whether the provider that gave `answer` serves `preset`: its preset-less
/// catalog answer lists that preset in `presets`. Basal reads these answers
/// when the run starts and always without a preset, which is the only place
/// the contract allows the list to appear.
///
/// A provider that lists no presets, or lists other presets, gets no preset
/// on its tool calls: the call then runs the provider's default variant,
/// since an absent `preset` means exactly that (tool-provider contract §4.1),
/// and a provider must serve every preset it lists (contract §4.3).
pub fn provider_serves_preset(answer: &CatalogAnswer, preset: &str) -> bool {
    answer.lists_preset(preset)
}

/// Whether the provider serves `entry` in codemode, as it declares it. A
/// provider declaring the legacy capability serves every tool. A provider
/// declaring `call-modes/v1` serves the tools whose `modes`, in its own
/// `tool.catalog` answer, include `codemode`. `answers` holds each module's
/// answer once read, so a provider listed for several entries is read once.
fn codemode_served(
    transport: &dyn Transport,
    read: &CatalogRead<'_>,
    module: &Value,
    entry: &CatalogEntry,
    answers: &mut BTreeMap<String, Result<CatalogAnswer, WireError>>,
) -> Result<Option<cortexkit_role_tool_provider::result::ResultFormat>, (RefusalReason, String)> {
    let provides = |token: &str| {
        module["capabilities"]["provides"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|cap| cap == token))
    };
    let legacy = provides(LEGACY_RUN_SCOPES_CAPABILITY);
    if !legacy && !provides(CALL_MODES_CAPABILITY) {
        return Err((
            RefusalReason::NotOptedIn,
            format!(
                "it declares neither {CALL_MODES_CAPABILITY} nor {LEGACY_RUN_SCOPES_CAPABILITY}"
            ),
        ));
    }
    let answer = answers.entry(entry.module.clone()).or_insert_with(|| {
        read_tool_catalog_on_run(transport, read.scope, read.identity, &entry.module)
    });
    let answer = answer.as_ref().map_err(|error| {
        let hint = if catalog_read_may_recover(error) {
            "; retrying the run may succeed"
        } else {
            ""
        };
        (
            RefusalReason::CatalogUnreadable,
            format!("its tool.catalog could not be read: {error:?}{hint}"),
        )
    })?;
    // An invalid modes list on any tool makes the whole answer invalid, not
    // only that tool's entry.
    for tool in &answer.tools {
        check_modes(tool).map_err(|problem| {
            (
                RefusalReason::InvalidAnswer,
                format!("its tool.catalog answer is invalid: {problem}"),
            )
        })?;
    }
    let mut listed = answer.tools.iter().filter(|tool| tool.name == entry.op);
    let Some(tool) = listed.next() else {
        return Err((
            RefusalReason::ModeNotServed,
            format!("its tool.catalog does not list {}", entry.op),
        ));
    };
    if listed.next().is_some() {
        return Err((
            RefusalReason::InvalidAnswer,
            format!("its tool.catalog lists {} more than once", entry.op),
        ));
    }
    if !legacy && !serves_call_mode(tool, &CallMode::Codemode) {
        return Err((
            RefusalReason::ModeNotServed,
            format!("the modes of {} do not include codemode", entry.op),
        ));
    }
    Ok(tool.result_format.clone())
}

/// Resolve only tools declared by the daemon, not schemas carried by core or
/// script input. Basal inserts its own model tools separately.
///
/// Core's entries must name tools the daemon catalog declares, once each, and
/// never codemode itself; any entry that does not fails the whole catalog.
///
/// An entry its provider does not serve in codemode (see `codemode_served`)
/// is dropped and named in a gap; the run still gets the other entries. When
/// a provider's `role.describe` or `tool.catalog` can't be read, or its answer
/// is invalid, every entry from that provider is dropped: basal never guesses
/// a result format. Every provider catalog is read, including legacy
/// providers whose capability admits codemode without per-tool mode opt-in.
///
/// The reads go over the run's own scope and bind with the identity in `read`
/// (see `CatalogRead`). Each module is read once, with requests bounded by
/// the transport timeout; callers must not hold a store lock across this
/// function.
///
/// `preset` is core's preset for the run, when it chose one. It is never sent
/// on these reads; it only decides which providers `preset_modules` names.
pub fn catalog(
    transport: &dyn Transport,
    read: &CatalogRead<'_>,
    daemon: &Value,
    entries: &[CatalogEntry],
    preset: Option<&str>,
) -> Result<ResolvedCatalog, WireError> {
    let modules = daemon["modules"]
        .as_array()
        .ok_or_else(|| WireError::Unreadable("daemon catalog has no modules".into()))?;
    let mut answers = BTreeMap::new();
    // Each usable entry with its provider, so a provider that fails on a later
    // entry can still have its earlier entries removed.
    let mut usable: Vec<(&str, Value)> = Vec::new();
    let mut gaps: Vec<CatalogGap> = Vec::new();
    // Providers whose whole answer is unusable, in the order first seen.
    let mut failed: Vec<(&str, RefusalReason, String)> = Vec::new();
    for entry in entries {
        if entry.tool == "codemode" || (entry.module == "basal" && entry.op == "codemode") {
            return Err(WireError::Unreadable(
                "recursive codemode catalog entry".into(),
            ));
        }
        let module = modules
            .iter()
            .find(|module| module["module_id"] == entry.module)
            .ok_or_else(|| WireError::NeverSent(format!("provider {} is absent", entry.module)))?;
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
            return Err(WireError::Unreadable("ambiguous tool declaration".into()));
        }
        if failed.iter().any(|(module, ..)| *module == entry.module) {
            continue;
        }
        match codemode_served(transport, read, module, entry, &mut answers) {
            Ok(result_format) => {
                let mut resolved = serde_json::json!({"name":entry.tool,"module":entry.module,"op":entry.op,"input_schema":schema});
                if let Some(format) = result_format {
                    resolved["result_format"] = serde_json::to_value(format).unwrap();
                }
                usable.push((&entry.module, resolved));
            }
            Err((reason, problem)) if reason.whole_provider() => {
                failed.push((&entry.module, reason, problem));
            }
            Err((reason, problem)) => {
                match gaps.iter_mut().find(|gap| {
                    gap.module == entry.module && gap.reason == reason && gap.problem == problem
                }) {
                    Some(gap) => gap.tools.push(entry.tool.clone()),
                    None => gaps.push(CatalogGap {
                        reason,
                        module: entry.module.clone(),
                        tools: vec![entry.tool.clone()],
                        problem,
                    }),
                }
            }
        }
    }
    // A failed provider's gap names all of its entries and replaces any
    // per-tool gap recorded for it before the failure was seen.
    let is_failed = |module: &str| failed.iter().any(|(failed, ..)| *failed == module);
    usable.retain(|(module, _)| !is_failed(module));
    gaps.retain(|gap| !is_failed(&gap.module));
    let preset_modules = answers
        .iter()
        .filter(|(module, answer)| {
            let Some(preset) = preset else {
                // Core chose no preset, so no tool call names one.
                return false;
            };
            !is_failed(module)
                && usable.iter().any(|(usable, _)| *usable == module.as_str())
                && answer
                    .as_ref()
                    .is_ok_and(|answer| provider_serves_preset(answer, preset))
        })
        .map(|(module, _)| module.clone())
        .collect();
    for (module, reason, problem) in &failed {
        gaps.push(CatalogGap {
            reason: *reason,
            module: (*module).to_owned(),
            tools: entries
                .iter()
                .filter(|entry| entry.module == *module)
                .map(|entry| entry.tool.clone())
                .collect(),
            problem: problem.clone(),
        });
    }
    Ok(ResolvedCatalog {
        catalog: Value::Array(usable.into_iter().map(|(_, tool)| tool).collect()),
        gaps,
        preset_modules,
    })
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
    use subc_protocol::BindIdentity;
    use subc_protocol::scope::ScopeSelector;

    /// Providers whose `tool.catalog` answers are set by each test. The
    /// daemon catalog is passed to `catalog` directly, so this transport only
    /// answers the providers' own role ops and records each one. Like AFT, it
    /// refuses a bind whose harness is not a host harness name, so a read
    /// with basal's generic `basal` identity never gets an answer. Like
    /// cerebellum, it admits basal only on the run's own scope, refusing any
    /// other route `principal_denied`, and refuses a `tool.catalog` request
    /// carrying a member the role does not define.
    struct Provider {
        catalogs: BTreeMap<String, Result<Value, WireError>>,
        calls: Mutex<Vec<(String, String)>>,
        identities: Mutex<Vec<BindIdentity>>,
        scopes: Mutex<Vec<ScopeSelector>>,
    }

    impl Provider {
        fn answering(catalog: Result<Value, WireError>) -> Self {
            Self::answering_each([("mock", catalog)])
        }
        fn answering_each<const N: usize>(catalogs: [(&str, Result<Value, WireError>); N]) -> Self {
            Self {
                catalogs: catalogs
                    .into_iter()
                    .map(|(module, catalog)| (module.to_owned(), catalog))
                    .collect(),
                calls: Mutex::new(Vec::new()),
                identities: Mutex::new(Vec::new()),
                scopes: Mutex::new(Vec::new()),
            }
        }
        /// The role ops sent to `mock`, in order.
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(module, _)| module == "mock")
                .map(|(_, name)| name.clone())
                .collect()
        }
        fn answer(&self, module: &str, name: &str, arguments: &Value) -> Result<Value, WireError> {
            self.calls
                .lock()
                .unwrap()
                .push((module.into(), name.into()));
            match name {
                "role.describe" => Ok(
                    json!({"majors":[{"version":"tool-provider/v1","ops":["role.describe","tool.catalog"],
                        "stability":"alpha"}],"implementation_version":"1"}),
                ),
                "tool.catalog" => {
                    let allowed = [
                        "params",
                        "preset",
                        "composition",
                        "system_text",
                        "digest_only",
                    ];
                    if let Some(member) = arguments.as_object().and_then(|members| {
                        members.keys().find(|key| !allowed.contains(&key.as_str()))
                    }) {
                        return Err(WireError::Refused {
                            code: "invalid_request".into(),
                            message: format!("unknown catalog member {member}"),
                        });
                    }
                    self.catalogs[module].clone()
                }
                other => panic!("unexpected role op {other}"),
            }
        }
    }

    /// AFT's bind check: the harness must be a host harness name.
    fn host_harness(harness: &str) -> bool {
        matches!(harness, "opencode" | "pi" | "runner")
            || harness.starts_with("mcp:")
            || harness.starts_with("fed:")
    }

    impl Transport for Provider {
        fn catalog(&self) -> Result<Value, WireError> {
            panic!("the daemon catalog is passed in, never read here")
        }
        fn management(&self, _: &str, op: &str, _: Value) -> Result<Value, WireError> {
            panic!("unexpected management op {op}")
        }
        fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
            // An unscoped route reaches the provider as a classic call.
            Err(WireError::Refused {
                code: "principal_denied".into(),
                message: "basal may call only on a codemode run scope".into(),
            })
        }
        fn role_on_run_scope(
            &self,
            scope: &ScopeSelector,
            identity: &BindIdentity,
            module: &str,
            name: &str,
            arguments: Value,
        ) -> Result<Value, WireError> {
            self.identities.lock().unwrap().push(identity.clone());
            self.scopes.lock().unwrap().push(scope.clone());
            if scope != &run_selector() {
                return Err(WireError::Refused {
                    code: "principal_denied".into(),
                    message: "basal may call only on a codemode run scope".into(),
                });
            }
            if !host_harness(&identity.harness) {
                return Err(WireError::NeverSent(
                    "config_divergence: configure payload invalid field 'harness'".into(),
                ));
            }
            self.answer(module, name, &arguments)
        }
    }

    /// The run scope core opened for `run-1`.
    fn run_selector() -> ScopeSelector {
        ScopeSelector::new(
            subc_protocol::Principal::Reserved {
                module_id: CORE.into(),
            },
            "run:1",
            Some(9),
        )
    }

    fn identity() -> BindIdentity {
        catalog_read_identity(std::path::Path::new("/agent/project"), "run-1")
    }

    fn provider_module(module: &str, provides: &[&str]) -> Value {
        json!({"module_id":module,"capabilities":{"provides":provides},"roles":[
            {"role":"tool_provider","tools":[
                {"name":"read","execution_mode":"pure","schema":{"type":"object"}},
                {"name":"write","execution_mode":"mutating","schema":{"type":"object"}}
            ],"identity_scope":[],"concurrency":"serial","emits_push":false,"sub_supervises":false}
        ]})
    }

    /// A daemon catalog listing `mock`, declaring `provides`, with the tools
    /// `read` and `write`.
    fn daemon(provides: &[&str]) -> Value {
        json!({"modules":[provider_module("mock", provides)]})
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
        module_entry("mock", op)
    }

    fn module_entry(module: &str, op: &str) -> CatalogEntry {
        CatalogEntry {
            tool: format!("{module}_{op}"),
            module: module.into(),
            op: op.into(),
        }
    }

    fn resolve(provider: &Provider, daemon: &Value, entries: &[CatalogEntry]) -> ResolvedCatalog {
        read_catalog(provider, daemon, entries).expect("the catalog resolves")
    }

    fn read_catalog(
        provider: &Provider,
        daemon: &Value,
        entries: &[CatalogEntry],
    ) -> Result<ResolvedCatalog, WireError> {
        let (scope, identity) = (run_selector(), identity());
        let read = CatalogRead {
            scope: &scope,
            identity: &identity,
        };
        catalog(provider, &read, daemon, entries, None)
    }

    /// A provider answer whose one tool serves codemode and whose answer
    /// lists `presets`.
    fn answer_listing(presets: &[&str]) -> Value {
        let mut answer = answer(vec![tool("read", Some(json!(["codemode"])))]);
        answer["presets"] = json!(presets);
        answer
    }

    /// Resolve the catalog of `provider` for a run whose core preset is
    /// `preset`.
    fn preset_modules_for(provider: &Provider, preset: Option<&str>) -> BTreeSet<String> {
        let (scope, identity) = (run_selector(), identity());
        let read = CatalogRead {
            scope: &scope,
            identity: &identity,
        };
        catalog(
            provider,
            &read,
            &daemon(&[CALL_MODES_CAPABILITY]),
            &[entry("read")],
            preset,
        )
        .expect("the catalog resolves")
        .preset_modules
    }

    fn names(resolved: &ResolvedCatalog) -> Vec<&str> {
        resolved
            .catalog
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect()
    }

    /// The only gap in `resolved`.
    fn gap(resolved: &ResolvedCatalog) -> &CatalogGap {
        assert_eq!(resolved.gaps.len(), 1, "{:?}", resolved.gaps);
        &resolved.gaps[0]
    }

    #[test]
    fn a_provider_listing_the_runs_preset_serves_it() {
        let provider = Provider::answering(Ok(answer_listing(&["head", "worker", "reader"])));
        assert_eq!(
            preset_modules_for(&provider, Some("worker")),
            BTreeSet::from(["mock".to_owned()])
        );
    }

    #[test]
    fn a_provider_listing_other_presets_serves_none() {
        let provider = Provider::answering(Ok(answer_listing(&["head", "worker"])));
        assert!(preset_modules_for(&provider, Some("reader")).is_empty());
    }

    #[test]
    fn a_provider_listing_no_presets_serves_none() {
        let provider =
            Provider::answering(Ok(answer(vec![tool("read", Some(json!(["codemode"])))])));
        assert!(preset_modules_for(&provider, Some("worker")).is_empty());
    }

    /// A tool tagged `code.read/v1` once marked a provider as serving presets.
    /// That stand-in is gone: a preset must be listed in the answer.
    #[test]
    fn a_code_reading_tool_without_a_listed_preset_serves_none() {
        let mut tagged = tool("read", Some(json!(["codemode"])));
        tagged["capabilities"] = json!(["code.read/v1"]);
        let provider = Provider::answering(Ok(answer(vec![tagged])));
        assert!(preset_modules_for(&provider, Some("worker")).is_empty());
    }

    #[test]
    fn a_run_without_a_preset_serves_none_whatever_the_answer_lists() {
        let provider = Provider::answering(Ok(answer_listing(&["head", "worker", "reader"])));
        assert!(preset_modules_for(&provider, None).is_empty());
    }

    /// Basal reads catalogs when the run starts, and this is the only request
    /// whose answer may list presets, so the read never names the run's.
    #[test]
    fn catalog_reads_name_no_preset_even_when_the_run_has_one() {
        let provider = Provider::answering(Ok(answer_listing(&["worker"])));
        let resolved = preset_modules_for(&provider, Some("worker"));
        assert_eq!(resolved, BTreeSet::from(["mock".to_owned()]));
        assert_eq!(provider.calls(), ["role.describe", "tool.catalog"]);
    }

    #[test]
    fn catalog_reads_bind_as_a_runner_with_the_callers_project_root() {
        let provider =
            Provider::answering(Ok(answer(vec![tool("read", Some(json!(["codemode"])))])));
        let resolved = resolve(
            &provider,
            &daemon(&[CALL_MODES_CAPABILITY]),
            &[entry("read")],
        );
        assert_eq!(names(&resolved), ["mock_read"]);
        assert!(resolved.gaps.is_empty());
        let expected = BindIdentity::new("/agent/project", "runner", "basal:codemode:run-1");
        assert_eq!(
            provider.identities.lock().unwrap().as_slice(),
            [expected.clone(), expected],
            "role.describe and tool.catalog both bind as the runner"
        );
        assert_eq!(
            provider.scopes.lock().unwrap().as_slice(),
            [run_selector(), run_selector()],
            "role.describe and tool.catalog both go over the run's own scope"
        );
    }

    #[test]
    fn a_call_modes_provider_whose_tool_lists_codemode_is_admitted() {
        let provider = Provider::answering(Ok(answer(vec![
            tool("read", Some(json!(["classic", "codemode"]))),
            tool("write", Some(json!(["codemode"]))),
        ])));
        let resolved = resolve(
            &provider,
            &daemon(&[CALL_MODES_CAPABILITY]),
            &[entry("read"), entry("write")],
        );
        assert_eq!(names(&resolved), ["mock_read", "mock_write"]);
        assert!(resolved.gaps.is_empty());
        assert_eq!(
            provider.calls(),
            ["role.describe", "tool.catalog"],
            "a provider listed for two entries is read once"
        );
    }

    #[test]
    fn a_call_modes_tool_that_does_not_list_codemode_is_dropped_by_name() {
        for modes in [
            None,
            Some(json!(["classic"])),
            Some(json!(["classic", "flow"])),
        ] {
            let provider = Provider::answering(Ok(answer(vec![
                tool("read", Some(json!(["codemode"]))),
                tool("write", modes.clone()),
            ])));
            // Only the tool that does not serve codemode is dropped.
            let resolved = resolve(
                &provider,
                &daemon(&[CALL_MODES_CAPABILITY]),
                &[entry("read"), entry("write")],
            );
            assert_eq!(names(&resolved), ["mock_read"], "{modes:?}");
            let dropped = gap(&resolved);
            assert_eq!(dropped.reason, RefusalReason::ModeNotServed, "{modes:?}");
            assert_eq!(dropped.reason.code(), "call_mode_unsupported");
            assert_eq!(
                (dropped.module.as_str(), dropped.tools.as_slice()),
                ("mock", ["mock_write".to_owned()].as_slice())
            );
            assert!(dropped.message().contains("mock_write from provider mock"));
        }
        let unlisted = Provider::answering(Ok(answer(vec![tool("read", None)])));
        let resolved = resolve(
            &unlisted,
            &daemon(&[CALL_MODES_CAPABILITY]),
            &[entry("write")],
        );
        assert!(names(&resolved).is_empty());
        assert_eq!(gap(&resolved).reason, RefusalReason::ModeNotServed);
        assert_eq!(gap(&resolved).tools, ["mock_write"]);
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
            let resolved = resolve(
                &provider,
                &daemon(&[capability]),
                &[entry("read"), entry("write")],
            );
            assert_eq!(resolved.catalog[0]["result_format"], "content/v1");
            assert!(resolved.catalog[1].get("result_format").is_none());
            assert_eq!(provider.calls(), ["role.describe", "tool.catalog"]);
        }
    }

    #[test]
    fn an_unreadable_provider_is_dropped_while_another_provider_still_resolves() {
        // Run routes advertise content/v1 and held-reply/v1, so even legacy
        // providers must disclose each result_format: an unreadable one has
        // every entry dropped rather than a guessed format.
        let provider = Provider::answering_each([
            ("mock", Err(WireError::TimedOut("offline".into()))),
            (
                "other",
                Ok(answer(vec![tool("read", Some(json!(["codemode"])))])),
            ),
        ]);
        let daemon = json!({"modules":[
            provider_module("mock", &[LEGACY_RUN_SCOPES_CAPABILITY]),
            provider_module("other", &[CALL_MODES_CAPABILITY]),
        ]});
        let resolved = resolve(
            &provider,
            &daemon,
            &[entry("read"), module_entry("other", "read"), entry("write")],
        );
        assert_eq!(names(&resolved), ["other_read"]);
        let dropped = gap(&resolved);
        assert_eq!(dropped.reason, RefusalReason::CatalogUnreadable);
        assert_eq!(dropped.reason.code(), "catalog_unreadable");
        assert_eq!(dropped.module, "mock");
        assert_eq!(dropped.tools, ["mock_read", "mock_write"]);
        // A timeout may clear up, so the message says a retry may succeed.
        assert!(dropped.message().contains("retrying the run may succeed"));
        assert_eq!(provider.calls(), ["role.describe", "tool.catalog"]);
    }

    #[test]
    fn only_a_recoverable_catalog_read_failure_suggests_retrying_the_run() {
        let refused = |code: &str| WireError::Refused {
            code: code.into(),
            message: "refused".into(),
        };
        for (failure, recoverable) in [
            (WireError::TimedOut("no reply".into()), true),
            (WireError::Unknown("connection lost".into()), true),
            (refused("module_reloading"), true),
            (refused("scope_not_synced"), true),
            (refused("principal_denied"), false),
            // Clears once the head has a live route to the provider.
            (refused("run_parent_unavailable"), true),
            (WireError::Unreadable("garbled".into()), false),
        ] {
            assert_eq!(
                catalog_read_may_recover(&failure),
                recoverable,
                "{failure:?}"
            );
            let provider = Provider::answering(Err(failure.clone()));
            let resolved = resolve(
                &provider,
                &daemon(&[CALL_MODES_CAPABILITY]),
                &[entry("read")],
            );
            let dropped = gap(&resolved);
            assert_eq!(dropped.reason, RefusalReason::CatalogUnreadable);
            assert_eq!(
                dropped.message().contains("retrying the run may succeed"),
                recoverable,
                "{failure:?}: {}",
                dropped.message()
            );
        }
    }

    #[test]
    fn a_readable_legacy_catalog_does_not_require_per_tool_mode_opt_in() {
        let provider =
            Provider::answering(Ok(answer(vec![tool("read", None), tool("write", None)])));
        let resolved = resolve(
            &provider,
            &daemon(&[LEGACY_RUN_SCOPES_CAPABILITY]),
            &[entry("read"), entry("write")],
        );
        assert_eq!(names(&resolved), ["mock_read", "mock_write"]);
        assert!(
            resolved
                .catalog
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.get("result_format").is_none())
        );
    }

    #[test]
    fn a_provider_declaring_neither_capability_is_dropped_unread() {
        let provider =
            Provider::answering(Ok(answer(vec![tool("read", Some(json!(["codemode"])))])));
        let resolved = resolve(
            &provider,
            &daemon(&["flow-scopes/v1"]),
            &[entry("read"), entry("write")],
        );
        assert!(names(&resolved).is_empty());
        let dropped = gap(&resolved);
        assert_eq!(dropped.reason, RefusalReason::NotOptedIn);
        assert_eq!(dropped.reason.code(), "provider_not_opted_in");
        assert_eq!(dropped.tools, ["mock_read", "mock_write"]);
        assert!(provider.calls().is_empty(), "no catalog read");
    }

    #[test]
    fn an_invalid_answer_drops_every_entry_from_its_provider() {
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
            let resolved = resolve(
                &provider,
                &daemon(&[CALL_MODES_CAPABILITY]),
                &[entry("read")],
            );
            assert!(names(&resolved).is_empty(), "{modes}");
            let dropped = gap(&resolved);
            assert_eq!(dropped.reason, RefusalReason::InvalidAnswer, "{modes}");
            assert_eq!(dropped.reason.code(), "invalid_answer");
            assert!(
                dropped.problem.contains("write modes"),
                "{}",
                dropped.problem
            );
        }
        // `write` is listed twice. `read` was usable before that was seen,
        // and is dropped with it.
        let twice = Provider::answering(Ok(answer(vec![
            tool("read", Some(json!(["codemode"]))),
            tool("write", Some(json!(["codemode"]))),
            tool("write", Some(json!(["codemode"]))),
        ])));
        let resolved = resolve(
            &twice,
            &daemon(&[CALL_MODES_CAPABILITY]),
            &[entry("read"), entry("write")],
        );
        assert!(names(&resolved).is_empty());
        assert_eq!(gap(&resolved).reason, RefusalReason::InvalidAnswer);
        assert_eq!(gap(&resolved).tools, ["mock_read", "mock_write"]);
    }

    #[test]
    fn an_unreadable_catalog_is_dropped_and_never_admitted() {
        for failure in [
            Err(WireError::TimedOut("no reply".into())),
            Err(WireError::Unknown("route closed".into())),
            // `modes` must be a list of strings; this does not decode.
            Ok(answer(vec![tool("read", Some(json!("codemode")))])),
        ] {
            let provider = Provider::answering(failure.clone());
            let resolved = resolve(
                &provider,
                &daemon(&[CALL_MODES_CAPABILITY]),
                &[entry("read")],
            );
            assert!(names(&resolved).is_empty(), "{failure:?}");
            assert_eq!(
                gap(&resolved).reason,
                RefusalReason::CatalogUnreadable,
                "{failure:?}"
            );
            assert_eq!(gap(&resolved).tools, ["mock_read"]);
        }
    }

    #[test]
    fn invalid_core_entries_still_fail_the_whole_catalog() {
        let provider =
            Provider::answering(Ok(answer(vec![tool("read", Some(json!(["codemode"])))])));
        let mut ambiguous = daemon(&[CALL_MODES_CAPABILITY]);
        let duplicate = ambiguous["modules"][0]["roles"][0].clone();
        ambiguous["modules"][0]["roles"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        let recursive = CatalogEntry {
            tool: "codemode".into(),
            module: "basal".into(),
            op: "codemode".into(),
        };
        for (daemon, bad) in [
            (daemon(&[CALL_MODES_CAPABILITY]), recursive),
            (
                daemon(&[CALL_MODES_CAPABILITY]),
                module_entry("absent", "read"),
            ),
            (daemon(&[CALL_MODES_CAPABILITY]), entry("delete")),
            (ambiguous, entry("read")),
        ] {
            // An unreadable provider listed first does not hide the invalid
            // entry after it.
            let unreadable = Provider::answering(Err(WireError::TimedOut("offline".into())));
            for provider in [&provider, &unreadable] {
                let result = read_catalog(provider, &daemon, &[entry("read"), bad.clone()]);
                assert!(result.is_err(), "{bad:?}: {result:?}");
            }
        }
    }
}
