use basal_core::broca::BrocaStore;
use basal_core::tokens::Usage as LedgerUsage;
use basal_core::{ActivationEnd, Config, DispatchState, NoHooks, RunState, Runtime, Store};
use basal_host::broca::{
    BrocaHost, StateStore,
    fake::FakeBroca,
    wire::{RunFinishReason, Usage},
};
use basal_host::{Completion, CompletionAck, HostOutcome, TokenUsage};
use basal_proto::JsonText;
use basal_testkit::harness::{World, test_manifest};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn setup(world: &World) -> (Runtime, Arc<BrocaHost>, Arc<FakeBroca>, Arc<BrocaStore>) {
    let fake = Arc::new(FakeBroca::default());
    let (rt, host, snapshots) = open(world, fake.clone());
    (rt, host, fake, snapshots)
}
fn open(world: &World, fake: Arc<FakeBroca>) -> (Runtime, Arc<BrocaHost>, Arc<BrocaStore>) {
    let config = Config {
        selector: world.selector.clone(),
        auto_resume: false,
        activation_deadline: Duration::from_secs(60),
        install_gate: basal_core::InstallGate::Off,
        ..Config::default()
    };
    let store = Arc::new(Store::open(world.store_path(), world.durability).unwrap());
    let recovered = Runtime::recover_hosts(store, Arc::new(NoHooks), &config).unwrap();
    let snapshots = Arc::new(BrocaStore::new_staged(&recovered));
    let host = Arc::new(BrocaHost::new(
        fake,
        snapshots.clone(),
        "/project".into(),
        "basal".into(),
        world.selector.clone(),
    ));
    let rt = Runtime::new_staged(
        recovered,
        host.clone(),
        Arc::new(world.catalog.clone()),
        Arc::new(NoHooks),
        Some(world.source.clone()),
        config,
    )
    .unwrap();
    rt.retain_detail_for_test().unwrap();
    rt.recover().unwrap();
    (rt, host, snapshots)
}
fn admit(rt: &Runtime, world: &World, script: &str) -> String {
    let mut manifest = test_manifest();
    manifest["llm"] = json!({"iq":0,"token_cap":{"tokens":10000,"window":"1h"},"max_output":100});
    rt.admit(&world.spec_with(rt, script, &manifest).unwrap())
        .unwrap()
        .run_id()
        .unwrap()
        .to_owned()
}
fn totals(rt: &Runtime, id: &str) -> (u64, u64, u64, u64, u64, u64) {
    let call = rt.fixture_call_state(id, 0).unwrap();
    let usage = rt
        .token_usage("flow-test", call.intent.as_ref().unwrap().audit_at)
        .unwrap()
        .unwrap();
    (
        usage.reserved,
        usage.input_tokens,
        usage.cache_write_tokens,
        usage.output_tokens,
        usage.cached_input_tokens,
        usage.unreported_tokens,
    )
}

#[test]
fn broca_usage_settles_reservation_once_and_classify_returns_a_string() {
    let world = World::new("broca-reservation");
    let (rt, host, fake, snapshots) = setup(&world);
    let id = admit(
        &rt,
        &world,
        "return await classify('hello', ['yes', 'no']);",
    );
    assert!(matches!(
        rt.resume(&id).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    rt.quiesce();
    let key = fake.keys()[0].clone();
    fake.finish(
        &key,
        "yes",
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
    host.poll().unwrap();
    host.poll().unwrap();
    let saved = snapshots.load().unwrap()[0].clone();
    assert!(saved.acknowledged);
    let outcome = HostOutcome {
        settlement: basal_proto::Settlement::Fulfilled,
        value: JsonText::new("\"yes\"").unwrap(),
        usage: Some(TokenUsage {
            input_tokens: Some(7),
            cache_write_tokens: Some(2),
            output_tokens: Some(5),
            cached_input_tokens: Some(100),
        }),
    };
    assert_eq!(
        rt.complete(&Completion {
            run_id: id.clone(),
            position: 0,
            handle: saved.handle.unwrap(),
            outcome
        })
        .unwrap(),
        CompletionAck::Duplicate
    );
    assert!(
        !rt.report_usage(
            &key,
            LedgerUsage {
                input_tokens: 999,
                cache_write_tokens: 999,
                output_tokens: 999,
                cached_input_tokens: 999
            }
        )
        .unwrap()
    );
    let call = rt.fixture_call_state(&id, 0).unwrap();
    assert!(call.arrival.is_some());
    assert_eq!(rt.open_reservations(&id).unwrap(), 0);
    let report = call.usage_report.unwrap();
    assert_eq!(
        (
            report.input_tokens,
            report.cache_write_tokens,
            report.output_tokens,
            report.cached_input_tokens
        ),
        (Some(7), Some(2), Some(5), Some(100))
    );
    assert_eq!(totals(&rt, &id), (0, 7, 2, 5, 100, 0));
    assert_eq!(
        totals(&rt, &id).1 + totals(&rt, &id).2 + totals(&rt, &id).3,
        14,
        "cached reads do not add to the charged total"
    );
    assert!(matches!(
        rt.resume(&id).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    rt.quiesce();
    assert_eq!(rt.run(&id).unwrap().result.as_deref(), Some("\"yes\""));
}

#[test]
fn script_value_cannot_supply_usage_and_absent_ledger_fields_are_nullable() {
    let world = World::new("broca-metadata");
    let (rt, host, fake, snapshots) = setup(&world);
    let id = admit(&rt, &world, "return await llm({prompt:'hi'});");
    assert!(matches!(
        rt.resume(&id).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    rt.quiesce();
    let key = fake.keys()[0].clone();
    fake.finish(
        &key,
        "answer",
        RunFinishReason::Completed,
        Some(Usage {
            input_tokens: Some(0),
            ..Usage::default()
        }),
    )
    .unwrap();
    host.poll().unwrap();
    let report = rt.fixture_call_state(&id, 0).unwrap().usage_report.unwrap();
    assert_eq!(report.input_tokens, Some(0));
    assert_eq!(report.cache_write_tokens, None);
    assert_eq!(report.output_tokens, None);
    assert!(totals(&rt, &id).5 > 0);
    assert!(
        snapshots.load().unwrap()[0]
            .outcome
            .as_ref()
            .unwrap()
            .value
            .find("usage")
            .is_none()
    );
    drop(host);
    assert!(matches!(
        rt.resume(&id).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    rt.quiesce();

    // Only host-supplied token metadata can settle the reservation. A usage
    // property in the script-visible result must not lower the token charge
    // when the provider reported no measurements.
    let world = World::new("broca-value-usage");
    let rt = world
        .runtime(
            Arc::new(NoHooks),
            Config {
                selector: world.selector.clone(),
                auto_resume: false,
                activation_deadline: Duration::from_secs(60),
                install_gate: basal_core::InstallGate::Off,
                ..Config::default()
            },
        )
        .unwrap();
    let id = admit(&rt, &world, "return await llm({prompt:'hi'});");
    assert!(matches!(
        rt.resume(&id).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    rt.quiesce();
    let handle = world.mock.handle_for(&id, 0).unwrap();
    world.mock.complete(&handle, HostOutcome::fulfilled(JsonText::new(json!({"text":"hi","usage":{"input_tokens":0,"cache_write_tokens":0,"output_tokens":0,"cached_input_tokens":0}}).to_string()).unwrap()));
    let call = rt.fixture_call_state(&id, 0).unwrap();
    assert!(call.usage_report.is_none());
    let reservation = call.intent.unwrap().reservation.unwrap();
    assert_eq!(
        totals(&rt, &id).5,
        reservation.input_tokens + reservation.output_tokens
    );
    assert!(totals(&rt, &id).5 > 0);
}

#[test]
fn an_unknown_broca_run_moves_the_run_to_needs_reconcile_naming_it() {
    let world = World::new("broca-unknown-run");
    let (rt, host, fake, snapshots) = setup(&world);
    let id = admit(&rt, &world, "return await llm({prompt:'hi'});");
    assert!(matches!(
        rt.resume(&id).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    rt.quiesce();
    let key = fake.keys()[0].clone();
    fake.forget(&key).unwrap();
    host.poll().unwrap();
    let run = rt.run(&id).unwrap();
    assert_eq!(run.state, RunState::NeedsReconcile);
    assert_eq!(run.error_kind.as_deref(), Some("unknown_outcome"));
    let detail = run.error_detail.unwrap();
    assert!(
        detail.contains(&format!("run:{key}")) && detail.contains("unknown_run"),
        "{detail}"
    );
    let call = rt.calls(&id).unwrap()[0].clone();
    assert_eq!(call.dispatch, DispatchState::Unknown);
    assert!(call.outcome.is_none());
    assert_eq!(
        rt.unknown_reason(&id, 0).unwrap(),
        Some(basal_host::UnknownReason::ProviderLostRun)
    );
    // A crash before the host saved its acknowledgement redelivers the
    // report; the runtime recognises it and nothing changes.
    let mut saved = snapshots.load().unwrap()[0].clone();
    assert!(saved.acknowledged && saved.outcome.is_none());
    saved.acknowledged = false;
    snapshots.save(&saved).unwrap();
    host.poll().unwrap();
    assert!(snapshots.load().unwrap()[0].acknowledged);
    assert_eq!(rt.run(&id).unwrap().error_detail.unwrap(), detail);
}

#[test]
fn usage_settles_once_when_an_outcome_is_redelivered_after_a_restart() {
    let world = World::new("broca-usage-once");
    let (rt, host, fake, snapshots) = setup(&world);
    let id = admit(&rt, &world, "return await llm({prompt:'hi'});");
    assert!(matches!(
        rt.resume(&id).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    rt.quiesce();
    let key = fake.keys()[0].clone();
    let usage = Usage {
        input_tokens: Some(7),
        cache_write_tokens: Some(2),
        output_tokens: Some(5),
        cached_input_tokens: Some(100),
        reasoning_tokens: Some(3),
    };
    fake.finish(&key, "once", RunFinishReason::Completed, Some(usage))
        .unwrap();
    host.poll().unwrap();
    assert_eq!(totals(&rt, &id), (0, 7, 2, 5, 100, 0));
    // Simulate a crash after the runtime recorded the outcome and before
    // the host saved its acknowledgement, then restart both.
    let mut saved = snapshots.load().unwrap()[0].clone();
    saved.acknowledged = false;
    snapshots.save(&saved).unwrap();
    let calls = fake.calls();
    drop(host);
    drop(snapshots);
    drop(rt);
    let (rt, _host, snapshots) = open(&world, fake.clone());
    assert!(snapshots.load().unwrap()[0].acknowledged);
    // The saved outcome was redelivered without asking Broca again.
    assert_eq!(fake.calls(), calls);
    assert_eq!(totals(&rt, &id), (0, 7, 2, 5, 100, 0));
    assert!(matches!(
        rt.resume(&id).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    rt.quiesce();
    assert_eq!(totals(&rt, &id), (0, 7, 2, 5, 100, 0));
}

#[test]
fn pruning_a_settled_flow_removes_its_model_calls_wait_and_wakes() {
    let world = World::new("broca-cascade");
    let (rt, _host, _fake, _snapshots) = setup(&world);
    let id = admit(&rt, &world, "return await llm({prompt:'hi'});");
    assert!(matches!(
        rt.resume(&id).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    rt.quiesce();
    let counts = || -> (i64, i64) {
        rt.store()
            .read(|c| {
                Ok((
                    c.query_row("SELECT COUNT(*) FROM broca_waits", [], |r| r.get(0))?,
                    c.query_row("SELECT COUNT(*) FROM broca_wakes", [], |r| r.get(0))?,
                ))
            })
            .unwrap()
    };
    rt.store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO broca_wakes (send_id, event_key, kind, digest, received_at) SELECT send_id, 'k', 'ended', 'd', 0 FROM broca_waits",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(counts(), (1, 1));
    let handle = rt.calls(&id).unwrap()[0].handle.clone().unwrap();
    rt.cancel(&id, "operator").unwrap();
    assert_eq!(
        rt.complete(&basal_host::Completion {
            run_id: id.clone(),
            position: 0,
            handle,
            outcome: basal_host::HostOutcome::fulfilled(basal_proto::JsonText::null()),
        })
        .unwrap(),
        basal_host::CompletionAck::Accepted
    );
    // Stand in for the adapter's durable acknowledgement. Retention, rather
    // than a journal FK cascade, owns deleting the settled call and its waits.
    rt.store()
        .write(|tx| {
            tx.execute("UPDATE broca_calls SET snapshot=json_set(snapshot,'$.acknowledged',json('true')) WHERE run_id=?1", [&id])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(rt.prune(i64::MAX / 2, 0).unwrap().pruned, vec![id]);
    assert_eq!(counts(), (0, 0));
}
