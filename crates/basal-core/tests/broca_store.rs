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
    let store = Arc::new(Store::open(dir.join("store.db"), Durability::default()).unwrap());
    let memory = Arc::new(MemoryStore::default());
    let selection = FakeSelector::default()
        .select(&SelectionRequest {
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
