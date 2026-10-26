//! Wire fixtures verify request and reply formats without connecting to a daemon.
#[path = "hosts/board_sink.rs"]
mod board_sink;
mod common;
#[path = "hosts/digest_withdraw.rs"]
mod digest_withdraw;
#[path = "hosts/dry_run_capabilities.rs"]
mod dry_run_capabilities;
#[path = "hosts/wait_resolve.rs"]
mod wait_resolve;
mod wire;
use std::sync::{Arc, Mutex};

use basal_core::{NoHooks, RunState};
use basal_host::core_consent::CoreConsent;
use basal_host::core_host::{CoreHost, IntentContext, intent, validate_reply};
use basal_host::routing::{ModuleOpsHost, RoutingHost};
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{WireError, map_error, tool_body};
use basal_host::{CallRequest, Consent, DecisionEvent, DecisionSink, Dispatched, Host, SinkError};
use basal_module::caller::Caller;
use basal_module::module::Hosts;
use basal_module::unconfigured::UnconfiguredHost;
use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use serde_json::{Value, json};
use wire::Fake;

fn call(p: Primitive, args: Value) -> CallRequest {
    CallRequest {
        flow_version: None,
        flow_id: "flow".into(),
        run_id: "run".into(),
        position: 7,
        kind: CallKind::Primitive(p),
        args: JsonText::new(args.to_string()).unwrap(),
        idempotency_key: "key-7".into(),
        attempt: 1,
    }
}

fn set_core_digest_replace_key_capability(fake: &Fake, supported: bool) {
    set_core_capability(
        fake,
        basal_host::core_host::FLOW_DIGEST_REPLACE_KEY_V1,
        supported,
    );
}

fn set_core_capability(fake: &Fake, capability: &str, supported: bool) {
    let mut catalog = fake.catalog.lock().unwrap();
    let modules = catalog["modules"].as_array_mut().unwrap();
    let core = modules
        .iter_mut()
        .find(|module| module["module_id"] == "prefrontal-core");
    if let Some(core) = core {
        let capabilities = core["capabilities"]["provides"]
            .as_array_mut()
            .expect("core provides is an array");
        capabilities.retain(|provided| provided != capability);
        if supported {
            capabilities.push(json!(capability));
        }
    } else {
        modules.push(json!({
            "module_id": "prefrontal-core",
            "capabilities": {"provides": if supported {
                vec![capability]
            } else {
                Vec::new()
            }}
        }));
    }
}

#[test]
fn core_requests_match_c3_c4_field_for_field() {
    let f = Fake::new();
    let host = CoreHost::new(f.clone());
    let digest = intent(
        Primitive::SinkDigest,
        &json!({"agent":"SYNAPSE","action":"piggyback","item":{"title":"New inference post","body":"Plain text body.","data":{"posts":2},"links":[{"kind":"url","url":"https://x.com/someone/status/1"},{"kind":"work","id":"wi_0123"}]}}),
        IntentContext {
            flow_id: "synapse-xcom-inference-news",
            version: 3,
            run_id: "run_01JB4M5N6P7Q8R9S",
            position: 7,
            due_at: 1790000000000,
            created_at: 1790000000000,
        },
    );
    let expected = json!({"flow_id":"synapse-xcom-inference-news","flow_version":3,"run_id":"run_01JB4M5N6P7Q8R9S","call_position":7,"agent":"SYNAPSE","action":"piggyback","due_at":1790000000000i64,"created_at":1790000000000i64,"item":{"title":"New inference post","body":"Plain text body.","data":{"posts":2},"links":[{"kind":"url","url":"https://x.com/someone/status/1"},{"kind":"work","id":"wi_0123"}]},"claim":null});
    host.dispatch(&call(Primitive::SinkDigest, digest)).unwrap();
    assert_eq!(
        f.calls("sink.digest")[0],
        json!({"method":"sink.digest","params":expected})
    );
    let status = intent(
        Primitive::SinkStatus,
        &json!({"agent":"SYNAPSE","value":"x.com: 2 new"}),
        IntentContext {
            flow_id: "synapse-xcom-inference-news",
            version: 3,
            run_id: "run",
            position: 7,
            due_at: 0,
            created_at: 1790000000000,
        },
    );
    host.dispatch(&call(Primitive::SinkStatus, status)).unwrap();
    assert_eq!(
        f.calls("sink.status")[0]["params"],
        json!({"flow_id":"synapse-xcom-inference-news","flow_version":3,"agent":"SYNAPSE","text":"x.com: 2 new","ttl_ms":1800000,"revision":1790000000000i64})
    );
    let facts = intent(
        Primitive::Facts,
        &json!({"agent":"BASAL","options":{"fields":["identity","residence","activity","attention"],"include":[],"max_age_ms":null}}),
        IntentContext {
            flow_id: "dark-wake-basal",
            version: 1,
            run_id: "run",
            position: 7,
            due_at: 0,
            created_at: 0,
        },
    );
    host.dispatch(&call(Primitive::Facts, facts)).unwrap();
    assert_eq!(
        f.calls("agent.facts")[0]["params"],
        json!({"flow_id":"dark-wake-basal","flow_version":1,"agent":"BASAL","fields":["identity","residence","activity","attention"],"include":[],"max_age_ms":null})
    );
}
#[test]
fn closed_sink_dispositions_and_unknown_facts_survive() {
    for d in ["stored", "claim_in_fallback", "episode_ended"] {
        let v = json!({"disposition":d,"fire_id":if d=="stored" {json!("wf_1")} else {Value::Null},"replayed":true,"future":42});
        assert_eq!(
            validate_reply(Primitive::SinkDigest, v.clone(), false).unwrap(),
            v
        );
    }
    for v in [
        json!({"disposition":"published","accepted_revision":1,"segment":"flow:f","scope":"session:s","replayed":false}),
        json!({"disposition":"no_live_session","accepted_revision":null,"segment":null,"scope":null,"replayed":true}),
    ] {
        assert_eq!(
            validate_reply(Primitive::SinkStatus, v.clone(), false).unwrap(),
            v
        );
    }
    for p in [Primitive::SinkDigest, Primitive::SinkStatus] {
        assert!(matches!(
            validate_reply(
                p,
                json!({"disposition":"new_value","fire_id":"wf_1","replayed":false,"accepted_revision":1,"segment":"flow:f","scope":"session:s"}),
                false,
            ),
            Err(WireError::Unreadable(_))
        ));
        assert!(validate_reply(p, json!({}), false).is_err());
    }
    assert!(validate_reply(Primitive::SinkStatus,json!({"disposition":"published","accepted_revision":null,"segment":"flow:f","scope":null}), false).is_err());
    let f = Fake::new();
    let host = CoreHost::new(f);
    let Dispatched::Completed(outcome) = host.dispatch(&call(Primitive::Facts, json!({}))).unwrap()
    else {
        panic!("completed")
    };
    let value: Value = serde_json::from_str(outcome.value.as_str()).unwrap();
    assert_eq!(value["activity"]["state"]["status"], "unknown");
    assert!(value["activity"]["state"]["value"].is_null());
}
#[test]
fn every_core_refusal_retains_its_code() {
    let f = Fake::new();
    let host = CoreHost::new(f.clone());
    for (p, codes) in [
        (
            Primitive::SinkDigest,
            vec![
                "sink_principal_refused",
                "sink_flow_not_installed",
                "sink_instance_expired",
                "sink_target_not_granted",
                "sink_action_over_cap",
                "sink_item_invalid",
                "sink_input_conflict",
                "sink_claim_unsupported",
            ],
        ),
        (
            Primitive::SinkStatus,
            vec![
                "sink_principal_refused",
                "sink_flow_not_installed",
                "sink_status_target_not_granted",
                "sink_item_invalid",
                "sink_input_conflict",
            ],
        ),
        (Primitive::Facts, vec!["facts_target_not_granted"]),
    ] {
        let op = match p {
            Primitive::SinkDigest => "sink.digest",
            Primitive::SinkStatus => "sink.status",
            _ => "agent.facts",
        };
        for code in codes {
            f.enqueue(
                op,
                vec![Err(WireError::Refused {
                    code: code.into(),
                    message: "refused".into(),
                })],
            );
            let Dispatched::Completed(r) = host.dispatch(&call(p, json!({}))).unwrap() else {
                panic!("completed")
            };
            assert_eq!(r.settlement, Settlement::Rejected);
            let v: Value = serde_json::from_str(r.value.as_str()).unwrap();
            assert_eq!(v["code"], code);
        }
    }
}
#[test]
fn transport_certainty_and_tool_key_are_explicit() {
    use subc_client_rs::consumer::CallError;
    assert!(matches!(
        map_error(CallError::NotSent(Box::new(std::io::Error::other(
            "never wrote"
        )))),
        WireError::NeverSent(_)
    ));
    assert!(matches!(
        map_error(CallError::OutcomeUnknown(Box::new(std::io::Error::other(
            "lost reply"
        )))),
        WireError::Unknown(_)
    ));
    let e = serde_json::from_value(json!({"code":"provider_denied","message":"no"})).unwrap();
    assert_eq!(
        map_error(CallError::Module(e)),
        WireError::Refused {
            code: "provider_denied".into(),
            message: "no".into()
        }
    );
    assert_eq!(
        tool_body("send", json!({"n":1}), "key-7").unwrap(),
        json!({"name":"send","arguments":{"n":1},"call_key":"key-7"})
    );
}
fn manifest() -> Value {
    json!({"id":"host-flow","version":1,"purpose":"Report facts to Synapse","trigger":{"schedule":{"interval":"1h"}},"sinks":[{"agent":"SYNAPSE","digest_max":"piggyback"}],"status":["SYNAPSE"],"facts":{"targets":["SYNAPSE"],"text":false},"ops":[{"module":"mock","op":"echo"},{"module":"mock","op":"post"},{"module":"mock","op":"send"}]})
}
fn hosts(f: Arc<Fake>, consent: Arc<CoreConsent>) -> Hosts {
    let cat = Arc::new(SubcCatalog::new(f.clone()));
    Hosts {
        transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
        host: Arc::new(RoutingHost::new(
            Arc::new(ModuleOpsHost::new(f.clone(), cat.clone())),
            Arc::new(CoreHost::new(f)),
            Arc::new(UnconfiguredHost::new()),
        )),
        catalog: cat,
        consent,
        hooks: Arc::new(NoHooks),
    }
}
fn fixture(f: Arc<Fake>, consent: Arc<CoreConsent>, tag: &str) -> common::Fixture {
    fixture_with_options(f, consent, tag, common::Options::default())
}
fn worker_fixture(f: Arc<Fake>, consent: Arc<CoreConsent>, tag: &str) -> common::Fixture {
    fixture_with_options(f, consent, tag, common::Options::with_worker())
}
fn fixture_with_options(
    f: Arc<Fake>,
    consent: Arc<CoreConsent>,
    tag: &str,
    options: common::Options,
) -> common::Fixture {
    common::fixture(
        tag,
        common::Options {
            hosts: Some(hosts(f, consent)),
            ..options
        },
    )
}
fn approve(f: &Fake, module: &basal_module::module::Module) {
    let flow = f.calls("flow.install_request")[0]["params"]["flow_install"].clone();
    f.enqueue(
        "flow.install_status",
        vec![Ok(
            json!({"state":"active","code_hash":flow["code_hash"],"approval":{"kind":"card"}}),
        )],
    );
    module
        .handle(
            &Caller::Core,
            "flow.install_decided",
            json!({"flow_id":flow["flow_id"],"version":flow["version"],"attempt":1}),
        )
        .unwrap();
}
#[test]
fn catalog_install_refuses_missing_ops_and_events_and_marks_mutations() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    // Acknowledgement must not substitute for the event's published declaration.
    let capabilities = Arc::new(basal_host::subc_catalog::DaemonCapabilities::default());
    capabilities.observe_hello_ack(
        &serde_json::from_value(json!({
            "negotiated_ver": subc_protocol::PROTOCOL_VERSION,
            "subc_ops": [],
            "subc_capabilities": [subc_protocol::manifest::CAP_CATALOG_EVENTS_V1],
            "storage": null,
            "machine_id": null,
        }))
        .unwrap(),
    );
    let mut configured_hosts = hosts(fake.clone(), consent);
    configured_hosts.catalog =
        Arc::new(SubcCatalog::new(fake.clone()).with_daemon_capabilities(capabilities));
    let f = common::fixture(
        "catalog-real",
        common::Options {
            hosts: Some(configured_hosts),
            ..common::Options::default()
        },
    );
    let mut m = manifest();
    m["ops"] = json!([{"module":"mock","op":"absent"}]);
    let err = f
        .module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({"script":"return 1;","manifest":m.to_string()}),
        )
        .unwrap_err();
    assert!(err.message.contains("absent"), "{err:?}");
    m = manifest();
    m["trigger"] = json!({"events":[{"module":"mock","name":"changed","version":1}]});
    let err = f
        .module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({"script":"return 1;","manifest":m.to_string()}),
        )
        .unwrap_err();
    assert_eq!(err.code, "event_not_declared");
    assert!(err.message.contains("published declaration"), "{err:?}");
    let cat = SubcCatalog::new(fake.clone());
    use basal_host::{Catalog, OpKind};
    assert_eq!(cat.op("mock", "post").unwrap().kind, Some(OpKind::Mutate));
    assert_eq!(cat.op("mock", "read").unwrap().kind, Some(OpKind::Query));
    assert!(!cat.op("mock", "shell").unwrap().shell_capable);
    assert!(cat.agent_known("ag_synapse").unwrap());
    assert!(cat.agent_known("SYNAPSE").unwrap());
    assert!(!cat.agent_known("unknown").unwrap());
    common::install(
        &f,
        &agent_on_scope("ag_synapse", "scope-live"),
        "return 1;",
        &manifest(),
    );
    let cards = f.module.rt.cards("host-flow").unwrap();
    let fields: Value = serde_json::from_str(&cards[0].card).unwrap();
    assert_eq!(fields["ops"][1]["kind"], "mutate");
}
/// The caller basal decides for a route under core's owner-authorized
/// session scope `scope_ref`, naming `agent`, as the daemon stamps it.
fn agent_on_scope(agent: &str, scope_ref: &str) -> Caller {
    use subc_protocol::Principal;
    use subc_protocol::scope::{ScopeAttributes, ScopeKind, ScopeStamp};
    let stamp = ScopeStamp::new(
        Principal::Reserved {
            module_id: basal_module::caller::CORE_MODULE.into(),
        },
        scope_ref,
        1,
        ScopeKind::Head,
    )
    .with_attributes(ScopeAttributes::new().with_agent_id(Some(agent.into())))
    .with_owner_authorized(true);
    basal_module::caller::from_route(Some(&Principal::Direct), Some(&stamp))
}

#[test]
fn install_card_is_byte_exact_and_scope_comes_from_the_route_stamp() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = fixture(fake.clone(), consent, "card-real");
    let script = "return 1;\n";
    let m = format!("  {}\n", manifest());
    let stamped = "5e1f0c3a9b7d4e2f8a6c1b3d5f7e9a0c";
    let caller = agent_on_scope("ag_synapse", stamped);
    f.module
        .handle(
            &caller,
            "flow.install",
            json!({"script":script,"manifest":m}),
        )
        .unwrap();
    let sent = fake.calls("flow.install_request")[0]["params"].clone();
    let hash = basal_module::card::code_hash_hex(&basal_core::ids::code_hash(script, &m));
    assert_eq!(sent["args_digest"], hash);
    assert_eq!(sent["flow_install"]["code_hash"], hash);
    assert_eq!(sent["flow_install"]["manifest_json"], m);
    assert_eq!(sent["flow_install"]["script"], script);
    // The author is the scope ref the daemon stamped on the route, and the
    // request names no session: core takes both agent and session from its
    // own record of that scope.
    assert_eq!(sent["flow_install"]["author"], json!({ "scope": stamped }));
    assert!(sent.get("session_ref").is_none(), "{sent:#}");
    assert_eq!(sent["facts"], json!([]));
    assert_eq!(sent["dedup_key"], "flow_install:host-flow:1");
    assert_eq!(
        sent["options"],
        json!([{"id":"approve","label":"Approve","effect":"grant"},{"id":"decline","label":"Decline","effect":"decline"}])
    );
    for key in [
        "purpose",
        "trigger",
        "ops",
        "caps",
        "sinks",
        "status_targets",
        "facts",
    ] {
        assert!(sent["flow_install"].get(key).is_none(), "{key}");
    }
    let mut m = manifest();
    m["id"] = json!("operator-flow");
    common::install(&f, &Caller::Operator, script, &m);
    let operator = fake.calls("flow.install_request")[1]["params"].clone();
    assert!(operator.get("session_ref").is_none());
    assert_eq!(operator["flow_install"]["author"], json!({"operator":true}));
    assert_eq!(
        operator["flow_install"]["token_usage"],
        json!({"window":"1d","fresh_input":0,"cache_write":0,"output":0,"cache_read":0})
    );
}

#[test]
fn board_read_install_request_preserves_manifest_bytes_and_hash() {
    let fake = Fake::new();
    fake.catalog.lock().unwrap()["modules"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "module_id":"prefrontal-core", "roles":[{"role":"management_surface",
                "operations":[{"name":"board.read","kind":"query"},{"name":"work.unsettled","kind":"query"}],
                "config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}],
            "control_ops":[]
        }));
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = fixture(fake.clone(), consent, "board-read-bytes");
    let script = "return 1;\n";
    let manifest = include_str!("../../../flows/board-hygiene/manifest.json");
    let reply = f
        .module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({
                "script":script,"manifest":manifest
            }),
        )
        .unwrap();
    assert_eq!(reply["state"], "pending");
    let calls = fake.calls("flow.install_request");
    assert_eq!(calls.len(), 1);
    let install = &calls[0]["params"]["flow_install"];
    assert_eq!(install["manifest_json"], manifest);
    let parsed: Value = serde_json::from_str(install["manifest_json"].as_str().unwrap()).unwrap();
    assert_eq!(parsed["board_read"], json!(["$audience"]));
    let expected = basal_core::ids::hex(&basal_core::ids::code_hash(script, manifest));
    assert_eq!(install["code_hash"], expected);
    assert_eq!(reply["code_hash"], expected);
    let without = manifest.replace("  \"board_read\": [\"$audience\"],\n", "");
    assert_ne!(without, manifest);
    assert_ne!(
        expected,
        basal_core::ids::hex(&basal_core::ids::code_hash(script, &without))
    );
}

#[test]
fn install_request_carries_dry_run_text_only_when_core_lists_the_capability() {
    use basal_host::InstallCard;
    use basal_host::core_consent::{FLOW_INSTALL_DRY_RUN_TEXT_V1, request};
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent, "dry-run-text");
    let script = "return 1;\n";
    // The request an older core receives: built from the card as recorded.
    let unchanged = |flow: &str| {
        let card = f.module.rt.cards(flow).unwrap().remove(0);
        let fields: Value = serde_json::from_str(&card.card).unwrap();
        let sent = request(&InstallCard {
            card_id: card.card_id,
            flow_id: card.flow_id,
            version: card.version,
            fields: fields.clone(),
        })
        .unwrap();
        (sent, fields)
    };

    set_core_capability(&fake, FLOW_INSTALL_DRY_RUN_TEXT_V1, false);
    common::install(&f, &Caller::Operator, script, &manifest());
    let without = fake.calls("flow.install_request")[0]["params"].clone();
    let (expected, fields) = unchanged("host-flow");
    assert_eq!(without, expected, "exactly the request an older core takes");
    assert!(without["flow_install"].get("dry_run_text").is_none());
    // The card holds the text either way; only the request is gated.
    assert!(fields["dry_run_text"]["summary"].is_string(), "{fields:#}");

    set_core_capability(&fake, FLOW_INSTALL_DRY_RUN_TEXT_V1, true);
    let mut m = manifest();
    m["id"] = json!("text-flow");
    common::install(&f, &Caller::Operator, script, &m);
    let mut with = fake.calls("flow.install_request")[1]["params"].clone();
    let (expected, fields) = unchanged("text-flow");
    let text = with["flow_install"]
        .as_object_mut()
        .unwrap()
        .remove("dry_run_text")
        .expect("dry_run_text is sent when core lists the capability");
    assert_eq!(text, fields["dry_run_text"]);
    assert_eq!(
        text["summary"],
        "In the last 24 hours it would have run 24 times, every hour; the dry run tried the newest 10."
    );
    assert_eq!(text["would"], json!([]));
    assert_eq!(text["problems"], json!([]));
    // Apart from the text, the request is unchanged, raw summary included.
    assert_eq!(with, expected);
    assert!(with["flow_install"]["dry_run_summary"]["runs"].is_array());
}

#[test]
fn card_author_is_scope_operator_or_local_and_nothing_else() {
    use basal_host::core_consent::request;
    use basal_host::{ConsentError, InstallCard};
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = fixture(fake.clone(), consent, "card-local");
    let script = "return 1;\n";

    // A local caller's card reaches core as {"local": true} with no session,
    // and core's fake accepts it because the flow declares a digest sink.
    common::install(&f, &Caller::Local, script, &manifest());
    let calls = fake.calls("flow.install_request");
    assert_eq!(calls.len(), 1);
    let local = &calls[0]["params"];
    assert_eq!(local["flow_install"]["author"], json!({ "local": true }));
    assert!(local.get("session_ref").is_none(), "{local:#}");

    // Without a digest sink basal refuses it itself, before core sees it.
    let mut bare = manifest();
    bare["id"] = json!("bare-flow");
    bare["sinks"] = json!([]);
    let r = f.module.handle(
        &Caller::Local,
        "flow.install",
        json!({ "script": script, "manifest": bare.to_string() }),
    );
    assert_eq!(
        r.map_err(|e| e.code),
        Err("local_install_needs_digest_sink".to_owned())
    );
    assert_eq!(fake.calls("flow.install_request").len(), 1, "nothing sent");

    // The consent host sends only the three author forms core accepts
    // (operator, scope, local), and never a session_ref, even when the
    // card's fields hold one. It refuses any other author form before
    // sending anything.
    let card = |author: Value| InstallCard {
        card_id: "card:f:v1:0123456789abcdef".into(),
        flow_id: "f".into(),
        version: 1,
        fields: json!({"purpose":"Purpose","wire_author":author,"session_ref":"ses-from-a-bind","code_hash":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","code":{"script":"return 1;","manifest":"{\"id\":\"f\",\"version\":1}"},"placement":"machine:local","warnings":[],"dry_run_summary":{},"token_cap":null,"token_window":null}),
    };
    for author in [
        json!({ "local": true }),
        json!({ "operator": true }),
        json!({ "scope": "5e1f0c3a" }),
    ] {
        let sent = request(&card(author.clone())).unwrap();
        assert_eq!(sent["flow_install"]["author"], author);
        assert!(sent.get("session_ref").is_none(), "{sent:#}");
    }
    for author in [
        // Core no longer accepts an agent named directly by its id, which it
        // could only take on basal's word.
        json!({ "agent": "SYNAPSE" }),
        json!({ "scope": "" }),
        json!({ "scope": 7 }),
        json!({ "scope": "5e1f0c3a", "agent": "SYNAPSE" }),
        json!({ "local": false }),
        json!({ "local": "yes" }),
        json!({ "operator": false }),
        json!({ "operator": true, "local": true }),
        json!({ "unverified": true }),
        json!({}),
        json!("local"),
    ] {
        let r = request(&card(author.clone()));
        assert!(
            matches!(r, Err(ConsentError::Refused(_))),
            "{author}: {r:?}"
        );
    }
}

#[test]
fn core_refusing_the_author_scope_is_a_clear_install_error() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = fixture(fake.clone(), consent, "card-scope-refused");
    let script = "return 1;\n";
    let install_as = |caller: &Caller| {
        f.module.handle(
            caller,
            "flow.install",
            json!({ "script": script, "manifest": manifest().to_string() }),
        )
    };

    // Core's records: a scope that has ended, and one with no agent.
    for (scope_ref, agent, live) in [
        ("scope-gone", Some("ag_synapse"), false),
        ("scope-agentless", None, true),
    ] {
        fake.scopes.lock().unwrap().insert(
            scope_ref.into(),
            wire::CoreScope {
                agent: agent.map(str::to_owned),
                live,
            },
        );
    }
    // Core answers with a refusal of the scope: the install fails with core's
    // refusal code as its error code, not as a consent outage or a lost
    // reply. The version stays installed.
    for (scope_ref, code) in [
        ("scope-gone", "flow_install_scope_ended"),
        ("scope-nobody-minted", "flow_install_scope_unknown"),
        ("scope-agentless", "flow_install_scope_unknown"),
    ] {
        let r = install_as(&agent_on_scope("ag_synapse", scope_ref));
        let e = r.expect_err("refused");
        assert_eq!(e.code, code, "{scope_ref}: {e:?}");
        assert!(e.message.contains("live agent session"), "{e:?}");
    }
    assert_eq!(f.module.rt.cards("host-flow").unwrap().len(), 1);

    // Installing again from a route under a live scope sends the existing
    // card with that live scope as its author, rather than the ended scope
    // recorded when the card was first created.
    let reply = install_as(&agent_on_scope("ag_synapse", "scope-live")).expect("raised");
    assert_eq!(reply["state"], "pending");
    let calls = fake.calls("flow.install_request");
    let last = &calls.last().expect("sent")["params"];
    assert_eq!(
        last["flow_install"]["author"],
        json!({ "scope": "scope-live" })
    );
}

#[derive(Default)]
struct Decisions {
    events: Mutex<Vec<DecisionEvent>>,
    answers: Mutex<Vec<basal_host::DecisionAnswer>>,
    fail: Mutex<bool>,
}
fn cingulate() -> basal_host::DecisionPath {
    basal_host::DecisionPath::Consent("cingulate".into())
}
impl DecisionSink for Decisions {
    fn decision_paths(&self) -> Result<Vec<basal_host::DecisionPath>, SinkError> {
        Ok(vec![cingulate()])
    }
    fn has_open_cards_on(&self, _: &basal_host::DecisionPath) -> Result<bool, SinkError> {
        Ok(true)
    }
    fn decide(&self, d: &DecisionEvent) -> Result<(), SinkError> {
        if *self.fail.lock().unwrap() {
            return Err(SinkError("disk cut".into()));
        }
        self.events.lock().unwrap().push(d.clone());
        Ok(())
    }
    fn answer_on(
        &self,
        path: &basal_host::DecisionPath,
        answer: &basal_host::DecisionAnswer,
    ) -> Result<(), SinkError> {
        assert_eq!(*path, cingulate());
        if *self.fail.lock().unwrap() {
            return Err(SinkError("disk cut".into()));
        }
        self.answers.lock().unwrap().push(answer.clone());
        Ok(())
    }
}
#[test]
fn decisions_ack_only_after_sink_commit_and_survive_restart() {
    let fake = Fake::new();
    let sink = Arc::new(Decisions::default());
    *sink.fail.lock().unwrap() = true;
    let record = json!({"elicitation_id":"el-decision","state":"answered","answered_choice_id":"reenable","flow_decision":{"decision":"reenable","flow_id":"f","version":1}});
    let page = json!({"records":[record],"cursor":9});
    fake.enqueue("consent.answers", vec![Ok(page.clone()), Ok(page)]);
    let consent = CoreConsent::new(fake.clone());
    consent.attach(sink.clone());
    assert!(consent.poll_once().is_err());
    assert!(fake.calls("consent.ack").is_empty());
    drop(consent);
    *sink.fail.lock().unwrap() = false;
    let restarted = CoreConsent::new(fake.clone());
    restarted.attach(sink.clone());
    restarted.poll_once().unwrap();
    assert_eq!(
        sink.answers.lock().unwrap()[0].choice.as_deref(),
        Some("reenable")
    );
    assert!(sink.events.lock().unwrap().is_empty());
    assert_eq!(
        fake.calls_to("cingulate", "consent.ack")[0]["params"],
        json!({"cursor":9})
    );
}
#[test]
fn routing_host_runs_install_decision_facts_ops_and_sinks_end_to_end() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent.clone(), "routing-e2e");
    common::install(
        &f,
        &agent_on_scope("ag_synapse", "scope-live"),
        "const a=await facts('SYNAPSE'); const b=await ops.call('mock','echo',{n:2}); await sink.digest('SYNAPSE',{title:'facts',body:'unknown'},'piggyback'); const s=await sink.status('SYNAPSE','ready'); return {state:a.activity.state.status,n:b.n,status:s.disposition};",
        &manifest(),
    );
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "one");
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded, "{done:?}");
    let v: Value = serde_json::from_str(done.result.as_ref().unwrap()).unwrap();
    assert_eq!(v, json!({"state":"unknown","n":2,"status":"published"}));
    assert_eq!(fake.calls("sink.digest").len(), 1);
    assert_eq!(fake.calls("sink.status").len(), 1);
}

#[test]
fn digest_replace_key_validation_refuses_invalid_values_unsent() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent.clone(), "replace-key-invalid");
    let malformed = json!([
        "",
        "x".repeat(201),
        "é".repeat(101),
        "line\nbreak",
        "delete\u{7f}",
        "control\u{0085}",
        null,
        4,
        false,
        {},
        []
    ]);
    let script = format!(
        "const keys={malformed}; const results=[]; for (const replace_key of keys) {{ try {{ await sink.digest('SYNAPSE',{{title:'x',body:'y'}},'silent',{{replace_key}}); results.push('sent'); }} catch (e) {{ results.push(e.data.code); }} }} return results;"
    );
    common::install(&f, &Caller::Operator, &script, &manifest());
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "invalid-keys");
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded);
    assert_eq!(
        serde_json::from_str::<Value>(done.result.as_ref().unwrap()).unwrap(),
        json!(vec![
            "invalid_arguments";
            malformed.as_array().unwrap().len()
        ])
    );
    assert!(fake.calls("sink.digest").is_empty());
    let calls = f.module.rt.calls(&run).unwrap();
    assert_eq!(calls.len(), malformed.as_array().unwrap().len());
    assert!(calls.iter().all(|call| call.request.is_none()));
}

#[test]
fn digest_replace_key_requires_core_catalog_capability() {
    let fake = Fake::new();
    set_core_digest_replace_key_capability(&fake, false);
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent.clone(), "replace-key-unsupported");
    common::install(
        &f,
        &Caller::Operator,
        "try { await sink.digest('SYNAPSE',{title:'x',body:'y'},'silent',{replace_key:'article:42'}); } catch (e) { return e.data.code; } return 'sent';",
        &manifest(),
    );
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "unsupported-key");
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded);
    assert_eq!(
        done.result.as_deref(),
        Some("\"digest_replace_key_unsupported\"")
    );
    assert!(fake.calls("sink.digest").is_empty());
    let calls = f.module.rt.calls(&run).unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].request.is_none());
}

#[test]
fn digest_replace_key_is_top_level_when_core_catalog_capability_is_present() {
    let fake = Fake::new();
    set_core_digest_replace_key_capability(&fake, true);
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent.clone(), "replace-key-supported");
    let replace_key = "é".repeat(100);
    let script = format!(
        "await sink.digest('SYNAPSE',{{title:'x',body:'y'}},'silent',{{replace_key:{}}}); return 1;",
        json!(replace_key)
    );
    common::install(&f, &Caller::Operator, &script, &manifest());
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "supported-key");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
    let sent = fake.calls("sink.digest");
    assert_eq!(sent.len(), 1);
    let expected = json!({
        "flow_id": "host-flow",
        "flow_version": 1,
        "run_id": run,
        "call_position": 0,
        "agent": "SYNAPSE",
        "action": "silent",
        "due_at": common::T0,
        "created_at": common::T0,
        "item": {"title":"x","body":"y"},
        "claim": null,
        "replace_key": replace_key
    });
    assert_eq!(sent[0]["params"], expected);
    assert_eq!(sent[0]["params"]["replace_key"], replace_key);
    assert!(sent[0]["params"]["item"].get("replace_key").is_none());
}

#[test]
fn digest_status_tag_and_claim_options_are_refused_before_send() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent.clone(), "removed-digest-options");
    let script = "const options=[{status_tag:'NI'},{claim:'episode'}]; const results=[]; for (const option of options) { try { await sink.digest('SYNAPSE',{title:'x',body:'y'},'silent',option); results.push('sent'); } catch (e) { results.push(e.data.code); } } return results;";
    common::install(&f, &Caller::Operator, script, &manifest());
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "removed-options");
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded);
    assert_eq!(
        serde_json::from_str::<Value>(done.result.as_ref().unwrap()).unwrap(),
        json!(["invalid_arguments", "invalid_arguments"])
    );
    assert!(fake.calls("sink.digest").is_empty());
    let calls = f.module.rt.calls(&run).unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call.request.is_none()));
}

#[test]
fn transport_failures_reach_journal_with_safe_retries_only() {
    for (op, failure, state, sends) in [
        (
            "post",
            WireError::Unknown("lost".into()),
            RunState::NeedsReconcile,
            1,
        ),
        (
            "post",
            WireError::NeverSent("not written".into()),
            RunState::Succeeded,
            2,
        ),
        (
            "send",
            WireError::Unknown("lost".into()),
            RunState::NeedsReconcile,
            1,
        ),
    ] {
        let fake = Fake::new();
        let consent = Arc::new(CoreConsent::new(fake.clone()));
        let f = worker_fixture(fake.clone(), consent.clone(), op);
        common::install(
            &f,
            &Caller::Operator,
            &format!("await ops.call('mock','{op}',{{n:1}});return 1;"),
            &manifest(),
        );
        approve(&fake, &f.module);
        fake.enqueue(op, vec![Err(failure)]);
        let run = common::admit(&f, "host-flow", "one");
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(f.module.rt.run(&run).unwrap().state, state);
        assert_eq!(fake.calls(op).len(), sends);
        if op == "send" {
            assert_eq!(
                fake.calls(op)[0]["call_key"],
                f.module.rt.calls(&run).unwrap()[0].idempotency_key
            );
        }
    }
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent.clone(), "sink-retry");
    common::install(
        &f,
        &Caller::Operator,
        "try { await sink.digest('SYNAPSE',{title:'x',body:'y'},'piggyback'); await ops.call('mock','post',{}); } catch(e) { return e.data.code; } return 'wrong';",
        &manifest(),
    );
    approve(&fake, &f.module);
    fake.enqueue(
        "sink.digest",
        vec![Err(WireError::Unknown("reply cut".into()))],
    );
    fake.enqueue(
        "post",
        vec![Err(WireError::Refused {
            code: "provider_denied".into(),
            message: "no".into(),
        })],
    );
    let run = common::admit(&f, "host-flow", "one");
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded);
    assert_eq!(done.result.as_deref(), Some("\"provider_denied\""));
    let calls = fake.calls("sink.digest");
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].to_string().as_bytes(),
        calls[1].to_string().as_bytes()
    );
    assert_eq!(
        calls[0]["params"],
        serde_json::from_str::<Value>(
            f.module.rt.calls(&run).unwrap()[0]
                .request
                .as_ref()
                .unwrap()
                .as_str()
        )
        .unwrap()
    );
}

#[test]
fn journaled_sink_intent_is_byte_identical_after_a_cut() {
    use basal_core::{Clock, Config, Durability};
    use basal_module::{
        dryrun::DryRunConfig,
        engine::EngineConfig,
        module::{Module, ModuleConfig},
        pool::{PoolConfig, ProcessSpawner},
    };
    use basal_testkit::harness::{Point, Probe};
    for (op, script, pending_registry) in [
        (
            "sink.digest",
            "await sink.digest('SYNAPSE',{title:'x',body:'y'},'piggyback',{replace_key:'article:42'}); return 1;",
            false,
        ),
        (
            "sink.status",
            "await sink.status('SYNAPSE','ready'); return 1;",
            false,
        ),
        ("agent.facts", "await facts('SYNAPSE'); return 1;", false),
        (
            "sink.board",
            "await sink.board.append('SYNAPSE','work'); return 1;",
            false,
        ),
        (
            "sink.board",
            "await sink.board.append('SYNAPSE','work'); return 1;",
            true,
        ),
    ] {
        let dir = common::scratch("core-cut");
        let clock = Clock::manual(common::T0);
        let pool = PoolConfig::new(
            basal_testkit::channel::worker_binary(),
            basal_module::process::WorkerLaunch::Plain,
        );
        let config = ModuleConfig {
            store_path: dir.join("basal.db"),
            durability: Durability::throwaway(),
            runtime: Config {
                clock: clock.clone(),
                install_gate: basal_core::InstallGate::Off,
                ..Default::default()
            },
            pool: pool.clone(),
            engine: EngineConfig::default(),
            dry_run: DryRunConfig::new(dir.join("dry")),
        };
        let fake = Fake::new();
        if op == "sink.digest" {
            set_core_digest_replace_key_capability(&fake, true);
        }
        let consent = Arc::new(CoreConsent::new(fake.clone()));
        let probe = Arc::new(Probe::crash_at(
            Point::parse(if pending_registry {
                "CallCommitted { position: 0 }#1"
            } else {
                "HostAnswered { position: 0 }#1"
            })
            .unwrap(),
        ));
        let mut dependencies = hosts(fake.clone(), consent.clone());
        dependencies.hooks = probe.clone();
        let module = Module::start(
            config.clone(),
            dependencies,
            Arc::new(ProcessSpawner::new(&pool)),
        )
        .unwrap();
        let mut test_manifest = manifest();
        test_manifest["board"] = json!([{"agent":"SYNAPSE","title":"Work"}]);
        module
            .handle(
                &Caller::Operator,
                "flow.install",
                json!({"script":script,"manifest":test_manifest.to_string()}),
            )
            .unwrap();
        approve(&fake, &module);
        let run = module
            .rt
            .admit_trigger(
                "host-flow",
                "cut",
                JsonText::new(json!({"kind":"schedule","due":"2026-05-01T00:00:00Z"}).to_string())
                    .unwrap(),
            )
            .unwrap()
            .run_id()
            .unwrap()
            .to_owned();
        if pending_registry {
            fake.enqueue(
                "agent.list",
                vec![Err(WireError::NeverSent("offline".into()))],
            );
        }
        assert!(module.engine.run_until_idle(50).is_err());
        module.rt.quiesce();
        module.pool.stop();
        assert!(probe.fired());
        if pending_registry {
            assert!(fake.calls("sink.board").is_empty());
            fake.enqueue(
                "sink.board",
                vec![Err(WireError::Refused {
                    code: "sink_store_failed".into(),
                    message: "temporary".into(),
                })],
            );
        }
        consent.attach(Arc::new(Decisions::default()));
        drop(module);
        clock.set(common::T0 + 10000);
        let module = Module::start(
            config,
            hosts(fake.clone(), consent.clone()),
            Arc::new(ProcessSpawner::new(&pool)),
        )
        .unwrap();
        if pending_registry {
            assert!(module.rt.calls(&run).unwrap()[0].request.is_none());
        }
        module.engine.run_until_idle(50).unwrap();
        if pending_registry {
            assert_eq!(
                module.rt.calls(&run).unwrap()[0].dispatch,
                basal_core::DispatchState::Deferred
            );
            let reads = fake.calls("agent.list").len();
            let retry: i64 = module
                .rt
                .store()
                .read(|c| {
                    Ok(c.query_row(
                        "SELECT retry_not_before FROM journal WHERE run_id=?1",
                        [&run],
                        |r| r.get(0),
                    )?)
                })
                .unwrap();
            clock.set(retry);
            module.engine.run_until_idle(50).unwrap();
            assert_eq!(fake.calls("agent.list").len(), reads);
        }
        assert_eq!(module.rt.run(&run).unwrap().state, RunState::Succeeded);
        let sent = fake.calls(op);
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[0].to_string().as_bytes(),
            sent[1].to_string().as_bytes()
        );
        if op == "sink.digest" {
            assert_eq!(sent[0]["params"]["claim"], Value::Null);
            assert_eq!(sent[1]["params"]["claim"], Value::Null);
            assert_eq!(sent[0]["params"]["replace_key"], "article:42");
            assert_eq!(sent[1]["params"]["replace_key"], "article:42");
            assert_eq!(sent[1]["params"]["created_at"], common::T0);
            assert_eq!(sent[1]["params"]["due_at"], common::T0);
            assert_eq!(sent[1]["params"]["run_id"], run);
            assert_eq!(sent[1]["params"]["call_position"], 0);
        }
        if op == "sink.status" {
            assert_eq!(sent[1]["params"]["revision"], common::T0);
        }
        if op == "sink.board" {
            assert_eq!(sent[1]["params"]["stamp"]["flow_version"], 1);
            assert_eq!(
                sent[1]["params"]["receipt_key"],
                module.rt.calls(&run).unwrap()[0].idempotency_key
            );
        } else {
            assert_eq!(sent[1]["params"]["flow_version"], 1);
        }
        module.pool.stop();
        consent.attach(Arc::new(Decisions::default()));
        drop(module);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn consent_hash_matches_shared_vectors_and_full_c2_envelope() {
    use basal_host::InstallCard;
    use basal_host::core_consent::request;
    for (script, manifest, hash) in [
        (
            "",
            "",
            "b5377f42c91b9631929ca590092769366de868dd1fc3c217d2abfc6f48382a78",
        ),
        (
            "a",
            "b",
            "cbca786f5bfade88b7953752a4144f71d49f585751311684635ed1169a956473",
        ),
        (
            "return 1;",
            "{\"id\":\"flow-x\",\"version\":1}",
            "7f9dcaf2bf757a09c91732726dc294f1178187e822c195c83ef66902ecb24841",
        ),
    ] {
        let actual =
            basal_module::card::code_hash_hex(&basal_core::ids::code_hash(script, manifest));
        let summary = json!({"window_from_ms":1789990000000i64,"window_to_ms":1790000000000i64,"partial":false,"reads":[],"writes":[]});
        let card = InstallCard {
            card_id: "local-card".into(),
            flow_id: "flow-x".into(),
            version: 3,
            fields: json!({"purpose":"Purpose","wire_author":{"scope":"5e1f0c3a9b7d4e2f8a6c1b3d5f7e9a0c"},"placement":"machine:studio","warnings":[{"kind":"private_text_with_outbound_op","text":"Private text can leave"}],"dry_run_summary":summary,"code_hash":actual,"code":{"script":script,"manifest":manifest},"token_cap":{"window":"1d"},"token_window":{"input_tokens":11,"cache_write_tokens":12,"output_tokens":13,"cached_input_tokens":14}}),
        };
        let sent = request(&card).unwrap();
        assert_eq!(
            sent,
            json!({"kind":"flow_install","title":"Install flow flow-x v3","prompt":"Purpose","options":[{"id":"approve","label":"Approve","effect":"grant"},{"id":"decline","label":"Decline","effect":"decline"}],"default":"decline","urgency":"normal","on_expiry":"deny","material_damage":false,"late_execution":"notify_only","args_digest":hash,"dedup_key":"flow_install:flow-x:3","target":{"kind":"flow","label":"flow-x v3"},"facts":[],"preview":{"label":"Code","text":script},"expires_in_ms":86400000,"flow_install":{"flow_id":"flow-x","version":3,"code_hash":hash,"script":script,"manifest_json":manifest,"author":{"scope":"5e1f0c3a9b7d4e2f8a6c1b3d5f7e9a0c"},"placement":"machine:studio","warnings":[{"code":"private_text_with_outbound_op","detail":"Private text can leave"}],"dry_run_summary":summary,"token_usage":{"window":"1d","fresh_input":11,"cache_write":12,"output":13,"cache_read":14}}})
        );
    }
}

#[test]
fn denylist_and_paged_agent_registry_fail_closed() {
    use basal_host::Catalog;
    let fake = Fake::new();
    let mut catalog = fake.catalog.lock().unwrap().clone();
    catalog["modules"][0]["module_id"] = json!("aft");
    catalog["modules"][0]["roles"][1]["tools"][0]["name"] = json!("bash");
    *fake.catalog.lock().unwrap() = catalog;
    let cat = Arc::new(SubcCatalog::new(fake.clone()));
    assert!(cat.op("aft", "bash").unwrap().shell_capable);
    let host = ModuleOpsHost::new(fake.clone(), cat.clone());
    let req = CallRequest {
        kind: CallKind::Op {
            module: "aft".into(),
            op: "bash".into(),
        },
        ..call(Primitive::Facts, json!({}))
    };
    let Dispatched::Completed(r) = host.dispatch(&req).unwrap() else {
        panic!("completed")
    };
    assert_eq!(r.settlement, Settlement::Rejected);
    assert!(fake.calls("bash").is_empty());
    fake.enqueue(
        "agent.list",
        vec![
            Ok(json!({"agents":[],"next_cursor":"page2"})),
            Ok(json!({"agents":[{"agent_id":"ag_2","name":"ALF"}]})),
        ],
    );
    assert!(cat.agent_known("ALF").unwrap());
    assert_eq!(
        fake.calls("agent.list")[1]["params"],
        json!({"cursor":"page2"})
    );
    fake.enqueue(
        "agent.list",
        vec![
            Ok(json!({"agents":[],"next_cursor":"page2"})),
            Ok(json!({"agents":[{"agent_id":"ag_2","name":"ALF"}]})),
        ],
    );
    assert_eq!(cat.agent_id("ALF").unwrap().as_deref(), Some("ag_2"));
    fake.enqueue(
        "agent.list",
        vec![Ok(json!({
            "agents":[{"agent_id":"ag_2","name":"ALF"}]
        }))],
    );
    assert_eq!(cat.agent_id("ag_2").unwrap().as_deref(), Some("ag_2"));
    fake.enqueue("agent.list", vec![Ok(json!({"agents":[]}))]);
    assert_eq!(cat.agent_id("missing"), Ok(None));
    // A failed read must stay an error, never an answer that the agent is
    // absent: callers refuse it as retryable rather than unknown.
    fake.enqueue(
        "agent.list",
        vec![Err(WireError::Unknown("registry unavailable".into()))],
    );
    assert!(cat.agent_id("ALF").is_err());
    fake.enqueue("agent.list", vec![Ok(json!({"agents":[{"name":"ALF"}]}))]);
    assert!(
        cat.agent_id("ALF").is_err(),
        "a name without a stable id cannot confer ownership"
    );
    fake.enqueue(
        "agent.list",
        vec![
            Ok(json!({"agents":[],"next_cursor":"repeat"})),
            Ok(json!({"agents":[],"next_cursor":"repeat"})),
        ],
    );
    assert!(cat.known_agent("missing").is_err());
    fake.enqueue(
        "agent.list",
        vec![Err(WireError::Unknown("registry unavailable".into()))],
    );
    assert!(cat.agent_known("ALF").is_err());
}
#[test]
fn sink_reply_missing_fields_and_wrong_types_are_unknown_outcomes() {
    for (p, v) in [
        (
            Primitive::SinkDigest,
            json!({"disposition":"stored","fire_id":"wf_1","replayed":false}),
        ),
        (
            Primitive::SinkDigest,
            json!({"disposition":"claim_in_fallback","fire_id":null,"replayed":true}),
        ),
        (
            Primitive::SinkDigest,
            json!({"disposition":"episode_ended","fire_id":null,"replayed":true}),
        ),
        (
            Primitive::SinkStatus,
            json!({"disposition":"published","accepted_revision":17,"segment":"flow:f","scope":"session:s","replayed":false}),
        ),
        (
            Primitive::SinkStatus,
            json!({"disposition":"no_live_session","accepted_revision":null,"segment":null,"scope":null,"replayed":true}),
        ),
    ] {
        let mut extra = v.clone();
        extra["future"] = json!({"ignored":true});
        assert_eq!(validate_reply(p, extra.clone(), false).unwrap(), extra);
        for key in v.as_object().unwrap().keys() {
            let mut missing = v.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                matches!(
                    validate_reply(p, missing, false),
                    Err(WireError::Unreadable(_))
                ),
                "missing {key}: {v}"
            );
            let mut wrong = v.clone();
            wrong[key] = json!({});
            assert!(
                matches!(
                    validate_reply(p, wrong, false),
                    Err(WireError::Unreadable(_))
                ),
                "wrong {key}: {v}"
            );
        }
    }
    for v in [
        json!({"disposition":"stored","fire_id":null,"replayed":false}),
        json!({"disposition":"episode_ended","fire_id":"wf_1","replayed":false}),
    ] {
        assert!(validate_reply(Primitive::SinkDigest, v, false).is_err());
    }
}

#[test]
fn operator_keyed_tools_retry_but_unfenceable_tools_do_not() {
    use basal_host::CallClass;
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let catalog = Arc::new(SubcCatalog::new(fake.clone()));
    let ops = Arc::new(
        ModuleOpsHost::new(fake.clone(), catalog.clone()).with_idempotent_ops(
            [
                ("mock".into(), "send".into()),
                ("mock".into(), "shell".into()),
                ("mock".into(), "post".into()),
            ]
            .into(),
        ),
    );
    let kind = |op: &str| CallKind::Op {
        module: "mock".into(),
        op: op.into(),
    };
    assert_eq!(
        ops.classify(&kind("send")),
        CallClass::Mutation {
            honours_idempotency_keys: true
        }
    );
    assert_eq!(
        ops.classify(&kind("shell")),
        CallClass::Mutation {
            honours_idempotency_keys: false
        }
    );
    assert_eq!(
        ops.classify(&kind("post")),
        CallClass::Mutation {
            honours_idempotency_keys: false
        }
    );
    let f = common::fixture(
        "keyed-tool",
        common::Options {
            hosts: Some(Hosts {
                transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
                host: Arc::new(RoutingHost::new(
                    ops,
                    Arc::new(CoreHost::new(fake.clone())),
                    Arc::new(UnconfiguredHost::new()),
                )),
                catalog,
                consent: consent.clone(),
                hooks: Arc::new(NoHooks),
            }),
            ..common::Options::with_worker()
        },
    );
    common::install(
        &f,
        &Caller::Operator,
        "await ops.call('mock','send',{n:1});return 1;",
        &manifest(),
    );
    approve(&fake, &f.module);
    fake.enqueue("send", vec![Err(WireError::Unknown("reply lost".into()))]);
    let run = common::admit(&f, "host-flow", "one");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
    let sent = fake.calls("send");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    assert_eq!(
        sent[0]["call_key"],
        f.module.rt.calls(&run).unwrap()[0].idempotency_key
    );
}

#[test]
fn consent_refusal_codes_and_unknown_transport_are_typed() {
    use basal_host::{ConsentError, InstallCard};
    let fake = Fake::new();
    let consent = CoreConsent::new(fake.clone());
    let card = InstallCard {
        card_id: "card:f:v1:0123456789abcdef".into(),
        flow_id: "f".into(),
        version: 1,
        fields: json!({"purpose":"Purpose","wire_author":{"operator":true},"code_hash":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","code":{"script":"return 1;","manifest":"{\"id\":\"f\",\"version\":1}"},"placement":"machine:local","warnings":[],"dry_run_summary":{},"token_cap":null,"token_window":null}),
    };
    for code in [
        "elicitation_requester_refused",
        "flow_install_hash_mismatch",
        "flow_install_manifest_mismatch",
        "flow_install_unknown_agent",
        "flow_install_claim_unsupported",
        "elicitation_invalid_request",
        "elicitation_unknown_kind",
    ] {
        fake.enqueue(
            "flow.install_request",
            vec![Err(WireError::Refused {
                code: code.into(),
                message: "reason".into(),
            })],
        );
        assert_eq!(
            consent.raise(&card),
            Err(ConsentError::Refused(format!("{code}: reason")))
        );
    }
    for e in [
        WireError::Unknown("lost".into()),
        WireError::NeverSent("not written".into()),
    ] {
        fake.enqueue("flow.install_request", vec![Err(e)]);
        assert!(matches!(
            consent.raise(&card),
            Err(ConsentError::Unavailable(_))
        ));
    }
}

struct ChangeCatalogAfterIntent(Arc<Fake>);
impl basal_core::Hooks for ChangeCatalogAfterIntent {
    fn at(&self, _: &str, boundary: &basal_core::Boundary) -> basal_core::Step {
        if *boundary == (basal_core::Boundary::CallCommitted { position: 0 }) {
            self.0.catalog.lock().unwrap()["modules"][0]["roles"][0]["operations"][0]["kind"] =
                json!("mutate");
        }
        basal_core::Step::Continue
    }
}
#[test]
fn catalog_changes_cannot_execute_mutations_under_a_query_retry_policy() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let mut dependencies = hosts(fake.clone(), consent.clone());
    dependencies.hooks = Arc::new(ChangeCatalogAfterIntent(fake.clone()));
    let f = common::fixture(
        "catalog-change",
        common::Options {
            hosts: Some(dependencies),
            ..common::Options::with_worker()
        },
    );
    common::install(
        &f,
        &Caller::Operator,
        "try { await ops.call('mock','echo',{}); } catch(e) { return e.data.code; } return 'sent';",
        &manifest(),
    );
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "one");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(
        f.module.rt.run(&run).unwrap().result.as_deref(),
        Some("\"op_kind_changed\"")
    );
    assert!(fake.calls("echo").is_empty());
    assert_eq!(
        f.module.rt.calls(&run).unwrap()[0].class,
        basal_core::StoredClass::Query
    );
}

#[test]
fn fact_identity_and_clock_fields_are_required_without_normalising_leaves() {
    let reply = json!({"agent_id":"ag_synapse","as_of":17,"home":"local","core_boot_at":0,"activity":{"state":{"value":null,"status":"unknown","observed_at":null,"privacy":"public","source":{"kind":"memory","ref":"session_activity_states","survives_restart":false}}},"attention":{"last_assistant_text":{"value":null,"status":"denied","observed_at":null,"privacy":"private-text","source":{"kind":"store","ref":"sessions","survives_restart":true}}},"future_group":{"opaque":[1,2]}});
    let fake = Fake::new();
    fake.enqueue("agent.facts", vec![Ok(reply.clone())]);
    let host = CoreHost::new(fake);
    let Dispatched::Completed(outcome) = host.dispatch(&call(Primitive::Facts, json!({}))).unwrap()
    else {
        panic!("completed")
    };
    assert_eq!(
        serde_json::from_str::<Value>(outcome.value.as_str()).unwrap(),
        reply
    );
    for key in ["agent_id", "as_of", "home", "core_boot_at"] {
        let mut missing = reply.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(matches!(
            validate_reply(Primitive::Facts, missing, false),
            Err(WireError::Unreadable(_))
        ));
        let mut wrong = reply.clone();
        wrong[key] = json!({});
        assert!(matches!(
            validate_reply(Primitive::Facts, wrong, false),
            Err(WireError::Unreadable(_))
        ));
    }
}

fn model_manifest() -> Value {
    let mut m = manifest();
    m["llm"] = json!({"iq":70,"eq":20,"token_cap":{"tokens":10000,"window":"1h"},"max_output":32});
    m
}
fn model_fixture(
    tag: &str,
    wire: Arc<Fake>,
    selector: Arc<dyn basal_host::selector::ModelSelector>,
    hooks: Arc<dyn basal_core::Hooks>,
) -> (
    common::Fixture,
    Arc<basal_host::broca::BrocaHost>,
    Arc<basal_host::broca::fake::FakeBroca>,
    Arc<basal_core::broca::BrocaStore>,
) {
    let snapshots = Arc::new(basal_core::broca::BrocaStore::default());
    let fake = Arc::new(basal_host::broca::fake::FakeBroca::default());
    let model = Arc::new(basal_host::broca::BrocaHost::new(
        fake.clone(),
        snapshots.clone(),
        "/project".into(),
        "basal".into(),
        selector.clone(),
    ));
    let consent = Arc::new(CoreConsent::new(wire.clone()));
    let catalog = Arc::new(SubcCatalog::new(wire.clone()));
    let dependencies = Hosts {
        transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
        host: Arc::new(RoutingHost::new(
            Arc::new(ModuleOpsHost::new(wire.clone(), catalog.clone())),
            Arc::new(CoreHost::new(wire)),
            model.clone(),
        )),
        catalog,
        consent,
        hooks,
    };
    let f = common::fixture_with_store(
        tag,
        common::Options {
            hosts: Some(dependencies),
            selector,
            ..common::Options::with_worker()
        },
        |shared| snapshots.bind(shared).map_err(|e| e.to_string()),
    );
    (f, model, fake, snapshots)
}
fn approve_model(f: &common::Fixture) {
    let card = f.module.rt.cards("host-flow").unwrap()[0].card_id.clone();
    f.module
        .rt
        .decide_card(&card, basal_core::cards::Decision::Approve, "operator")
        .unwrap();
}

#[test]
fn routing_host_model_selection_and_digest_sink_share_the_journal_store() {
    use basal_host::broca::{
        StateStore,
        wire::{RunFinishReason, Usage},
    };
    let wire = Fake::new();
    let selector = Arc::new(basal_host::selector::RoutingSelector::new(wire.clone()));
    let (f, model, fake, snapshots) = model_fixture(
        "model-and-digest",
        wire.clone(),
        selector,
        Arc::new(NoHooks),
    );
    common::install(
        &f,
        &Caller::Operator,
        "const answer=await llm({prompt:'hello',max_output:999}); const receipt=await sink.digest('SYNAPSE',{title:'answer',body:answer.text}); return {text:answer.text,disposition:receipt.disposition};",
        &model_manifest(),
    );
    approve_model(&f);
    let run = common::admit(&f, "host-flow", "one");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Suspended);
    let call = f.module.rt.calls(&run).unwrap()[0].clone();
    let request: Value = serde_json::from_str(call.request.as_ref().unwrap().as_str()).unwrap();
    assert_eq!(
        request["selection"],
        json!({"providerID":"registry-provider","modelID":"registry-model","decisionID":format!("decision:{}",call.idempotency_key),"runner":{"provider":"fake","model":"test"}})
    );
    assert_eq!(
        wire.calls("route.select")[0]["params"],
        json!({"targetAgent":"flow","requirements":{"iq":70,"eq":20},"excludeRouteKeys":[],"sendID":call.idempotency_key,"taskId":format!("flow:host-flow:{run}"),"substrate":"broca"})
    );
    let sent: Value = serde_json::from_slice(&fake.sends()[0].1).unwrap();
    assert_eq!(sent["model"], json!({"provider":"fake","model":"test"}));
    assert_eq!(sent["generation"]["max_output_tokens"], 32);
    f.module.rt.quiesce();
    assert_eq!(
        fake.calls().1,
        1,
        "an accepted model call is read once after its handle commits"
    );
    assert!(
        snapshots.load().unwrap()[0].check_at.is_some(),
        "an accepted model call has a scheduled check, so a lost run.ended cannot strand it"
    );
    // The journal retains the full envelope; the snapshot needs only its
    // identity digest and the exact frozen send bytes, not another prompt copy.
    let saved = snapshots.load().unwrap().remove(0);
    assert_eq!(saved.params, fake.sends()[0].1);
    assert_eq!(
        saved.envelope,
        format!(
            "blake3:{}",
            serde_json::to_string(&basal_proto::ArgsDigest::of(call.request.as_ref().unwrap()).0)
                .unwrap()
        )
    );
    fake.finish(
        &call.idempotency_key,
        "answer",
        RunFinishReason::Completed,
        Some(Usage {
            input_tokens: Some(7),
            cache_write_tokens: Some(2),
            output_tokens: Some(5),
            cached_input_tokens: Some(100),
            reasoning_tokens: Some(3),
        }),
    )
    .unwrap();
    model.poll().unwrap();
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded);
    assert_eq!(
        serde_json::from_str::<Value>(done.result.as_deref().unwrap()).unwrap(),
        json!({"disposition":"stored","text":"answer"})
    );
    assert_eq!(
        wire.calls("sink.digest")[0]["params"]["action"],
        "piggyback"
    );
    assert_eq!(
        wire.calls("sink.digest")[0]["params"]["item"]["body"],
        "answer"
    );
    assert_eq!(
        wire.calls("route.set_decision_outcome")[0]["params"],
        json!({"decisionID":format!("decision:{}",call.idempotency_key),"outcome":"completed"})
    );
    f.module
        .rt
        .store()
        .read(|c| {
            let tokens: i64 = c.query_row(
                "SELECT output_tokens FROM token_ledger WHERE send_id=?1",
                [&call.idempotency_key],
                |r| r.get(0),
            )?;
            assert_eq!(tokens, 5);
            Ok(())
        })
        .unwrap();
}
#[test]
fn routing_select_request_bytes_are_exact_without_available_models() {
    use basal_host::selector::{ModelSelector, RoutingSelector, SelectionRequest};
    let wire = Fake::new();
    let selector = RoutingSelector::new(wire.clone());
    selector
        .select(&SelectionRequest {
            caller_class: "flow".into(),
            iq: 70,
            eq: 20,
            flow_id: "flow-a".into(),
            run_id: "run-a".into(),
            send_id: "send-1".into(),
        })
        .unwrap();
    assert_eq!(serde_json::to_vec(&wire.calls("route.select")[0]).unwrap(),br#"{"method":"route.select","params":{"excludeRouteKeys":[],"requirements":{"eq":20,"iq":70},"sendID":"send-1","substrate":"broca","targetAgent":"flow","taskId":"flow:flow-a:run-a"}}"#);
    assert_eq!(wire.records.lock().unwrap()[0].0, "prefrontal-routing");
    for (outcome, name) in [
        (basal_host::selector::ModelOutcome::Completed, "completed"),
        (basal_host::selector::ModelOutcome::Error, "error"),
        (basal_host::selector::ModelOutcome::Cancelled, "cancelled"),
        (
            basal_host::selector::ModelOutcome::Interrupted,
            "interrupted",
        ),
    ] {
        selector.report_outcome("decision-1", outcome).unwrap();
        let expected = format!(
            "{{\"method\":\"route.set_decision_outcome\",\"params\":{{\"decisionID\":\"decision-1\",\"outcome\":\"{name}\"}}}}"
        );
        assert_eq!(
            wire.calls("route.set_decision_outcome")
                .last()
                .unwrap()
                .to_string(),
            expected
        );
    }
}
#[test]
fn routing_selection_reply_shapes_require_runner_and_preserve_both_identities() {
    use basal_host::selector::{SelectionError, decode_selection};
    for variant in [None, Some("thinking")] {
        let mut reply = json!({"selected":{"model":{"providerID":"registry-provider","modelID":"registry-model"}},"decisionID":"decision-1","runner":{"provider":"runner-provider","model":"runner-model"}});
        if let Some(v) = variant {
            reply["selected"]["model"]["variant"] = json!(v);
            reply["runner"]["variant"] = json!("runner-variant");
        }
        let selected = decode_selection(reply.clone()).unwrap();
        assert_eq!(selected.provider_id, "registry-provider");
        assert_eq!(selected.model_id, "registry-model");
        assert_eq!(selected.variant.as_deref(), variant);
        assert_eq!(selected.decision_id, "decision-1");
        assert_eq!(
            serde_json::to_value(selected.runner).unwrap(),
            reply["runner"]
        );
        let mut missing = reply.clone();
        missing.as_object_mut().unwrap().remove("runner");
        assert!(
            matches!(decode_selection(missing),Err(SelectionError::Refused {code,..}) if code=="route_runner_missing")
        );
        for key in ["providerID", "modelID"] {
            let mut invalid = reply.clone();
            invalid["selected"]["model"][key] = json!({});
            assert!(
                matches!(decode_selection(invalid),Err(SelectionError::Refused {code,..}) if code=="route_reply_invalid")
            );
        }
        for key in ["provider", "model"] {
            let mut invalid = reply.clone();
            invalid["runner"][key] = json!({});
            assert!(decode_selection(invalid).is_err());
        }
        let mut invalid = reply.clone();
        invalid["decisionID"] = Value::Null;
        assert!(decode_selection(invalid).is_err());
        let mut invalid = reply;
        invalid["runner"] = Value::Null;
        assert!(decode_selection(invalid).is_err());
    }
}
#[test]
fn selector_refusals_are_journaled_with_each_code_and_never_send_broca() {
    use basal_host::selector::RoutingSelector;
    for code in [
        "no_available_model",
        "quota_or_cooldown_exhausted",
        "all_routes_inadequate",
        "no_adequate_route",
        "missing_required_capability",
        "context_too_small",
        "invalid_requirements",
        "route_runner_missing",
    ] {
        let wire = Fake::new();
        let selector = Arc::new(RoutingSelector::new(wire.clone()));
        let (f, _, fake, _) = model_fixture(code, wire.clone(), selector, Arc::new(NoHooks));
        common::install(
            &f,
            &Caller::Operator,
            "try {return await llm({prompt:'x'});}catch(e){return e.data.code;}",
            &model_manifest(),
        );
        approve_model(&f);
        if code == "route_runner_missing" {
            wire.enqueue(
                "route.select",
                vec![Ok(
                    json!({"selected":{"model":{"providerID":"p","modelID":"m"}},"decisionID":"d"}),
                )],
            );
        } else {
            wire.enqueue(
                "route.select",
                vec![Err(WireError::Refused {
                    code: code.into(),
                    message: "no route".into(),
                })],
            );
        }
        let run = common::admit(&f, "host-flow", "one");
        f.module.engine.run_until_idle(50).unwrap();
        let done = f.module.rt.run(&run).unwrap();
        assert_eq!(done.state, RunState::Succeeded);
        assert_eq!(done.result, Some(json!(code).to_string()));
        assert!(fake.sends().is_empty());
        assert_eq!(wire.calls("route.select").len(), 1);
        let row = f.module.rt.calls(&run).unwrap()[0].clone();
        assert_eq!(
            row.outcome.as_ref().unwrap().settlement,
            Settlement::Rejected
        );
        assert!(row.request.is_none());
    }
}
#[test]
fn unreachable_selector_retries_boundedly_then_journals_route_unavailable() {
    use basal_host::selector::RoutingSelector;
    let wire = Fake::new();
    let selector = Arc::new(RoutingSelector::new(wire.clone()));
    let (f, _, fake, _) = model_fixture("route-down", wire.clone(), selector, Arc::new(NoHooks));
    common::install(
        &f,
        &Caller::Operator,
        "try {return await classify('x',['a','b']);}catch(e){return e.data.code;}",
        &model_manifest(),
    );
    approve_model(&f);
    wire.enqueue(
        "route.select",
        vec![Err(WireError::Unknown("lost reply".into())); 10],
    );
    let run = common::admit(&f, "host-flow", "one");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(
        f.module.rt.run(&run).unwrap().result.as_deref(),
        Some("\"route_unavailable\"")
    );
    assert!(fake.sends().is_empty());
    let requests = wire.calls("route.select");
    assert_eq!(
        requests.len(),
        f.module.rt.config().unavailable_retries as usize + 1
    );
    assert!(requests.iter().all(|r| r == &requests[0]));
}
#[test]
fn script_model_or_provider_is_rejected_before_selection_even_if_it_matches() {
    use basal_host::selector::FakeSelector;
    for selection in [
        json!({"model":{"provider":"fake","model":"test"}}),
        json!({"model":{"provider":"expensive","model":"other"}}),
        json!({"provider":"fake"}),
        json!({"provider":"other"}),
        json!({"model":null}),
    ] {
        let wire = Fake::new();
        let selector = Arc::new(FakeSelector::default());
        let (f, _, fake, _) =
            model_fixture("no-script-model", wire, selector.clone(), Arc::new(NoHooks));
        let script = format!(
            "try {{return await llm(Object.assign({{prompt:'x'}},{}));}}catch(e){{return e.data.code;}}",
            selection
        );
        common::install(&f, &Caller::Operator, &script, &model_manifest());
        approve_model(&f);
        let run = common::admit(&f, "host-flow", "one");
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(
            f.module.rt.run(&run).unwrap().result.as_deref(),
            Some("\"model_not_allowed\"")
        );
        assert!(fake.sends().is_empty());
        assert!(selector.requests.lock().unwrap().is_empty());
    }
}
#[test]
fn omitted_digest_action_uses_each_recipient_approved_cap() {
    for cap in ["silent", "piggyback", "wake"] {
        let wire = Fake::new();
        let consent = Arc::new(CoreConsent::new(wire.clone()));
        let f = worker_fixture(wire.clone(), consent.clone(), "digest-default");
        let mut m = manifest();
        m["sinks"][0]["digest_max"] = json!(cap);
        common::install(
            &f,
            &Caller::Operator,
            "await sink.digest('SYNAPSE',{title:'x'});return 1;",
            &m,
        );
        approve(&wire, &f.module);
        let run = common::admit(&f, "host-flow", "one");
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
        assert_eq!(wire.calls("sink.digest")[0]["params"]["action"], cap);
    }
}

#[test]
fn selections_freeze_at_intent_commit_and_only_uncommitted_calls_reselect() {
    use basal_core::{Clock, Config, Durability};
    use basal_host::broca::{
        BrocaHost,
        fake::FakeBroca,
        wire::{ModelParams, RunFinishReason},
    };
    use basal_host::selector::{FakeSelector, ModelSelection};
    use basal_module::{
        dryrun::DryRunConfig,
        engine::EngineConfig,
        module::{Module, ModuleConfig},
        pool::{PoolConfig, ProcessSpawner},
    };
    use basal_testkit::harness::{Point, Probe};
    for boundary in [
        "ModelSelected { position: 0 }#1",
        "CallCommitted { position: 0 }#1",
        "HostAnswered { position: 0 }#1",
    ] {
        let dir = common::scratch("selection-cut");
        let selector = Arc::new(FakeSelector::default());
        let wire = Fake::new();
        let fake = Arc::new(FakeBroca::default());
        let choice = |id: &str| ModelSelection {
            provider_id: format!("registry-{id}"),
            model_id: format!("model-{id}"),
            variant: Some("registry-variant".into()),
            decision_id: id.into(),
            runner: ModelParams {
                provider: format!("provider-{id}"),
                model: format!("runner-{id}"),
                variant: Some("runner-variant".into()),
            },
        };
        selector
            .replies
            .lock()
            .unwrap()
            .extend([Ok(choice("first")), Ok(choice("second"))]);
        let pool = PoolConfig::new(
            basal_testkit::channel::worker_binary(),
            basal_module::process::WorkerLaunch::Plain,
        );
        let config = ModuleConfig {
            store_path: dir.join("basal.db"),
            durability: Durability::throwaway(),
            runtime: Config {
                clock: Clock::manual(common::T0),
                selector: selector.clone(),
                retry_backoff: std::time::Duration::ZERO,
                install_gate: basal_core::InstallGate::Off,
                ..Default::default()
            },
            pool: pool.clone(),
            engine: EngineConfig::default(),
            dry_run: DryRunConfig::new(dir.join("dry")),
        };
        let start = |hooks: Arc<dyn basal_core::Hooks>| {
            let snapshots = Arc::new(basal_core::broca::BrocaStore::default());
            let model = Arc::new(BrocaHost::new(
                fake.clone(),
                snapshots.clone(),
                "/project".into(),
                "basal".into(),
                selector.clone(),
            ));
            let cat = Arc::new(SubcCatalog::new(wire.clone()));
            let dependencies = Hosts {
                transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
                host: Arc::new(RoutingHost::new(
                    Arc::new(ModuleOpsHost::new(wire.clone(), cat.clone())),
                    Arc::new(CoreHost::new(wire.clone())),
                    model.clone(),
                )),
                catalog: cat,
                consent: Arc::new(CoreConsent::new(wire.clone())),
                hooks,
            };
            let module = Module::start_with_store(
                config.clone(),
                dependencies,
                Arc::new(ProcessSpawner::new(&pool)),
                |shared| snapshots.bind(shared).map_err(|e| e.to_string()),
            )
            .unwrap();
            (module, model, snapshots)
        };
        let probe = Arc::new(Probe::crash_at(Point::parse(boundary).unwrap()));
        let (module, model, snapshots) = start(probe.clone());
        module.handle(&Caller::Operator,"flow.install",json!({"script":"return await llm({prompt:'x'});","manifest":model_manifest().to_string()})).unwrap();
        let card = module.rt.cards("host-flow").unwrap()[0].card_id.clone();
        module
            .rt
            .decide_card(&card, basal_core::cards::Decision::Approve, "operator")
            .unwrap();
        let run = module
            .rt
            .admit_trigger("host-flow", "cut", JsonText::null())
            .unwrap()
            .run_id()
            .unwrap()
            .to_owned();
        assert!(module.engine.run_until_idle(50).is_err());
        assert!(probe.fired());
        module.rt.quiesce();
        module.pool.stop();
        assert_eq!(selector.requests.lock().unwrap().len(), 1);
        let before_commit = boundary.starts_with("ModelSelected");
        if before_commit {
            assert!(fake.sends().is_empty());
        }
        drop(module);
        drop(model);
        drop(snapshots);
        let (module, model, snapshots) = start(Arc::new(NoHooks));
        assert_eq!(
            module.rt.calls(&run).unwrap().len(),
            if before_commit { 0 } else { 1 }
        );
        module.engine.run_until_idle(50).unwrap();
        module.rt.quiesce();
        assert_eq!(module.rt.run(&run).unwrap().state, RunState::Suspended);
        assert_eq!(
            selector.requests.lock().unwrap().len(),
            if before_commit { 2 } else { 1 }
        );
        let row = module.rt.calls(&run).unwrap()[0].clone();
        let envelope: Value = serde_json::from_str(row.request.as_ref().unwrap().as_str()).unwrap();
        let expected = choice(if before_commit { "second" } else { "first" });
        assert_eq!(
            envelope["selection"],
            serde_json::to_value(&expected).unwrap()
        );
        let sends = fake.sends();
        assert!(!sends.is_empty());
        assert!(sends.iter().all(|(_, bytes)| bytes == &sends[0].1));
        assert_eq!(
            serde_json::from_slice::<Value>(&sends[0].1).unwrap()["model"],
            serde_json::to_value(expected.runner).unwrap()
        );
        fake.finish(
            &row.idempotency_key,
            "answer",
            RunFinishReason::Completed,
            None,
        )
        .unwrap();
        model.poll().unwrap();
        module.engine.run_until_idle(50).unwrap();
        assert_eq!(module.rt.run(&run).unwrap().state, RunState::Succeeded);
        assert_eq!(selector.reports.lock().unwrap()[0].0, expected.decision_id);
        module.pool.stop();
        drop(module);
        drop(model);
        drop(snapshots);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn an_unconfigured_selector_has_no_default_model() {
    let wire = Fake::new();
    let (f, _, fake, _) = model_fixture(
        "no-default-model",
        wire,
        Arc::new(basal_host::selector::UnconfiguredSelector),
        Arc::new(NoHooks),
    );
    common::install(
        &f,
        &Caller::Operator,
        "try{return await llm({prompt:'x'});}catch(e){return e.data.code;}",
        &model_manifest(),
    );
    approve_model(&f);
    let run = common::admit(&f, "host-flow", "one");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(
        f.module.rt.run(&run).unwrap().result.as_deref(),
        Some("\"route_not_configured\"")
    );
    assert!(fake.sends().is_empty());
}
#[test]
fn management_envelopes_preserve_frozen_parameter_bytes() {
    let params = br#"{ "prompt" : "unicode text", "send_id" : "key-1" }"#;
    let bytes = basal_host::transport::management_bytes("session.send", params).unwrap();
    assert_eq!(
        bytes,
        br#"{"method":"session.send","params":{ "prompt" : "unicode text", "send_id" : "key-1" }}"#
    );
    assert!(matches!(
        basal_host::transport::management_bytes("session.send", b"broken"),
        Err(WireError::NeverSent(_))
    ));
    assert!(basal_host::transport::management_bytes("session.send", b"[]").is_err());
}

#[test]
fn runtime_owner_notice_uses_the_core_management_carrier() {
    let fake = Fake::new();
    let host = CoreHost::new(fake.clone());
    let capability = basal_host::core_host::FLOW_OWNER_NOTICE_V1;
    assert!(!host.supports_core_capability(capability));
    set_core_capability(&fake, capability, true);
    assert!(host.supports_core_capability(capability));
    let vectors: Value = serde_json::from_str(include_str!(
        "../../../test-vectors/owner-notice/vectors.json"
    ))
    .unwrap();
    let request: Value =
        serde_json::from_str(vectors["cases"][0]["request"].as_str().unwrap()).unwrap();
    fake.enqueue("flow.owner_notice", vec![Ok(json!({"accepted":true}))]);
    assert_eq!(
        host.owner_notice(request.clone()).unwrap(),
        json!({"accepted":true})
    );
    assert_eq!(
        fake.calls("flow.owner_notice"),
        vec![json!({"method":"flow.owner_notice","params":request})]
    );
    assert!(
        !basal_host::core_host::is_core_carrier_op("prefrontal-core", "flow.owner_notice"),
        "runtime notices must not become manifest-declared script carrier calls"
    );
    set_core_capability(&fake, capability, false);
    assert!(!host.supports_core_capability(capability));
}
