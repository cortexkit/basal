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
                .any(|line| line.starts_with("SEARCH live_runs ") || line.starts_with("SEARCH r ")),
            "{name}: {details:?}"
        );
        assert!(
            details.iter().any(|line| line.contains("flow_id=?")),
            "{name}: {details:?}"
        );
        assert!(
            !details.iter().any(|line| line.starts_with("SCAN live_runs")
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
    let host = Arc::new(RecordingHost::default());
    let fixture = TestRuntime::with_host(host.clone());
    let rt = fixture.rt.clone();
    rt.store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO flows(flow_id,removed,created_at) VALUES ('f',1,0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    use crate::run_records::{Arrival, ArrivalDurability, Body, CallIdentity, Intent, State};
    use basal_proto::{CallKind, HostCall, JsonText, Primitive, Settlement};
    rt.fixture_run("r", "f", 1, "return 1", "{}", State::new(0))
        .unwrap();
    rt.fixture_append(
        "r",
        Body::Intent(Intent {
            call: CallIdentity::new(
                HostCall {
                    position: 0,
                    kind: CallKind::Primitive(Primitive::Llm),
                    args: JsonText::null(),
                },
                crate::StoredClass::Query,
            ),
            idempotency_key: Some("k".into()),
            frozen_request: JsonText::null(),
            attempts: 1,
            dispatch_count_window: None,
            audit_at: 0,
            reservation: None,
        }),
    )
    .unwrap();
    rt.fixture_append(
        "r",
        Body::Accepted {
            position: 0,
            handle: "k".into(),
        },
    )
    .unwrap();
    rt.fixture_append(
        "r",
        Body::Arrived(Arrival {
            position: 0,
            arrival_seq: 0,
            outcome: "fulfilled".into(),
            settlement: Settlement::Fulfilled,
            value_hash: crate::ids::payload_hash(Settlement::Fulfilled, &JsonText::null()),
            value: JsonText::null(),
            handle: Some("k".into()),
            usage: None,
            cost_micros: None,
            kv_effect: None,
            durability: ArrivalDurability {
                host_callback: true,
                ..Default::default()
            },
        }),
    )
    .unwrap();
    rt.store().write(|tx| { tx.execute("INSERT INTO broca_calls VALUES ('k','r',0,'{\"acknowledged\":false,\"deferred\":false}')", [])?; Ok(()) }).unwrap();
    rt.fixture_finish("r", crate::RunState::Cancelled, None, None)
        .unwrap();
    // Ignore route notifications from constructing the terminal fixture; the
    // assertions below isolate maintenance and the final adapter release.
    host.retained.lock().unwrap().clear();
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
