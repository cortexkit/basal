use super::*;
use crate::module::Hosts;
use crate::pool::{PoolConfig, ProcessSpawner};
use crate::process::WorkerLaunch;
use crate::tool::Context;
use basal_core::codemode::retention::sweep;
use basal_host::{catalog::MockCatalog, consent::MockConsent, mock::MockHost};
use cortexkit_role_tool_provider::CallMode;
use subc_protocol::{BindIdentity, Principal};

const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

fn late_fixture() -> (Arc<Store>, Clock, Codemode, Context, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("basal-late-cursor-{}", random_id().unwrap()));
    let store =
        Arc::new(Store::open(root.join("store.db"), basal_core::Durability::throwaway()).unwrap());
    let clock = Clock::manual(WEEK_MS);
    let config = basal_core::Config {
        clock: clock.clone(),
        ..Default::default()
    };
    let mut pool = PoolConfig::new("missing", WorkerLaunch::Plain);
    pool.warm_spares = 0;
    let spawner = Arc::new(ProcessSpawner::codemode(&pool));
    let codemode = Codemode::new(
        store.clone(),
        &Hosts {
            host: Arc::new(MockHost::new()),
            transport: Arc::new(crate::unconfigured::UnconfiguredTransport),
            catalog: Arc::new(MockCatalog::new()),
            consent: Arc::new(MockConsent::new()),
            hooks: Arc::new(basal_core::NoHooks),
        },
        Pool::new(pool, spawner, clock.clone(), Arc::default()),
        &config,
    )
    .unwrap();
    let context = Context {
        principal: Principal::Reserved {
            module_id: "broca".into(),
        },
        scope: None,
        bind_identity: BindIdentity::new("broca", "test"),
        call_mode: CallMode::Classic,
        consumer_capabilities: None,
    };

    (store, clock, codemode, context, root)
}

#[test]
fn late_cursor_survives_a_sweep_without_skipping_kept_rows() {
    let (store, clock, codemode, context, root) = late_fixture();
    // Settlement can precede publication, so a later sequence can be older.
    store
        .write(|tx| {
            for (id, settled_at) in [("seen", WEEK_MS), ("kept", WEEK_MS), ("old", 1)] {
                let entry = late_results::LateEntry::new(
                    "result",
                    "reserved:broca",
                    "scope",
                    1,
                    "reserved:broca",
                    id,
                    id,
                    settled_at as u64,
                )
                .with_result(json!(id));
                tx.execute(
                    "INSERT INTO codemode_late_results(custodian,entry,event_id,settled_at)
                     VALUES ('reserved:broca',?1,?2,?3)",
                    rusqlite::params![serde_json::to_string(&entry).unwrap(), id, settled_at],
                )?;
            }
            Ok(())
        })
        .unwrap();
    let first = codemode.late(&context, json!({"limit":1})).unwrap();
    assert_eq!(first["entries"][0]["event_id"], "seen");
    let last = codemode
        .late(&context, json!({"since":first["cursor"]}))
        .unwrap();
    assert_eq!(last["entries"][0]["event_id"], "kept");
    assert_eq!(last["entries"][1]["event_id"], "old");

    clock.advance(1);
    sweep(&store, clock.now_ms()).unwrap();

    let at_head = codemode
        .late(&context, json!({"since":last["cursor"]}))
        .unwrap();
    assert_eq!(at_head["entries"], json!([]));
    assert_eq!(at_head["cursor"], last["cursor"]);

    let crossing_gap = codemode
        .late(&context, json!({"since":first["cursor"]}))
        .unwrap();
    assert_eq!(crossing_gap["entries"], json!([last["entries"][0]]));
    codemode
        .ack(&context, json!({"through":crossing_gap["cursor"]}))
        .unwrap();
    assert_eq!(
        codemode
            .late(&context, json!({"since":last["cursor"]}))
            .unwrap(),
        at_head
    );

    let foreign = codemode
        .late(
            &context,
            json!({"since":{
                "provider_incarnation":"another-incarnation",
                "seq":last["cursor"]["seq"],
            }}),
        )
        .unwrap_err();
    assert_eq!(foreign.code, late_results::CURSOR_INCARNATION_CHANGED);
    drop((codemode, store));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn published_held_runs_are_forgotten_after_the_race_window() {
    let mut late = LateCalls::default();
    late.held.insert("old".into());
    late.mark_published("old", 1_000);
    assert!(late.held.is_empty());
    assert!(late.published.contains_key("old"));

    // Inside the window the entry still blocks a stale timeout from reopening it.
    late.mark_published("recent", 1_000 + PUBLISHED_MEMORY_MS - 1);
    assert!(late.published.contains_key("old"));

    // Once the window has passed, recording another run drops it, so the map
    // holds only runs published within the last window.
    late.mark_published("newer", 1_000 + PUBLISHED_MEMORY_MS);
    assert!(!late.published.contains_key("old"));
    assert_eq!(
        late.published.keys().cloned().collect::<Vec<_>>(),
        ["newer", "recent"]
    );
}

#[test]
fn withdrawing_a_pruned_run_does_not_return_its_cached_result() {
    let (store, _, codemode, broca, root) = late_fixture();
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO codemode_tombstones(run_id,pruned_at) VALUES ('pruned',1)",
                [],
            )?;
            tx.execute(
                "INSERT INTO codemode_tool_calls(carrier,call_key,run_id,scope,custodian,published,withdrawal)
                 VALUES ('reserved:broca','key','pruned','null','reserved:broca',1,?1)",
                [json!({"answer":"completed","result_retained":true,
                    "result":{"structuredContent":{"value":42}}}).to_string()],
            )?;
            Ok(())
        })
        .unwrap();
    // An older build could have pruned the run while leaving a cached result behind.
    let reply = codemode
        .withdraw(
            &broca,
            &ToolCallRequest::new("tool.withdraw", json!({"call_key":"key"})),
        )
        .unwrap();
    assert_eq!(reply["answer"], "completed");
    assert_eq!(reply["result_retained"], false);
    assert!(!reply.as_object().unwrap().contains_key("result"));
    drop((codemode, store));
    std::fs::remove_dir_all(root).unwrap();
}
