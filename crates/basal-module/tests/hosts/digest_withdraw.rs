use super::*;

const CAPABILITY: &str = "flow-digest-withdraw/v1";

fn vector(name: &str) -> Value {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/flow-digest-withdraw-v1");
    serde_json::from_slice(&std::fs::read(root.join(name)).unwrap()).unwrap()
}

fn withdrawal_args() -> Value {
    json!({"agent":"ag_test","withdraw":true,"options":{"replace_key":"daily-report"}})
}

#[test]
fn withdrawal_vectors_are_pinned_and_match_sink_digest_wire_bytes() {
    use sha2::{Digest, Sha256};

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/flow-digest-withdraw-v1");
    let sums = std::fs::read_to_string(root.join("SHA256SUMS")).unwrap();
    let mut count = 0;
    for line in sums.lines() {
        let (hash, name) = line.split_once("  ").unwrap();
        let bytes = std::fs::read(root.join(name)).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), hash, "{name}");
        count += 1;
    }
    assert_eq!(count, 3);
    assert_eq!(CAPABILITY, basal_host::core_host::FLOW_DIGEST_WITHDRAW_V1);

    let expected = vector("request.json");
    let intent = intent(
        Primitive::SinkDigest,
        &withdrawal_args(),
        IntentContext {
            flow_id: "flow-x",
            version: 1,
            run_id: "withdraw-vector",
            position: 2,
            due_at: 0,
            created_at: 0,
        },
    );
    assert_eq!(intent, expected);
    assert_eq!(
        intent.to_string().as_bytes(),
        expected.to_string().as_bytes()
    );

    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY, true);
    fake.enqueue("sink.digest", vec![Ok(vector("reply.json"))]);
    let host = CoreHost::new(fake.clone());
    let Dispatched::Completed(outcome) = host
        .dispatch(&call(Primitive::SinkDigest, expected.clone()))
        .unwrap()
    else {
        panic!("withdrawal completed synchronously")
    };
    assert_eq!(outcome.settlement, Settlement::Fulfilled);
    assert_eq!(
        serde_json::from_str::<Value>(outcome.value.as_str()).unwrap(),
        vector("reply.json")
    );
    assert_eq!(
        fake.calls("sink.digest").last().unwrap()["params"]
            .to_string()
            .as_bytes(),
        expected.to_string().as_bytes()
    );
}

#[test]
fn digest_replies_accept_only_the_dispositions_for_the_request_kind() {
    for disposition in ["withdrawn", "already_delivered", "not_found"] {
        let reply = json!({"disposition":disposition,"fire_id":null,"replayed":false});
        assert_eq!(
            validate_reply(Primitive::SinkDigest, reply.clone(), true).unwrap(),
            reply
        );
        assert!(validate_reply(Primitive::SinkDigest, reply, false).is_err());
    }

    for (disposition, fire_id) in [
        ("stored", json!("wf_1")),
        ("claim_in_fallback", Value::Null),
        ("episode_ended", Value::Null),
    ] {
        let reply = json!({"disposition":disposition,"fire_id":fire_id,"replayed":false});
        assert_eq!(
            validate_reply(Primitive::SinkDigest, reply.clone(), false).unwrap(),
            reply
        );
        assert!(validate_reply(Primitive::SinkDigest, reply, true).is_err());
    }

    for reply in [
        json!({"disposition":"withdrawn","fire_id":"wf_1","replayed":false}),
        json!({"disposition":"not_found","fire_id":null,"replayed":"no"}),
        json!({"disposition":"unknown","fire_id":null,"replayed":false}),
    ] {
        assert!(validate_reply(Primitive::SinkDigest, reply.clone(), true).is_err());
        assert!(validate_reply(Primitive::SinkDigest, reply, false).is_err());
    }
}

#[test]
fn withdrawal_dispatch_is_refused_without_core_capability() {
    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY, false);
    let host = CoreHost::new(fake.clone());
    let Dispatched::Completed(outcome) = host
        .dispatch(&call(Primitive::SinkDigest, vector("request.json")))
        .unwrap()
    else {
        panic!("withdrawal capability refusal completed synchronously")
    };
    assert_eq!(outcome.settlement, Settlement::Rejected);
    assert_eq!(
        serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
        "digest_withdraw_unsupported"
    );
    assert!(fake.calls("sink.digest").is_empty());
}

#[test]
fn digest_withdraw_requires_core_catalog_capability() {
    let fake = Fake::new();
    set_core_digest_replace_key_capability(&fake, true);
    set_core_capability(&fake, CAPABILITY, false);
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent.clone(), "withdraw-unsupported");
    common::install(
        &f,
        &Caller::Operator,
        "try { await sink.digest.withdraw('SYNAPSE',{replace_key:'daily-report'}); } catch (e) { return e.data.code; } return 'sent';",
        &manifest(),
    );
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "withdraw-unsupported");
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded);
    assert_eq!(
        done.result.as_deref(),
        Some("\"digest_withdraw_unsupported\"")
    );
    assert!(fake.calls("sink.digest").is_empty());
    let calls = f.module.rt.calls(&run).unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].request.is_none());
}

#[test]
fn digest_withdraw_validates_required_keys_and_the_digest_grant() {
    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY, true);
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent.clone(), "withdraw-validation");
    common::install(
        &f,
        &Caller::Operator,
        "const check = async (agent, options) => { try { await sink.digest.withdraw(agent, options); return 'sent'; } catch (e) { return e.data.code; } }; return [await check('SYNAPSE',{}), await check('SYNAPSE',{replace_key:''}), await check('SYNAPSE',{replace_key:'x'+String.fromCharCode(10)}), await check('OTHER',{replace_key:'daily-report'})];",
        &manifest(),
    );
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "withdraw-validation");
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded);
    assert_eq!(
        done.result.as_deref(),
        Some("[\"invalid_arguments\",\"invalid_arguments\",\"invalid_arguments\",\"denied\"]")
    );
    assert!(fake.calls("sink.digest").is_empty());
    assert!(
        f.module
            .rt
            .calls(&run)
            .unwrap()
            .iter()
            .all(|call| call.request.is_none())
    );
}

#[test]
fn withdrawal_journals_before_send_and_replays_identical_intent_after_cut() {
    use basal_core::{Clock, Config, Durability};
    use basal_module::{
        dryrun::DryRunConfig,
        engine::EngineConfig,
        module::{Module, ModuleConfig},
        pool::{PoolConfig, ProcessSpawner},
    };
    use basal_testkit::harness::{Point, Probe};

    let dir = common::scratch("digest-withdraw-cut");
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
    set_core_digest_replace_key_capability(&fake, true);
    set_core_capability(&fake, CAPABILITY, true);
    fake.enqueue(
        "sink.digest",
        vec![
            Ok(vector("reply.json")),
            Ok(json!({"disposition":"withdrawn","fire_id":null,"replayed":true})),
        ],
    );
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
            json!({"script":"await sink.digest.withdraw('SYNAPSE',{replace_key:'daily-report'}); return 1;","manifest":manifest().to_string()}),
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
    assert!(module.engine.run_until_idle(50).is_err());
    module.rt.quiesce();
    module.pool.stop();
    assert!(probe.fired());
    let connection = rusqlite::Connection::open_with_flags(
        &config.store_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let stored_text: String = connection
        .query_row(
            "SELECT request FROM journal WHERE run_id=?1 AND position=0",
            [&run],
            |row| row.get(0),
        )
        .unwrap();
    let stored: Value = serde_json::from_str(&stored_text).unwrap();
    assert_eq!(stored["withdraw"], true);
    assert_eq!(stored["replace_key"], "daily-report");
    assert!(stored.get("item").is_none());
    assert_eq!(fake.calls("sink.digest").len(), 1);

    consent.attach(Arc::new(Decisions::default()));
    drop(module);
    clock.set(common::T0 + 10_000);
    let module = Module::start(
        config,
        hosts(fake.clone(), consent.clone()),
        Arc::new(ProcessSpawner::new(&pool)),
    )
    .unwrap();
    module.engine.run_until_idle(50).unwrap();
    assert_eq!(module.rt.run(&run).unwrap().state, RunState::Succeeded);
    let sent = fake.calls("sink.digest");
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].to_string().as_bytes(),
        sent[1].to_string().as_bytes()
    );
    let params = &sent[1]["params"];
    assert_eq!(params["withdraw"], true);
    assert_eq!(params["replace_key"], "daily-report");
    assert_eq!(params["flow_id"], "host-flow");
    assert_eq!(params["flow_version"], 1);
    assert_eq!(params["run_id"], run);
    assert_eq!(params["call_position"], 0);
    assert!(params.get("item").is_none());
    assert!(params.get("action").is_none());
    module.pool.stop();
    consent.attach(Arc::new(Decisions::default()));
    drop(module);
    std::fs::remove_dir_all(dir).unwrap();
}
