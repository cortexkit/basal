use super::*;
use crate::runtime_authorization_regressions::{TestRuntime, connection};
use basal_host::{CallClass, CallRequest, CompletionSink, Dispatched, Host, TransportError};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[test]
fn retirement_queries_search_only_the_requested_flows_indexes() {
    let c = connection();
    for (name, sql) in [
        ("live runs", LIVE_RUN_VERSIONS_SQL),
        ("pending models", PENDING_MODEL_VERSIONS_SQL),
    ] {
        let details = c
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(["requested-flow"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        println!("{name}: {details:?}");
        assert!(
            details
                .iter()
                .any(|line| line.starts_with("SEARCH runs ") || line.starts_with("SEARCH r ")),
            "{name}: {details:?}"
        );
        assert!(
            details.iter().any(|line| line.contains("flow_id=?")),
            "{name}: {details:?}"
        );
        assert!(
            !details.iter().any(|line| line.starts_with("SCAN runs")
                || line.starts_with("SCAN r ")
                || line.starts_with("SCAN b ")),
            "{name}: {details:?}"
        );
        if name == "pending models" {
            assert!(
                details
                    .iter()
                    .any(|line| line.starts_with("SEARCH b ") && line.contains("run_id=?")),
                "{details:?}"
            );
        }
    }
}

#[derive(Default)]
struct RecordingHost {
    retained: Mutex<Vec<(String, HashSet<String>)>>,
    sink: Mutex<Option<Arc<dyn CompletionSink>>>,
}
impl Host for RecordingHost {
    fn classify(&self, _: &basal_proto::CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> std::result::Result<Dispatched, TransportError> {
        unreachable!()
    }
    fn retain_flow_routes(&self, flow: &str, live: &HashSet<String>) {
        self.retained
            .lock()
            .unwrap()
            .push((flow.into(), live.clone()));
    }
    fn attach(&self, sink: Arc<dyn CompletionSink>) {
        *self.sink.lock().unwrap() = Some(sink);
    }
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.5
    }
}

#[test]
fn terminal_model_acknowledgement_retires_its_flow_without_a_global_sweep() {
    let fixture = TestRuntime::new();
    let host = Arc::new(RecordingHost::default());
    let rt = crate::Runtime::new(
        fixture.rt.shared.store.clone(),
        host.clone(),
        Arc::new(basal_host::MockCatalog::standard()),
        Arc::new(crate::NoHooks),
        None,
        crate::Config::default(),
    );
    rt.store().write(|tx| {
        tx.execute_batch("INSERT INTO flows(flow_id,removed,created_at) VALUES ('f',1,0);\
            INSERT INTO run_snapshots(flow_id,flow_version,code_hash,script,manifest) VALUES ('f',1,zeroblob(32),'return 1','{}');\
             INSERT INTO runs(run_id,flow_id,flow_version,trigger_id,attempt,trigger,snapshot_id,code_hash,state,admitted_at) VALUES ('r','f',1,'t',1,'null',(SELECT snapshot_id FROM run_snapshots WHERE flow_id='f'),zeroblob(32),'cancelled',0);\
            INSERT INTO journal(run_id,position,kind_code,args_bytes,args_digest,idempotency_key,class,dispatch,issued_generation) VALUES ('r',0,1,2,zeroblob(32),'k','query','sent',1);\
            INSERT INTO journal_payloads VALUES ('r',0,'{}',NULL,NULL);\
             INSERT INTO broca_calls VALUES ('k','r',0,'{\"acknowledged\":false,\"deferred\":false}');")?;
        Ok(())
    }).unwrap();
    rt.flush_refusals().unwrap();
    rt.enforce_deadlines().unwrap();
    assert!(host.retained.lock().unwrap().is_empty());
    rt.release_unused_flow_routes_for("f").unwrap();
    assert_eq!(
        *host.retained.lock().unwrap(),
        vec![(
            "f".into(),
            [basal_host::flow_scope::flow_route_key("f", 1)]
                .into_iter()
                .collect()
        )]
    );
    rt.store()
        .write(|tx| {
            tx.execute(
                "UPDATE broca_calls SET snapshot='{\"acknowledged\":true}' WHERE send_id='k'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    host.sink
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .route_released("r");
    assert_eq!(
        host.retained.lock().unwrap().last(),
        Some(&("f".into(), HashSet::new()))
    );
    assert_eq!(host.retained.lock().unwrap().len(), 2);
}
