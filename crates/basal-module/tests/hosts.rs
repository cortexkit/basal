//! Wire fixtures verify request and reply formats without connecting to a daemon.
mod common;
mod wire;
use std::sync::{Arc, Mutex};

use basal_core::{NoHooks, RunState};
use basal_host::core_consent::CoreConsent;
use basal_host::core_host::{CoreHost, intent, validate_reply};
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
        "synapse-xcom-inference-news",
        3,
        "run_01JB4M5N6P7Q8R9S",
        7,
        1790000000000,
        1790000000000,
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
        "synapse-xcom-inference-news",
        3,
        "run",
        7,
        0,
        1790000000000,
    );
    host.dispatch(&call(Primitive::SinkStatus, status)).unwrap();
    assert_eq!(
        f.calls("sink.status")[0]["params"],
        json!({"flow_id":"synapse-xcom-inference-news","flow_version":3,"agent":"SYNAPSE","text":"x.com: 2 new","ttl_ms":1800000,"revision":1790000000000i64})
    );
    let facts = intent(
        Primitive::Facts,
        &json!({"agent":"BASAL","options":{"fields":["identity","residence","activity","attention"],"include":[],"max_age_ms":null}}),
        "dark-wake-basal",
        1,
        "run",
        7,
        0,
        0,
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
#[test]
fn install_card_is_byte_exact_and_session_comes_from_caller() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = fixture(fake.clone(), consent, "card-real");
    let script = "return 1;\n";
    let m = format!("  {}\n", manifest());
    let caller = Caller::Agent {
        agent_id: "SYNAPSE".into(),
        session: "ses-author".into(),
    };
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
    assert_eq!(sent["session_ref"], "ses-author");
    assert_eq!(sent["flow_install"]["author"], json!({"agent":"SYNAPSE"}));
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
            fields: json!({"purpose":"Purpose","wire_author":{"agent":"SYNAPSE"},"session_ref":"ses-author","placement":"machine:ufuk-mbp","warnings":[{"kind":"private_text_with_outbound_op","text":"Private text can leave"}],"dry_run_summary":summary,"code_hash":actual,"code":{"script":script,"manifest":manifest},"token_cap":{"window":"1d"},"token_window":{"input_tokens":11,"cache_write_tokens":12,"output_tokens":13,"cached_input_tokens":14}}),
        };
        let sent = request(&card).unwrap();
        assert_eq!(
            sent,
            json!({"kind":"flow_install","title":"Install flow flow-x v3","prompt":"Purpose","options":[{"id":"approve","label":"Approve","effect":"grant"},{"id":"decline","label":"Decline","effect":"decline"}],"default":"decline","urgency":"normal","on_expiry":"deny","material_damage":false,"late_execution":"notify_only","args_digest":hash,"dedup_key":"flow_install:flow-x:3","session_ref":"ses-author","target":{"kind":"flow","label":"flow-x v3"},"facts":[],"preview":{"label":"Code","text":script},"expires_in_ms":86400000,"flow_install":{"flow_id":"flow-x","version":3,"code_hash":hash,"script":script,"manifest_json":manifest,"author":{"agent":"SYNAPSE"},"placement":"machine:ufuk-mbp","warnings":[{"code":"private_text_with_outbound_op","detail":"Private text can leave"}],"dry_run_summary":summary,"token_usage":{"window":"1d","fresh_input":11,"cache_write":12,"output":13,"cache_read":14}}})
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
