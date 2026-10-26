use super::*;

const CAPABILITY: &str =
    include_str!("../../../../test-vectors/flow-wait-resolve-v1/capability.txt");

fn vector(name: &str) -> Value {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/flow-wait-resolve-v1");
    serde_json::from_slice(&std::fs::read(root.join(name)).unwrap()).unwrap()
}

fn wait_manifest() -> Value {
    let mut manifest = json!({"id":"host-flow","version":1,"purpose":"Resolve waits",
        "trigger":{"schedule":{"interval":"1h"}}});
    manifest["sinks"] = vector("manifest-grant.json")["sinks"].clone();
    manifest
}

#[test]
fn wait_vectors_are_byte_pinned() {
    use sha2::{Digest, Sha256};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/flow-wait-resolve-v1");
    let sums = std::fs::read_to_string(root.join("SHA256SUMS")).unwrap();
    let mut count = 0;
    for line in sums.lines() {
        let (hash, name) = line.split_once("  ").unwrap();
        let bytes = std::fs::read(root.join(name)).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), hash, "{name}");
        count += 1;
    }
    assert_eq!(count, 11);
    assert_eq!(
        CAPABILITY.trim(),
        basal_host::core_host::FLOW_WAIT_RESOLVE_V1
    );
}

#[test]
fn wait_wire_matches_met_failed_and_all_reply_vectors() {
    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let host = CoreHost::new(fake.clone());
    for (request, reply) in [
        ("request-met.json", "reply-met.json"),
        ("request-failed.json", "reply-noop.json"),
        ("request-met.json", "reply-replay.json"),
    ] {
        let expected = vector(request);
        fake.enqueue("sink.wait_resolve", vec![Ok(vector(reply))]);
        let mut request = call(Primitive::SinkWaitResolve, expected.clone());
        request.idempotency_key = expected["receipt_key"].as_str().unwrap().into();
        let Dispatched::Completed(outcome) = host.dispatch(&request).unwrap() else {
            panic!("completed")
        };
        assert_eq!(outcome.settlement, Settlement::Fulfilled);
        assert_eq!(
            outcome.value.as_str().as_bytes(),
            vector(reply).to_string().as_bytes()
        );
        // Both values use the transport's JSON serializer; only object key
        // order and the vector file's final LF are normalized.
        assert_eq!(
            fake.calls("sink.wait_resolve").last().unwrap()["params"]
                .to_string()
                .as_bytes(),
            expected.to_string().as_bytes()
        );
    }
    assert_eq!(
        host.classify(&CallKind::Primitive(Primitive::SinkWaitResolve)),
        basal_host::CallClass::Mutation {
            honours_idempotency_keys: true
        }
    );
    assert_eq!(
        basal_host::op_label(&CallKind::Primitive(Primitive::SinkWaitResolve)),
        "prefrontal-core.sink.wait_resolve"
    );
}

#[test]
fn wait_dispatch_gate_refuses_without_core_capability() {
    let fake = Fake::new();
    fake.enqueue("sink.wait_resolve", vec![Ok(vector("reply-met.json"))]);
    let host = CoreHost::new(fake.clone());
    let Dispatched::Completed(outcome) = host
        .dispatch(&call(
            Primitive::SinkWaitResolve,
            vector("request-met.json"),
        ))
        .unwrap()
    else {
        panic!("completed")
    };
    assert_eq!(outcome.settlement, Settlement::Rejected);
    assert_eq!(
        serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
        "wait_resolve_unsupported"
    );
    assert!(fake.calls("sink.wait_resolve").is_empty());
}

#[test]
fn wait_manifest_rejects_targeted_grants_and_missing_op_grant_at_install() {
    use basal_core::manifest::Manifest;
    let m = wait_manifest();
    let parsed = Manifest::parse(&m.to_string()).unwrap();
    assert!(parsed.wait_sink());
    assert_eq!(serde_json::to_value(&parsed).unwrap()["sinks"], m["sinks"]);
    assert_eq!(parsed.digest_sinks().count(), 0);
    assert!(parsed.agents().is_empty());
    for grant in [
        json!({"kind":"wait","targets":[]}),
        json!({"kind":"wait","agent":"SYNAPSE"}),
        json!({"kind":"other"}),
    ] {
        let fake = Fake::new();
        let f = worker_fixture(
            fake.clone(),
            Arc::new(CoreConsent::new(fake.clone())),
            "wait-install",
        );
        let mut m = m.clone();
        m["sinks"] = json!([grant]);
        assert!(
            f.module
                .handle(
                    &Caller::Operator,
                    "flow.install",
                    json!({"script":"return 1;", "manifest":m.to_string()})
                )
                .is_err()
        );
        assert!(fake.calls("flow.install_request").is_empty());
    }
    let mut missing = m.clone();
    missing["sinks"] = json!([]);
    missing["ops"] = json!([{"module":"prefrontal-core","op":"sink.wait_resolve"}]);
    assert!(Manifest::parse(&missing.to_string()).is_err());
    let fake = Fake::new();
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        "wait-missing-op-grant",
    );
    assert!(
        f.module
            .handle(
                &Caller::Operator,
                "flow.install",
                json!({"script":"return 1;", "manifest":missing.to_string()}),
            )
            .is_err()
    );
    assert!(fake.calls("flow.install_request").is_empty());
    let mut duplicate = m;
    duplicate["sinks"] = json!([{"kind":"wait"},{"kind":"wait"}]);
    assert!(Manifest::parse(&duplicate.to_string()).is_err());
}

#[test]
fn wait_script_gate_and_missing_grant_are_journaled_unsent() {
    for (grant, capability, code) in [
        (true, false, "wait_resolve_unsupported"),
        (false, true, "sink_wait_not_granted"),
    ] {
        let fake = Fake::new();
        set_core_capability(&fake, CAPABILITY.trim(), capability);
        let f = worker_fixture(
            fake.clone(),
            Arc::new(CoreConsent::new(fake.clone())),
            "wait-unsent",
        );
        let mut m = wait_manifest();
        if !grant {
            m["sinks"] = json!([]);
        }
        common::install(
            &f,
            &Caller::Operator,
            "return await sink.wait.resolve('met').catch(e => e.data.code);",
            &m,
        );
        approve(&fake, &f.module);
        let run = common::admit(&f, "host-flow", "wait-unsent");
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(
            f.module.rt.run(&run).unwrap().result.as_deref(),
            Some(json!(code).to_string().as_str())
        );
        assert!(fake.calls("sink.wait_resolve").is_empty());
        let row = &f.module.rt.calls(&run).unwrap()[0];
        assert_eq!(row.attempts, 0);
        assert_eq!(row.dispatch, basal_core::DispatchState::None);
    }
}

#[test]
fn wait_terminal_refusals_reach_script_and_store_failure_is_retryable() {
    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let host = CoreHost::new(fake.clone());
    for refusal in vector("refusals.json").as_array().unwrap() {
        let code = refusal["code"].as_str().unwrap();
        for detailed in [false, true] {
            let error = if detailed {
                WireError::RefusedDetails {
                    code: code.into(),
                    message: "no".into(),
                    detail: json!({"case":refusal["case"]}),
                }
            } else {
                WireError::Refused {
                    code: code.into(),
                    message: "no".into(),
                }
            };
            fake.enqueue("sink.wait_resolve", vec![Err(error)]);
            let Dispatched::Completed(outcome) = host
                .dispatch(&call(
                    Primitive::SinkWaitResolve,
                    vector("request-met.json"),
                ))
                .unwrap()
            else {
                panic!("terminal")
            };
            assert_eq!(outcome.settlement, Settlement::Rejected);
            assert_eq!(
                serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
                code
            );
        }
    }
    fake.enqueue(
        "sink.wait_resolve",
        vec![Err(WireError::Refused {
            code: "sink_store_failed".into(),
            message: "temporary".into(),
        })],
    );
    let Err(basal_host::TransportError::Refused(refusal)) = host.dispatch(&call(
        Primitive::SinkWaitResolve,
        vector("request-met.json"),
    )) else {
        panic!("retryable")
    };
    assert!(!refusal.terminal());
    assert_eq!(refusal.reason.as_str(), "sink_store_failed");
    assert_eq!(refusal.action, "sink.wait_resolve");
    for reply in [
        json!({}),
        json!({"resolved":-1,"replayed":false}),
        json!({"resolved":1,"replayed":"false"}),
        json!({"resolved":1,"replayed":false,"extra":true}),
    ] {
        fake.enqueue("sink.wait_resolve", vec![Ok(reply)]);
        assert!(matches!(
            host.dispatch(&call(
                Primitive::SinkWaitResolve,
                vector("request-met.json")
            )),
            Err(basal_host::TransportError::Unavailable {
                sent: basal_host::Sent::Maybe(_),
                ..
            })
        ));
    }
}

#[test]
fn wait_script_stamps_met_and_failed_without_script_identity() {
    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        "wait-script",
    );
    common::install(
        &f,
        &Caller::Operator,
        "await sink.wait.resolve('met','CI is green'); return await sink.wait.resolve('failed');",
        &wait_manifest(),
    );
    approve(&fake, &f.module);
    fake.enqueue(
        "sink.wait_resolve",
        vec![Ok(vector("reply-met.json")), Ok(vector("reply-noop.json"))],
    );
    let run = common::admit(&f, "host-flow", "wait-script");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
    let hash = fake.calls("flow.install_request")[0]["params"]["flow_install"]["code_hash"].clone();
    let rows = f.module.rt.calls(&run).unwrap();
    let sent = fake.calls("sink.wait_resolve");
    assert_eq!(sent.len(), 2);
    for (position, name) in ["request-met.json", "request-failed.json"]
        .into_iter()
        .enumerate()
    {
        let mut expected = vector(name);
        expected["stamp"]["flow_id"] = json!("host-flow");
        expected["stamp"]["code_hash"] = hash.clone();
        expected["receipt_key"] = json!(basal_core::ids::idempotency_key(
            "host-flow",
            &run,
            position as u64
        ));
        assert_eq!(
            sent[position]["params"].to_string().as_bytes(),
            expected.to_string().as_bytes()
        );
        expected.as_object_mut().unwrap().remove("receipt_key");
        assert_eq!(
            rows[position].request.as_ref().unwrap().as_str(),
            expected.to_string()
        );
        assert_eq!(rows[position].class, basal_core::StoredClass::KeyedMutation);
    }
    assert_eq!(
        serde_json::from_str::<Value>(
            fake.calls("flow.install_request")[0]["params"]["flow_install"]["manifest_json"]
                .as_str()
                .unwrap()
        )
        .unwrap()["sinks"],
        wait_manifest()["sinks"]
    );
}

#[test]
fn wait_invalid_arguments_are_unsent() {
    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        "wait-invalid",
    );
    let script = "const values=[['done'],['met','line\\nbreak'],['met','é'.repeat(251)],['met',null],['met',{flow_id:'other'}]]; const codes=[]; for (const args of values) { codes.push(await sink.wait.resolve(...args).catch(e => e.data.code)); } return codes;";
    common::install(&f, &Caller::Operator, script, &wait_manifest());
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "wait-invalid");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(f.module.rt.run(&run).unwrap().result.as_ref().unwrap())
            .unwrap(),
        json!(vec!["sink_item_invalid"; 5])
    );
    assert!(fake.calls("sink.wait_resolve").is_empty());
    let manifest = basal_core::manifest::Manifest::parse(&wait_manifest().to_string()).unwrap();
    assert_eq!(
        basal_core::authorize::check(
            &manifest,
            &basal_core::authorize::ShellDenylist::default(),
            f.module.rt.catalog(),
            &CallKind::Primitive(Primitive::SinkWaitResolve),
            &json!({"outcome":"met","flow_id":"other"})
        )
        .unwrap_err()
        .code,
        "sink_item_invalid"
    );
}

#[test]
fn wait_store_refusal_defers_and_reuses_frozen_receipt() {
    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        "wait-retry",
    );
    common::install(
        &f,
        &Caller::Operator,
        "return await sink.wait.resolve('met');",
        &wait_manifest(),
    );
    approve(&fake, &f.module);
    fake.enqueue(
        "sink.wait_resolve",
        vec![
            Err(WireError::Refused {
                code: "sink_store_failed".into(),
                message: "temporary".into(),
            }),
            Ok(vector("reply-met.json")),
        ],
    );
    let run = common::admit(&f, "host-flow", "wait-retry");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(
        f.module.rt.calls(&run).unwrap()[0].dispatch,
        basal_core::DispatchState::Deferred
    );
    let retry: i64 = f
        .module
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
    f.clock.set(retry);
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
    let sent = fake.calls("sink.wait_resolve");
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].to_string().as_bytes(),
        sent[1].to_string().as_bytes()
    );
}

#[test]
fn wait_dry_run_would_resolve_but_never_sends() {
    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        "wait-dry",
    );
    let m = wait_manifest();
    common::install(
        &f,
        &Caller::Operator,
        "await sink.wait.resolve('met').catch(()=>null); await sink.wait.resolve('failed').catch(()=>null);",
        &m,
    );
    for mode in ["capture", "live"] {
        let result = f
            .module
            .handle(
                &Caller::Operator,
                "flow.dry_run",
                json!({"flow_id":"host-flow","mode":mode,"trigger":{"kind":"test"}}),
            )
            .unwrap();
        let summary = &result["summary"];
        let calls = summary["runs"][0]["calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        for call in calls {
            assert_eq!(call["kind"], "sink.wait.resolve");
            assert_eq!(call["action"], "captured");
            assert_eq!(call["outcome"]["rejected"], "captured");
        }
        let parsed = basal_core::manifest::Manifest::parse(&m.to_string()).unwrap();
        let text = basal_core::dry_run_text::render(&parsed, summary);
        assert_eq!(
            text.would,
            [
                "Resolve its board waits as 'met'",
                "Resolve its board waits as 'failed'"
            ]
        );
        assert!(text.problems.is_empty(), "{text:?}");
        assert!(!text.summary.contains("stopped at"));
        assert!(fake.calls("sink.wait_resolve").is_empty());
    }
}

#[test]
fn wait_journals_before_send_and_replays_identical_intent_after_cut() {
    use basal_core::{Clock, Config, Durability};
    use basal_module::{
        dryrun::DryRunConfig,
        engine::EngineConfig,
        module::{Module, ModuleConfig},
        pool::{PoolConfig, ProcessSpawner},
    };
    use basal_testkit::harness::{Point, Probe};
    for (point, sent_before_cut) in [
        ("CallCommitted { position: 0 }#1", 0),
        ("HostAnswered { position: 0 }#1", 1),
    ] {
        let dir = common::scratch("wait-cut");
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
        set_core_capability(&fake, CAPABILITY.trim(), true);
        let consent = Arc::new(CoreConsent::new(fake.clone()));
        let probe = Arc::new(Probe::crash_at(Point::parse(point).unwrap()));
        let mut dependencies = hosts(fake.clone(), consent.clone());
        dependencies.hooks = probe.clone();
        let module = Module::start(
            config.clone(),
            dependencies,
            Arc::new(ProcessSpawner::new(&pool)),
        )
        .unwrap();
        module.handle(&Caller::Operator, "flow.install", json!({"script":"return await sink.wait.resolve('met','CI is green');", "manifest":wait_manifest().to_string()})).unwrap();
        approve(&fake, &module);
        fake.enqueue(
            "sink.wait_resolve",
            vec![
                Ok(vector("reply-met.json")),
                Ok(vector("reply-replay.json")),
            ],
        );
        let run = module
            .rt
            .admit_trigger(
                "host-flow",
                "cut",
                JsonText::new(json!({"kind":"test"}).to_string()).unwrap(),
            )
            .unwrap()
            .run_id()
            .unwrap()
            .to_owned();
        assert!(module.engine.run_until_idle(50).is_err());
        module.rt.quiesce();
        module.pool.stop();
        assert!(probe.fired());
        assert_eq!(fake.calls("sink.wait_resolve").len(), sent_before_cut);
        consent.attach(Arc::new(Decisions::default()));
        drop(module);
        clock.set(common::T0 + 10_000);
        let module = Module::start(
            config,
            hosts(fake.clone(), consent),
            Arc::new(ProcessSpawner::new(&pool)),
        )
        .unwrap();
        let row = module.rt.calls(&run).unwrap().remove(0);
        let frozen = row
            .request
            .expect("wait intent must be durable before transport can send");
        let value: Value = serde_json::from_str(frozen.as_str()).unwrap();
        assert_eq!(value["outcome"], "met");
        assert_eq!(value["note"], "CI is green");
        assert_eq!(value["stamp"]["flow_id"], "host-flow");
        assert_eq!(value["stamp"]["flow_version"], 1);
        assert_eq!(row.class, basal_core::StoredClass::KeyedMutation);
        module.engine.run_until_idle(50).unwrap();
        let done = module.rt.run(&run).unwrap();
        assert_eq!(done.state, RunState::Succeeded);
        assert_eq!(
            serde_json::from_str::<Value>(done.result.as_ref().unwrap()).unwrap(),
            vector(if sent_before_cut == 0 {
                "reply-met.json"
            } else {
                "reply-replay.json"
            })
        );
        let sent = fake.calls("sink.wait_resolve");
        assert_eq!(sent.len(), sent_before_cut + 1);
        let mut expected = value;
        expected["receipt_key"] = json!(row.idempotency_key);
        for call in &sent {
            assert_eq!(
                call["params"].to_string().as_bytes(),
                expected.to_string().as_bytes()
            );
        }
        assert_eq!(
            module.rt.calls(&run).unwrap()[0].request.as_ref(),
            Some(&frozen)
        );
        module.rt.quiesce();
        module.pool.stop();
        drop(module);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
