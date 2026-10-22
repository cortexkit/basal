//! Codemode routes over a real `SubcTransport` against a fake daemon whose
//! providers apply the admission rules of the real ones, so a route basal
//! opens the wrong way is refused here as it is live.
//!
//! The fake daemon stamps each bind the way subc does: with the record of
//! the scope the route was opened under (none for an unscoped open), the
//! opener's principal, and the call mode derived from that stamp
//! (`derive_call_mode`: a scope with `run_id` is codemode, anything else
//! without `flow_id` is classic). Basal can't name a call mode itself; it
//! gets codemode only by opening under the run's scope.
//!
//! - `cerebellum` admits basal only when the bind has an ephemeral scope with
//!   `run_id` and no `flow_id`, owned by an authorized `prefrontal-core`,
//!   principal `reserved:basal` and call mode codemode (else
//!   `principal_denied`), and when the run's parent is linked and its agent
//!   has a live head route (else `run_parent_unavailable`). Role ops
//!   (`role.describe`, `tool.catalog`, `late_results`, `late_results.ack`,
//!   `tool.withdraw`) must carry no envelope `call_key`, `tool.catalog` takes
//!   only the members the role defines, and a tool call needs a `call_key`;
//!   each violation is refused `invalid_request`.
//! - `aft` refuses a bind whose harness is not a host harness name.
//! - `broca` serves model management calls and records the bind's mode.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use subc_protocol::call_mode::{CallMode, derive_call_mode};
use subc_protocol::scope::{ParentState, ScopeKind, ScopeSelector, ScopeStamp};
use subc_protocol::{Flags, Frame, FrameType, Principal, Priority};

const RUN: &str = "run-1";
const AGENT: &str = "agent-1";

fn core() -> Principal {
    Principal::Reserved {
        module_id: "prefrontal-core".into(),
    }
}

/// The run scope core opened, and the selector basal opens routes under.
fn run_selector() -> ScopeSelector {
    ScopeSelector::new(core(), RUN, Some(19))
}

/// The head scope the run was opened from.
fn head_selector() -> ScopeSelector {
    ScopeSelector::new(core(), "agent-scope", Some(7))
}

fn run_stamp(parent_state: &str) -> ScopeStamp {
    serde_json::from_value(json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":RUN,"scope_epoch":19,
        "kind":"ephemeral","parent":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},
        "ref":"agent-scope","scope_epoch":7},"parent_state":parent_state,
        "attributes":{"agent_id":AGENT,"run_id":RUN},"owner_authorized":true
    }))
    .unwrap()
}

fn head_stamp() -> ScopeStamp {
    serde_json::from_value(json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"agent-scope","scope_epoch":7,
        "kind":"head","attributes":{"agent_id":AGENT,"delegates":true},"owner_authorized":true
    }))
    .unwrap()
}

/// One request a provider received: the provider, the request name, the
/// bind's call mode, and whether the envelope carried a `call_key`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Received {
    module: String,
    name: String,
    call_mode: CallMode,
    call_key: bool,
    preset: Option<String>,
    catalog_preset: Option<String>,
}

#[derive(Default)]
struct Daemon {
    /// Live scope records by (ref, epoch), all owned by prefrontal-core.
    scopes: BTreeMap<(String, u64), ScopeStamp>,
    /// Agents with a live head route.
    head_routes: BTreeSet<String>,
    /// Bound routes by channel: provider and the bind's call mode.
    routes: BTreeMap<u16, (String, CallMode)>,
    /// A provider refusal gate used to model AFT's required preset behavior.
    required_preset: Option<String>,
    received: Vec<Received>,
}

fn error_body(code: &str, message: &str) -> Value {
    json!({"code":code,"message":message})
}

fn host_harness(harness: &str) -> bool {
    matches!(harness, "opencode" | "pi" | "runner")
        || harness.starts_with("mcp:")
        || harness.starts_with("fed:")
}

impl Daemon {
    /// Admits or refuses one `route.open`, as subc and the target provider
    /// together would. `Ok` carries the call mode the bind was stamped with.
    fn bind(&self, open: &Value) -> Result<(String, CallMode), Value> {
        let module = open["target"]["module_id"].as_str().unwrap().to_owned();
        let stamp = match open.get("scope") {
            None | Some(Value::Null) => None,
            Some(selector) => {
                let selector: ScopeSelector = serde_json::from_value(selector.clone()).unwrap();
                let key = (
                    selector.scope_ref.clone(),
                    selector.scope_epoch.unwrap_or(0),
                );
                match self.scopes.get(&key).filter(|_| selector.owner == core()) {
                    Some(stamp) => Some(stamp.clone()),
                    None => return Err(error_body("scope_not_live", "no such scope")),
                }
            }
        };
        // The opener's principal: basal, unless the open names another
        // consumer identity.
        let principal = open["consumer_identity"]["module_id"]
            .as_str()
            .unwrap_or("basal")
            .to_owned();
        let call_mode = derive_call_mode(stamp.as_ref());
        match module.as_str() {
            "cerebellum" => {
                let admitted = stamp.as_ref().is_some_and(|stamp| {
                    stamp.kind == ScopeKind::Ephemeral
                        && stamp.attributes.run_id.is_some()
                        && stamp.attributes.flow_id.is_none()
                        && stamp.owner == core()
                        && stamp.owner_authorized
                }) && principal == "basal"
                    && call_mode == CallMode::Codemode;
                if !admitted {
                    return Err(error_body(
                        "principal_denied",
                        "basal may call only on a codemode run scope",
                    ));
                }
                let stamp = stamp.unwrap();
                let parent_live = stamp.parent_state == Some(ParentState::Linked)
                    && stamp
                        .attributes
                        .agent_id
                        .as_ref()
                        .is_some_and(|agent| self.head_routes.contains(agent));
                if !parent_live {
                    return Err(error_body(
                        "run_parent_unavailable",
                        "the run's parent session is not live",
                    ));
                }
            }
            "aft" if !host_harness(open["identity"]["harness"].as_str().unwrap_or("")) => {
                return Err(error_body(
                    "config_divergence",
                    "configure payload invalid field 'harness'",
                ));
            }
            _ => {}
        }
        Ok((module, call_mode))
    }

    /// Answers one request on a bound route, or refuses it with an error body.
    fn answer(&mut self, channel: u16, body: &Value) -> Result<Value, Value> {
        let (module, call_mode) = self.routes[&channel].clone();
        if module == "broca" {
            self.received.push(Received {
                module,
                name: body["method"].as_str().unwrap().into(),
                call_mode,
                call_key: false,
                preset: None,
                catalog_preset: None,
            });
            return Ok(json!({"result":{"state":"active","run_id":"broca-run"}}));
        }
        let name = body["name"].as_str().unwrap().to_owned();
        let call_key = body.get("call_key").is_some();
        let preset = body
            .get("preset")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let catalog_preset = body["arguments"]
            .get("preset")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.received.push(Received {
            module: module.clone(),
            name: name.clone(),
            call_mode,
            call_key,
            preset: preset.clone(),
            catalog_preset: catalog_preset.clone(),
        });
        if let Some(expected) = self.required_preset.as_deref() {
            if preset.as_deref() != Some(expected) {
                return Err(error_body(
                    "invalid_request",
                    "a tool call on a route bound under a scope must name its catalog preset",
                ));
            }
            if name == "tool.catalog" && catalog_preset.as_deref() != Some(expected) {
                return Err(error_body(
                    "invalid_request",
                    "tool.catalog must name the catalog preset",
                ));
            }
        }
        let role_op = matches!(
            name.as_str(),
            "role.describe"
                | "tool.catalog"
                | "late_results"
                | "late_results.ack"
                | "tool.withdraw"
        );
        if module == "cerebellum" {
            if role_op && call_key {
                return Err(error_body(
                    "invalid_request",
                    "role ops carry no envelope call_key",
                ));
            }
            if !role_op && !call_key {
                return Err(error_body("invalid_request", "a tool call needs call_key"));
            }
            if name == "tool.catalog" {
                let allowed = [
                    "params",
                    "preset",
                    "composition",
                    "system_text",
                    "digest_only",
                ];
                if let Some(member) = body["arguments"]
                    .as_object()
                    .into_iter()
                    .flat_map(|members| members.keys())
                    .find(|key| !allowed.contains(&key.as_str()))
                {
                    return Err(error_body(
                        "invalid_request",
                        &format!("unknown catalog member {member}"),
                    ));
                }
            }
        }
        Ok(match name.as_str() {
            "role.describe" => json!({"majors":[{"version":"tool-provider/v1",
                "ops":["role.describe","tool.catalog"],"stability":"alpha"}],"implementation_version":"1"}),
            "tool.catalog" => {
                let tool = if module == "cerebellum" {
                    "recall"
                } else {
                    "read"
                };
                json!({"generation":"g","catalog_digest":"d","tools":[{"name":tool,
                    "schema_digest":"0".repeat(64),"semantics":1,"capabilities":[],
                    "input_schema":{"type":"object"},"modes":["classic","codemode"],
                    "result_format":"content/v1"}]})
            }
            "late_results" => {
                json!({"entries":[],"cursor":{"provider_incarnation":"fake","seq":0},"more":false})
            }
            _ => {
                json!({"content":[{"type":"text","text":"ok"}],"structuredContent":body["arguments"]})
            }
        })
    }
}

/// Runs `test` against a transport connected to a fake daemon holding the
/// run scope (with the given parent state) and the head scope, with a live
/// head route for the run's agent.
fn with_daemon(parent_state: &str, test: impl FnOnce(Arc<SubcTransport>, Arc<Mutex<Daemon>>)) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use subc_transport::connection_file::{ConnectionInfo, Endpoint, SCHEMA_VERSION, write_atomic};
    use subc_transport::{authenticate_server, read_frame, write_frame};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "basal-run-scope-wire-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("subc-connection.json");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
        .unwrap();
    let info = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        }],
        key: vec![7; 32],
        daemon_id: [9; 16],
        pid: std::process::id(),
        daemon_ver: "run-scope-test".into(),
    };
    write_atomic(&file, &info).unwrap();
    let state = Arc::new(Mutex::new(Daemon::default()));
    {
        let mut daemon = state.lock().unwrap();
        daemon
            .scopes
            .insert((RUN.into(), 19), run_stamp(parent_state));
        daemon
            .scopes
            .insert(("agent-scope".into(), 7), head_stamp());
        daemon.head_routes.insert(AGENT.into());
    }
    let shared = state.clone();
    let server = runtime.spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        authenticate_server(
            &mut stream,
            &info.key,
            &info.daemon_id,
            "run-scope-test",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        let mut next_channel = 10u16;
        while let Some(frame) = read_frame(&mut stream).await.unwrap() {
            if frame.header.ty != FrameType::Request {
                continue;
            }
            let body: Value = serde_json::from_slice(&frame.body).unwrap();
            let reply = if frame.header.channel == 0 {
                if body["op"] == "route.open" {
                    let bound = shared.lock().unwrap().bind(&body);
                    bound.map(|route| {
                        next_channel += 1;
                        shared.lock().unwrap().routes.insert(next_channel, route);
                        json!({"op":"route.open","route_channel":next_channel,"route_epoch":1})
                    })
                } else {
                    // route.close and other control requests.
                    Ok(json!({"result":null}))
                }
            } else {
                shared.lock().unwrap().answer(frame.header.channel, &body)
            };
            let (ty, body) = match reply {
                Ok(body) => (FrameType::Response, body),
                Err(error) => (FrameType::Error, error),
            };
            let response = Frame::build(
                ty,
                Flags::new(false, Priority::Passive, false),
                frame.header.channel,
                frame.header.epoch,
                frame.header.corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap();
            write_frame(&mut stream, &response).await.unwrap();
        }
    });
    let transport =
        SubcTransport::connect_with_file(Duration::from_secs(10), Some(&file), None).unwrap();
    test(transport.clone(), state);
    server.abort();
    drop(transport);
    drop(runtime);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A daemon catalog listing cerebellum's `recall` and AFT's `read`, both
/// from providers declaring call modes.
fn daemon_catalog() -> Value {
    let module = |id: &str, tool: &str| {
        json!({"module_id":id,"capabilities":{"provides":[cortexkit_role_tool_provider::CALL_MODES_CAPABILITY]},
        "roles":[{"role":"tool_provider","tools":[{"name":tool,"execution_mode":"pure",
            "schema":{"type":"object"}}],"identity_scope":[],"concurrency":"serial",
            "emits_push":false,"sub_supervises":false}]})
    };
    json!({"modules":[module("cerebellum","recall"),module("aft","read")]})
}

fn entries() -> Vec<crate::run_scope::CatalogEntry> {
    [("cerebellum", "recall"), ("aft", "read")]
        .into_iter()
        .map(|(module, op)| crate::run_scope::CatalogEntry {
            tool: format!("{module}_{op}"),
            module: module.into(),
            op: op.into(),
        })
        .collect()
}

fn read_catalogs(
    transport: &SubcTransport,
    scope: &ScopeSelector,
    preset: Option<&str>,
) -> crate::run_scope::ResolvedCatalog {
    let identity =
        crate::run_scope::catalog_read_identity(std::path::Path::new("/agent/project"), RUN);
    let read = crate::run_scope::CatalogRead {
        scope,
        identity: &identity,
        preset,
    };
    crate::run_scope::catalog(transport, &read, &daemon_catalog(), &entries())
        .expect("the catalog resolves")
}

fn received(state: &Mutex<Daemon>) -> Vec<Received> {
    state.lock().unwrap().received.clone()
}

#[test]
fn admission_reads_and_run_calls_reach_each_provider_as_codemode() {
    with_daemon("linked", |transport, state| {
        let resolved = read_catalogs(&transport, &run_selector(), None);
        assert!(resolved.gaps.is_empty(), "{:?}", resolved.gaps);
        let names: Vec<_> = resolved
            .catalog
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].clone())
            .collect();
        assert_eq!(names, [json!("cerebellum_recall"), json!("aft_read")]);
        // The program's calls: one tool call to each provider, a custody
        // read on cerebellum's route and a model call to Broca, all on the
        // run's registered scope.
        let flow = format!("codemode:{RUN}");
        transport.configure_flow(
            &flow,
            Some(crate::flow_scope::RegisteredScope {
                selector: crate::flow_scope::FlowScope {
                    owner: core(),
                    scope_ref: RUN.into(),
                    epoch: 19,
                },
                targets: ["cerebellum", "aft", "broca"].map(String::from).into(),
            }),
        );
        let options = RunToolOptions {
            identity: BindIdentity::new("/agent/project", "opencode", "head-session"),
            reply_timeout: Duration::from_secs(10),
            preset: None,
        };
        for (module, op) in [("cerebellum", "recall"), ("aft", "read")] {
            let reply = transport
                .tool_for_run(
                    &flow,
                    module,
                    op,
                    json!({"q":module}),
                    &format!("bk1-{module}"),
                    &options,
                )
                .unwrap_or_else(|error| panic!("{module}.{op}: {error:?}"));
            assert_eq!(reply["structuredContent"], json!({"q":module}));
        }
        transport
            .role_for_run(&flow, "cerebellum", "late_results", json!({}), &options)
            .expect("late results read");
        transport
            .management_for_model(
                &flow,
                options.identity.clone(),
                "broca",
                "session.send",
                br#"{"send_id":"s1"}"#,
            )
            .expect("model call");
        let received = received(&state);
        let seen: Vec<_> = received
            .iter()
            .map(|r| (r.module.as_str(), r.name.as_str(), r.call_key))
            .collect();
        assert_eq!(
            seen,
            [
                ("cerebellum", "role.describe", false),
                ("cerebellum", "tool.catalog", false),
                ("aft", "role.describe", false),
                ("aft", "tool.catalog", false),
                ("cerebellum", "recall", true),
                ("aft", "read", true),
                ("cerebellum", "late_results", false),
                ("broca", "session.send", false),
            ]
        );
        for request in &received {
            assert_eq!(request.call_mode, CallMode::Codemode, "{request:?}");
            assert_eq!(request.preset, None, "{request:?}");
            assert_eq!(request.catalog_preset, None, "{request:?}");
        }
    });
}

#[test]
fn core_selected_preset_reaches_catalog_reads_tool_calls_and_custody() {
    with_daemon("linked", |transport, state| {
        let preset = "worker";
        state.lock().unwrap().required_preset = Some(preset.into());
        let resolved = read_catalogs(&transport, &run_selector(), Some(preset));
        assert!(resolved.gaps.is_empty(), "{:?}", resolved.gaps);

        let flow = format!("codemode:{RUN}");
        transport.configure_flow(
            &flow,
            Some(crate::flow_scope::RegisteredScope {
                selector: crate::flow_scope::FlowScope {
                    owner: core(),
                    scope_ref: RUN.into(),
                    epoch: 19,
                },
                targets: ["aft", "cerebellum"].map(String::from).into(),
            }),
        );
        let options = RunToolOptions {
            identity: BindIdentity::new("/agent/project", "opencode", "head-session"),
            reply_timeout: Duration::from_secs(10),
            preset: Some(preset.into()),
        };
        transport
            .tool_for_run(
                &flow,
                "aft",
                "read",
                json!({"q":"hello"}),
                "bk1-aft",
                &options,
            )
            .expect("the fake AFT accepts the core-selected preset");
        transport
            .role_for_run(&flow, "cerebellum", "late_results", json!({}), &options)
            .expect("custody carries the run preset");

        let received = received(&state);
        assert_eq!(received.len(), 6);
        for request in &received {
            assert_eq!(request.preset.as_deref(), Some(preset), "{request:?}");
            if request.name == "tool.catalog" {
                assert_eq!(
                    request.catalog_preset.as_deref(),
                    Some(preset),
                    "{request:?}"
                );
            } else {
                assert_eq!(request.catalog_preset, None, "{request:?}");
            }
        }
    });
}

#[test]
fn the_fake_cerebellum_refuses_reads_off_the_run_scope_or_without_a_live_parent() {
    // Read on the head scope: the bind is classic, so cerebellum refuses it
    // permanently, while AFT still answers.
    with_daemon("linked", |transport, state| {
        let resolved = read_catalogs(&transport, &head_selector(), None);
        assert_eq!(resolved.gaps.len(), 1, "{:?}", resolved.gaps);
        let gap = &resolved.gaps[0];
        assert_eq!(gap.module, "cerebellum");
        assert!(gap.problem.contains("principal_denied"), "{}", gap.problem);
        assert!(!gap.message().contains("retrying the run may succeed"));
        assert!(
            received(&state)
                .iter()
                .all(|r| r.module == "aft" && r.call_mode == CallMode::Classic)
        );
    });
    // On the run scope, but the run's parent is not linked.
    with_daemon("pending", |transport, _| {
        let resolved = read_catalogs(&transport, &run_selector(), None);
        assert_eq!(resolved.gaps.len(), 1, "{:?}", resolved.gaps);
        assert_eq!(resolved.gaps[0].module, "cerebellum");
        assert!(
            resolved.gaps[0].problem.contains("run_parent_unavailable"),
            "{}",
            resolved.gaps[0].problem
        );
        // A head that calls cerebellum makes the next run's bind succeed.
        assert!(
            resolved.gaps[0]
                .message()
                .contains("retrying the run may succeed")
        );
    });
}

#[test]
fn the_fake_cerebellum_refuses_envelope_keys_on_role_ops_and_unknown_catalog_members() {
    with_daemon("linked", |transport, _| {
        let flow = format!("codemode:{RUN}");
        transport.configure_flow(
            &flow,
            Some(crate::flow_scope::RegisteredScope {
                selector: crate::flow_scope::FlowScope {
                    owner: core(),
                    scope_ref: RUN.into(),
                    epoch: 19,
                },
                targets: ["cerebellum"].map(String::from).into(),
            }),
        );
        let options = RunToolOptions {
            identity: BindIdentity::new("/agent/project", "opencode", "head-session"),
            reply_timeout: Duration::from_secs(10),
            preset: None,
        };
        // tool_for_run always sends an envelope call key.
        let keyed = transport.tool_for_run(
            &flow,
            "cerebellum",
            "tool.catalog",
            json!({"params":{}}),
            "bk1-x",
            &options,
        );
        assert!(
            matches!(&keyed, Err(WireError::Refused { code, .. }) if code == "invalid_request"),
            "{keyed:?}"
        );
        let identity =
            crate::run_scope::catalog_read_identity(std::path::Path::new("/agent/project"), RUN);
        let unknown = transport.role_on_run_scope(
            &run_selector(),
            &identity,
            "cerebellum",
            "tool.catalog",
            json!({"params":{},"call_mode":"codemode"}),
            None,
        );
        assert!(
            matches!(&unknown, Err(WireError::Refused { code, message }) if code == "invalid_request" && message.contains("unknown catalog member")),
            "{unknown:?}"
        );
    });
}
