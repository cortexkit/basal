use basal_core::broca::BrocaStore;
use basal_core::tokens::Usage as LedgerUsage;
use basal_core::{ActivationEnd, Config, NoHooks, Runtime, Store};
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
    let store = Arc::new(Store::open(world.store_path(), world.durability).unwrap());
    let snapshots = Arc::new(BrocaStore::new(store.clone()));
    let fake = Arc::new(FakeBroca::default());
    let host = Arc::new(BrocaHost::new(
        fake.clone(),
        snapshots.clone(),
        "/project".into(),
        "basal".into(),
        world.selector.clone(),
    ));
    let rt = Runtime::new(
        store,
        host.clone(),
        Arc::new(world.catalog.clone()),
        Arc::new(NoHooks),
        Some(world.source.clone()),
        Config {
            selector: world.selector.clone(),
            auto_resume: false,
            activation_deadline: Duration::from_secs(60),
            ..Config::default()
        },
    );
    rt.recover().unwrap();
    (rt, host, fake, snapshots)
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
    rt.store().read(|c| {
        let row: (String, i64, i64, i64, i64, i64) = c.query_row("SELECT state, input_tokens, cache_write_tokens, output_tokens, cached_input_tokens, unreported_tokens FROM token_ledger WHERE send_id = ?1", [&key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        assert_eq!(row, ("settled".into(),7,2,5,100,0));
        let totals: (i64,i64,i64,i64,i64) = c.query_row("SELECT reserved,input_tokens,cache_write_tokens,output_tokens,cached_input_tokens FROM token_windows", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        assert_eq!(totals,(0,7,2,5,100)); Ok(())
    }).unwrap();
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
    rt.store().read(|c| {
        let row: (Option<i64>,Option<i64>,Option<i64>,i64) = c.query_row("SELECT input_tokens,cache_write_tokens,output_tokens,unreported_tokens FROM token_ledger WHERE send_id=?1",[&key],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        assert_eq!(row.0,Some(0)); assert_eq!(row.1,None); assert_eq!(row.2,None); assert!(row.3>0); Ok(())
    }).unwrap();
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
    rt.store()
        .read(|c| {
            let row: (i64, i64) = c.query_row(
                "SELECT reserved,unreported_tokens FROM token_ledger",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(row.0, row.1);
            assert!(row.1 > 0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn sqlite_snapshot_cut_never_persists_a_cursor_without_its_text() {
    let world = World::new("broca-snapshot-cut");
    let (rt, host, fake, snapshots) = setup(&world);
    let id = admit(&rt, &world, "return await llm({prompt:'hi'});");
    assert!(matches!(
        rt.resume(&id).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    rt.quiesce();
    host.poll().unwrap();
    let before = snapshots.load().unwrap()[0].clone();
    fake.assistant(
        &fake.keys()[0],
        vec![basal_host::broca::wire::ContentBlock::Text {
            text: "buffered".into(),
        }],
    )
    .unwrap();
    rt.store().cut();
    assert!(host.poll().is_err());
    drop(host);
    drop(snapshots);
    drop(rt);
    let store = Arc::new(Store::open(world.store_path(), world.durability).unwrap());
    let snapshots = Arc::new(BrocaStore::new(store));
    let saved = snapshots.load().unwrap()[0].clone();
    assert_eq!(saved.cursor, before.cursor);
    assert_eq!(saved.text, None);
    let host = BrocaHost::new(
        fake,
        snapshots.clone(),
        "/project".into(),
        "basal".into(),
        world.selector.clone(),
    );
    host.poll().unwrap();
    let after = snapshots.load().unwrap()[0].clone();
    assert_eq!(after.text.as_deref(), Some("buffered"));
    assert!(after.cursor > before.cursor);
}
