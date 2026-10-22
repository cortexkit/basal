use basal_core::{Durability, Store, broca::BrocaStore};
use basal_host::CallRequest;
use basal_host::broca::{
    BrocaHost, StateStore,
    fake::{FakeBroca, MemoryStore},
};
use basal_host::selector::{FakeSelector, ModelSelector, SelectionRequest};
use basal_proto::{CallKind, JsonText, Primitive};
use serde_json::json;
use std::sync::Arc;

#[test]
fn keyed_snapshot_lookup_ignores_unrelated_corruption_and_migrates_legacy_params() {
    let dir = std::env::temp_dir().join(format!("basal-broca-store-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let store = Arc::new(Store::open(dir.join("store.db"), Durability::throwaway()).unwrap());
    let memory = Arc::new(MemoryStore::default());
    let selection = FakeSelector::default()
        .select(&SelectionRequest {
            caller_class: "flow".into(),
            iq: 1,
            eq: 1,
            flow_id: "f".into(),
            run_id: "r".into(),
            send_id: "key".into(),
        })
        .unwrap();
    let request = CallRequest { flow_id:"f".into(), run_id:"r".into(), position:0, kind:CallKind::Primitive(Primitive::Llm), args:JsonText::new(json!({"send_id":"key", "work_class":"flow:f", "session":"basal:flow-f:r:0", "op":"llm", "max_output":12, "request":{"prompt":"legacy"}, "selection":selection}).to_string()).unwrap(), idempotency_key:"key".into(), attempt:1 };
    BrocaHost::new(
        Arc::new(FakeBroca::default()),
        memory.clone(),
        "/".into(),
        "basal".into(),
        Arc::new(FakeSelector::default()),
    )
    .dispatch_model(&request)
    .unwrap();
    let call = memory.get("key").unwrap().unwrap();
    let mut legacy = serde_json::to_value(&call).unwrap();
    legacy["params"] = json!(call.params);
    legacy["envelope"] = json!(request.args.as_str());
    store.read(|c| {
        // This fixture exercises the snapshot adapter independently of journal
        // admission; foreign keys are disabled only on its private connection.
        c.pragma_update(None, "foreign_keys", false)?;
        c.execute("INSERT INTO broca_calls VALUES ('key', 'r', 0, ?1)", [legacy.to_string()])?;
        c.execute("INSERT INTO broca_calls VALUES ('unrelated', 'other', 0, '{\"acknowledged\":true}')", [])?;
        Ok(())
    }).unwrap();
    let adapter = BrocaStore::new(store.clone());
    assert_eq!(adapter.get("key").unwrap().unwrap().params, call.params);
    assert!(adapter.get("missing").unwrap().is_none());
    store
        .read(|c| {
            let snapshot: String = c.query_row(
                "SELECT snapshot FROM broca_calls WHERE send_id = 'key'",
                [],
                |r| r.get(0),
            )?;
            assert!(
                serde_json::from_str::<serde_json::Value>(&snapshot).unwrap()["params"].is_string()
            );
            c.execute("DELETE FROM broca_calls WHERE send_id = 'unrelated'", [])?;
            // The adapter's own query must be able to use the partial index.
            let mut stmt = c.prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                basal_core::broca::PENDING_IDS_SQL
            ))?;
            let plan = stmt
                .query_map([], |r| r.get::<_, String>(3))?
                .collect::<Result<Vec<_>, _>>()?;
            assert!(
                plan.iter().any(|row| row.contains("broca_calls_pending")),
                "{plan:?}"
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(adapter.pending_ids().unwrap(), vec!["key"]);
    let mut settled = call.clone();
    settled.acknowledged = true;
    adapter.save(&settled).unwrap();
    assert!(adapter.pending_ids().unwrap().is_empty());
    settled.acknowledged = false;
    settled.deferred = true;
    adapter.save(&settled).unwrap();
    assert!(adapter.pending_ids().unwrap().is_empty());
}

fn scratch(name: &str) -> (std::path::PathBuf, Arc<Store>) {
    let dir = std::env::temp_dir().join(format!("basal-broca-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(dir.join("store.db"), Durability::throwaway()).unwrap());
    (dir, store)
}

fn count(store: &Store, table: &str) -> i64 {
    store
        .read(|c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?))
        .unwrap()
}

#[test]
fn a_wait_whose_call_is_gone_is_dropped_not_spun_on() {
    let (dir, store) = scratch("orphan");
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO broca_waits (send_id, broca_run_id, check_at) VALUES ('ghost', 'run-ghost', 0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let adapter = Arc::new(BrocaStore::new(store.clone()));
    let host = BrocaHost::new(
        Arc::new(FakeBroca::default()),
        adapter.clone(),
        "/".into(),
        "basal".into(),
        Arc::new(FakeSelector::default()),
    );
    // A due wait with no call: the scheduled read drops it.
    host.wake().unwrap();
    assert_eq!(count(&store, "broca_waits"), 0);
    // A committed wake whose call is gone: handling it drops it.
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO broca_wakes (send_id, event_key, kind, digest, received_at) VALUES ('ghost', 'k', 'ended', 'd', 0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(adapter.next_check().unwrap(), Some(0));
    host.wake().unwrap();
    assert_eq!(count(&store, "broca_wakes"), 0);
    assert_eq!(adapter.next_check().unwrap(), None);
    assert_eq!(host.next_wake_in(), None);
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn deleting_a_broca_call_drops_its_wait_and_wakes() {
    let (dir, store) = scratch("triggers");
    // A table rebuild in a later migration drops triggers silently; these
    // two keep a removed call (including a cascade from its journal or
    // codemode_calls row) from leaving a wait that stays due forever.
    let triggers: Vec<String> = store
        .read(|c| {
            let mut statement = c.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'trigger' AND name LIKE '%forget_wait' ORDER BY name",
            )?;
            let rows = statement.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap();
    assert_eq!(
        triggers,
        [
            "broca_calls_forget_wait",
            "codemode_broca_calls_forget_wait"
        ]
    );
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}
