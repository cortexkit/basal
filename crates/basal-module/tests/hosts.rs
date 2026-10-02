//! Wire fixtures verify request and reply formats without connecting to a daemon.
mod common;
mod wire;
use std::sync::{Arc, Mutex};

use basal_core::{NoHooks, RunState};
use basal_host::core_consent::CoreConsent;
use basal_host::core_host::{CoreHost, IntentContext, intent, validate_reply};
use basal_host::routing::{ModuleOpsHost, RoutingHost};
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{WireError, map_error, tool_body};
use basal_host::{
    CallRequest, CardDecision, Consent, DecisionEvent, DecisionSink, Dispatched, Host, SinkError,
};
use basal_module::caller::Caller;
use basal_module::module::Hosts;
use basal_module::unconfigured::UnconfiguredHost;
use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use serde_json::{Value, json};
use wire::Fake;

fn call(p: Primitive, args: Value) -> CallRequest {
    CallRequest {
        flow_id: "flow".into(),
        run_id: "run".into(),
        position: 7,
        kind: CallKind::Primitive(p),
        args: JsonText::new(args.to_string()).unwrap(),
        idempotency_key: "key-7".into(),
        attempt: 1,
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
        assert_eq!(validate_reply(Primitive::SinkDigest, v.clone()).unwrap(), v);
    }
    for v in [
        json!({"disposition":"published","accepted_revision":1,"segment":"flow:f","scope":"session:s","replayed":false}),
        json!({"disposition":"no_live_session","accepted_revision":null,"segment":null,"scope":null,"replayed":true}),
    ] {
        assert_eq!(validate_reply(Primitive::SinkStatus, v.clone()).unwrap(), v);
    }
    for p in [Primitive::SinkDigest, Primitive::SinkStatus] {
        assert!(matches!(
            validate_reply(
                p,
                json!({"disposition":"new_value","fire_id":"wf_1","replayed":false,"accepted_revision":1,"segment":"flow:f","scope":"session:s"})
            ),
            Err(WireError::Unknown(_))
        ));
        assert!(validate_reply(p, json!({})).is_err());
    }
    assert!(validate_reply(Primitive::SinkStatus,json!({"disposition":"published","accepted_revision":null,"segment":"flow:f","scope":null})).is_err());
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
    common::fixture(
        tag,
        common::Options {
            hosts: Some(hosts(f, consent)),
            ..Default::default()
        },
    )
}
fn approve(f: &Fake, consent: &CoreConsent) {
    let flow = f.calls("elicitation.request")[0]["params"]["flow_install"].clone();
    f.enqueue("elicitation.answers",vec![Ok(json!({"records":[{"state":"answered","answered_choice_id":"approve","flow_install":flow}],"cursor":1}))]);
    consent.poll_once().unwrap();
}
#[test]
fn catalog_install_refuses_missing_ops_and_events_and_marks_mutations() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = fixture(fake.clone(), consent, "catalog-real");
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
    assert!(cat.agent_known("ag_synapse"));
    assert!(cat.agent_known("SYNAPSE"));
    assert!(!cat.agent_known("unknown"));
    common::install(&f, &common::agent("SYNAPSE"), "return 1;", &manifest());
    let cards = f.module.rt.cards("host-flow").unwrap();
    let fields: Value = serde_json::from_str(&cards[0].card).unwrap();
    assert_eq!(fields["ops"][1]["kind"], "mutate");
}
/// The caller basal decides for a route under core's owner-authorized
/// session scope `scope_ref`, naming `agent`, as the daemon stamps it.
fn agent_on_scope(agent: &str, scope_ref: &str) -> Caller {
    use subc_protocol::Principal;
    use subc_protocol::scope::{ScopeAttributes, ScopeKind, ScopeStamp};
    let stamp = ScopeStamp {
        owner: Principal::Reserved {
            module_id: basal_module::caller::CORE_MODULE.into(),
        },
        scope_ref: scope_ref.into(),
        scope_epoch: 1,
        kind: ScopeKind::Head,
        parent: None,
        parent_state: None,
        attributes: ScopeAttributes {
            agent_id: Some(agent.into()),
            delegates: false,
        },
        owner_authorized: true,
    };
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
    let caller = agent_on_scope("SYNAPSE", stamped);
    f.module
        .handle(
            &caller,
            "flow.install",
            json!({"script":script,"manifest":m}),
        )
        .unwrap();
    let sent = fake.calls("elicitation.request")[0]["params"].clone();
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
    let operator = fake.calls("elicitation.request")[1]["params"].clone();
    assert!(operator.get("session_ref").is_none());
    assert_eq!(operator["flow_install"]["author"], json!({"operator":true}));
    assert_eq!(
        operator["flow_install"]["token_usage"],
        json!({"window":"1d","fresh_input":0,"cache_write":0,"output":0,"cache_read":0})
    );
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
    let calls = fake.calls("elicitation.request");
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
    assert_eq!(fake.calls("elicitation.request").len(), 1, "nothing sent");

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
        let r = install_as(&agent_on_scope("SYNAPSE", scope_ref));
        let e = r.expect_err("refused");
        assert_eq!(e.code, code, "{scope_ref}: {e:?}");
        assert!(e.message.contains("live agent session"), "{e:?}");
    }
    assert_eq!(f.module.rt.cards("host-flow").unwrap().len(), 1);

    // Installing again from a route under a live scope sends the existing
    // card with that live scope as its author, rather than the ended scope
    // recorded when the card was first created.
    let reply = install_as(&agent_on_scope("SYNAPSE", "scope-live")).expect("raised");
    assert_eq!(reply["state"], "pending");
    let calls = fake.calls("elicitation.request");
    let last = &calls.last().expect("sent")["params"];
    assert_eq!(
        last["flow_install"]["author"],
        json!({ "scope": "scope-live" })
    );
}

#[derive(Default)]
struct Decisions {
    events: Mutex<Vec<DecisionEvent>>,
    fail: Mutex<bool>,
}
impl DecisionSink for Decisions {
    fn decide(&self, d: &DecisionEvent) -> Result<(), SinkError> {
        if *self.fail.lock().unwrap() {
            return Err(SinkError("disk cut".into()));
        }
        self.events.lock().unwrap().push(d.clone());
        Ok(())
    }
}
#[test]
fn decisions_ack_only_after_sink_commit_and_survive_restart() {
    let fake = Fake::new();
    let sink = Arc::new(Decisions::default());
    *sink.fail.lock().unwrap() = true;
    let record = json!({"state":"answered","answered_choice_id":"approve","flow_install":{"flow_id":"f","version":1,"code_hash":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}});
    let page = json!({"records":[record],"cursor":9});
    fake.enqueue("elicitation.answers", vec![Ok(page.clone()), Ok(page)]);
    let consent = CoreConsent::new(fake.clone());
    consent.attach(sink.clone());
    assert!(consent.poll_once().is_err());
    assert!(fake.calls("elicitation.ack").is_empty());
    drop(consent);
    *sink.fail.lock().unwrap() = false;
    let restarted = CoreConsent::new(fake.clone());
    restarted.attach(sink.clone());
    restarted.poll_once().unwrap();
    assert_eq!(
        sink.events.lock().unwrap()[0],
        DecisionEvent {
            card_id: "card:f:v1:0123456789abcdef".into(),
            decision: CardDecision::Approve,
            decided_by: "core:elicitation".into()
        }
    );
    assert_eq!(
        fake.calls("elicitation.ack")[0]["params"],
        json!({"cursor":9})
    );
}
#[test]
fn routing_host_runs_install_decision_facts_ops_and_sinks_end_to_end() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = fixture(fake.clone(), consent.clone(), "routing-e2e");
    common::install(
        &f,
        &common::agent("SYNAPSE"),
        "const a=await facts('SYNAPSE'); const b=await ops.call('mock','echo',{n:2}); await sink.digest('SYNAPSE',{title:'facts',body:'unknown'},'piggyback'); const s=await sink.status('SYNAPSE','ready'); return {state:a.activity.state.status,n:b.n,status:s.disposition};",
        &manifest(),
    );
    approve(&fake, &consent);
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
        let f = fixture(fake.clone(), consent.clone(), op);
        common::install(
            &f,
            &Caller::Operator,
            &format!("await ops.call('mock','{op}',{{n:1}});return 1;"),
            &manifest(),
        );
        approve(&fake, &consent);
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
    let f = fixture(fake.clone(), consent.clone(), "sink-retry");
    common::install(
        &f,
        &Caller::Operator,
        "try { await sink.digest('SYNAPSE',{title:'x',body:'y'},'piggyback'); await ops.call('mock','post',{}); } catch(e) { return e.data.code; } return 'wrong';",
        &manifest(),
    );
    approve(&fake, &consent);
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
    for (op, script) in [
        (
            "sink.digest",
            "await sink.digest('SYNAPSE',{title:'x',body:'y'},'piggyback'); return 1;",
        ),
        (
            "sink.status",
            "await sink.status('SYNAPSE','ready'); return 1;",
        ),
        ("agent.facts", "await facts('SYNAPSE'); return 1;"),
    ] {
        let dir = common::scratch("core-cut");
        let clock = Clock::manual(common::T0);
        let pool = PoolConfig::new(basal_testkit::channel::worker_binary());
        let config = ModuleConfig {
            store_path: dir.join("basal.db"),
            durability: Durability { fullfsync: false },
            runtime: Config {
                clock: clock.clone(),
                ..Default::default()
            },
            pool: pool.clone(),
            engine: EngineConfig::default(),
            dry_run: DryRunConfig::new(dir.join("dry")),
        };
        let fake = Fake::new();
        let consent = Arc::new(CoreConsent::new(fake.clone()));
        let probe = Arc::new(Probe::crash_at(
            Point::parse("HostAnswered { position: 0 }#1").unwrap(),
        ));
        let mut dependencies = hosts(fake.clone(), consent.clone());
        dependencies.hooks = probe.clone();
        let module = Module::start(
            config.clone(),
            dependencies,
            Arc::new(ProcessSpawner::new(&pool)),
        )
        .unwrap();
        module
            .handle(
                &Caller::Operator,
                "flow.install",
                json!({"script":script,"manifest":manifest().to_string()}),
            )
            .unwrap();
        approve(&fake, &consent);
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
        assert!(module.engine.run_until_idle(50).is_err());
        module.rt.quiesce();
        module.pool.stop();
        assert!(probe.fired());
        consent.attach(Arc::new(Decisions::default()));
        drop(module);
        clock.set(common::T0 + 10000);
        let module = Module::start(
            config,
            hosts(fake.clone(), consent.clone()),
            Arc::new(ProcessSpawner::new(&pool)),
        )
        .unwrap();
        module.engine.run_until_idle(50).unwrap();
        assert_eq!(module.rt.run(&run).unwrap().state, RunState::Succeeded);
        let sent = fake.calls(op);
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[0].to_string().as_bytes(),
            sent[1].to_string().as_bytes()
        );
        if op == "sink.digest" {
            assert_eq!(sent[1]["params"]["created_at"], common::T0);
            assert_eq!(sent[1]["params"]["due_at"], common::T0);
            assert_eq!(sent[1]["params"]["run_id"], run);
            assert_eq!(sent[1]["params"]["call_position"], 0);
        }
        if op == "sink.status" {
            assert_eq!(sent[1]["params"]["revision"], common::T0);
        }
        assert_eq!(sent[1]["params"]["flow_version"], 1);
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
            fields: json!({"purpose":"Purpose","wire_author":{"scope":"5e1f0c3a9b7d4e2f8a6c1b3d5f7e9a0c"},"placement":"machine:ufuk-mbp","warnings":[{"kind":"private_text_with_outbound_op","text":"Private text can leave"}],"dry_run_summary":summary,"code_hash":actual,"code":{"script":script,"manifest":manifest},"token_cap":{"window":"1d"},"token_window":{"input_tokens":11,"cache_write_tokens":12,"output_tokens":13,"cached_input_tokens":14}}),
        };
        let sent = request(&card).unwrap();
        assert_eq!(
            sent,
            json!({"kind":"flow_install","title":"Install flow flow-x v3","prompt":"Purpose","options":[{"id":"approve","label":"Approve","effect":"grant"},{"id":"decline","label":"Decline","effect":"decline"}],"default":"decline","urgency":"normal","on_expiry":"deny","material_damage":false,"late_execution":"notify_only","args_digest":hash,"dedup_key":"flow_install:flow-x:3","target":{"kind":"flow","label":"flow-x v3"},"facts":[],"preview":{"label":"Code","text":script},"expires_in_ms":86400000,"flow_install":{"flow_id":"flow-x","version":3,"code_hash":hash,"script":script,"manifest_json":manifest,"author":{"scope":"5e1f0c3a9b7d4e2f8a6c1b3d5f7e9a0c"},"placement":"machine:ufuk-mbp","warnings":[{"code":"private_text_with_outbound_op","detail":"Private text can leave"}],"dry_run_summary":summary,"token_usage":{"window":"1d","fresh_input":11,"cache_write":12,"output":13,"cache_read":14}}})
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
    assert!(cat.agent_known("ALF"));
    assert_eq!(
        fake.calls("agent.list")[1]["params"],
        json!({"cursor":"page2"})
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
    assert!(!cat.agent_known("ALF"));
}
#[test]
fn decline_and_expiry_are_rejections_and_unrecognised_answers_are_not_acked() {
    let fake = Fake::new();
    let sink = Arc::new(Decisions::default());
    let consent = CoreConsent::new(fake.clone());
    consent.attach(sink.clone());
    for (state, choice) in [("answered", "decline"), ("expired", "")] {
        fake.enqueue("elicitation.answers",vec![Ok(json!({"records":[{"state":state,"answered_choice_id":choice,"flow_install":{"flow_id":"f","version":1,"code_hash":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}}],"cursor":1}))]);
        consent.poll_once().unwrap();
    }
    assert_eq!(sink.events.lock().unwrap().len(), 2);
    assert!(
        sink.events
            .lock()
            .unwrap()
            .iter()
            .all(|e| e.decision == CardDecision::Reject)
    );
    fake.enqueue(
        "elicitation.answers",
        vec![Ok(
            json!({"records":[{"state":"answered","answered_choice_id":"surprise"}],"cursor":2}),
        )],
    );
    assert!(consent.poll_once().is_err());
    assert_eq!(fake.calls("elicitation.ack").len(), 2);
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
        assert_eq!(validate_reply(p, extra.clone()).unwrap(), extra);
        for key in v.as_object().unwrap().keys() {
            let mut missing = v.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                matches!(validate_reply(p, missing), Err(WireError::Unknown(_))),
                "missing {key}: {v}"
            );
            let mut wrong = v.clone();
            wrong[key] = json!({});
            assert!(
                matches!(validate_reply(p, wrong), Err(WireError::Unknown(_))),
                "wrong {key}: {v}"
            );
        }
    }
    for v in [
        json!({"disposition":"stored","fire_id":null,"replayed":false}),
        json!({"disposition":"episode_ended","fire_id":"wf_1","replayed":false}),
    ] {
        assert!(validate_reply(Primitive::SinkDigest, v).is_err());
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
                host: Arc::new(RoutingHost::new(
                    ops,
                    Arc::new(CoreHost::new(fake.clone())),
                    Arc::new(UnconfiguredHost::new()),
                )),
                catalog,
                consent: consent.clone(),
                hooks: Arc::new(NoHooks),
            }),
            ..Default::default()
        },
    );
    common::install(
        &f,
        &Caller::Operator,
        "await ops.call('mock','send',{n:1});return 1;",
        &manifest(),
    );
    approve(&fake, &consent);
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
fn expired_answer_tombstones_read_the_owned_record_before_ack() {
    let fake = Fake::new();
    let sink = Arc::new(Decisions::default());
    let consent = CoreConsent::new(fake.clone());
    consent.attach(sink.clone());
    fake.enqueue("elicitation.answers",vec![Ok(json!({"records":[{"elicitation_id":"el_old","state":"expired","settled_at":10}],"cursor":2}))]);
    fake.enqueue("elicitation.await",vec![Ok(json!({"state":"expired","flow_install":{"flow_id":"old","version":1,"code_hash":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}}))]);
    consent.poll_once().unwrap();
    assert_eq!(
        fake.calls("elicitation.await")[0]["params"],
        json!({"elicitation_id":"el_old","timeout_ms":0})
    );
    assert_eq!(
        sink.events.lock().unwrap()[0].decision,
        CardDecision::Reject
    );
    assert_eq!(
        fake.calls("elicitation.ack")[0]["params"],
        json!({"cursor":2})
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
            "elicitation.request",
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
        fake.enqueue("elicitation.request", vec![Err(e)]);
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
            ..Default::default()
        },
    );
    common::install(
        &f,
        &Caller::Operator,
        "try { await ops.call('mock','echo',{}); } catch(e) { return e.data.code; } return 'sent';",
        &manifest(),
    );
    approve(&fake, &consent);
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
            validate_reply(Primitive::Facts, missing),
            Err(WireError::Unknown(_))
        ));
        let mut wrong = reply.clone();
        wrong[key] = json!({});
        assert!(matches!(
            validate_reply(Primitive::Facts, wrong),
            Err(WireError::Unknown(_))
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
            ..Default::default()
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
        fake.watches().len(),
        1,
        "accepted model call must begin watching its session after its handle commits"
    );
    assert_eq!(
        snapshots.load().unwrap()[0].envelope,
        call.request.unwrap().as_str()
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
        let f = fixture(wire.clone(), consent.clone(), "digest-default");
        let mut m = manifest();
        m["sinks"][0]["digest_max"] = json!(cap);
        common::install(
            &f,
            &Caller::Operator,
            "await sink.digest('SYNAPSE',{title:'x'});return 1;",
            &m,
        );
        approve(&wire, &consent);
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
        let pool = PoolConfig::new(basal_testkit::channel::worker_binary());
        let config = ModuleConfig {
            store_path: dir.join("basal.db"),
            durability: Durability { fullfsync: false },
            runtime: Config {
                clock: Clock::manual(common::T0),
                selector: selector.clone(),
                retry_backoff: std::time::Duration::ZERO,
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
