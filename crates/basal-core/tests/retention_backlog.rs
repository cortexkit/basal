use basal_core::{Clock, Config, InstallGate, NoHooks, Runtime, Store};
use basal_host::{CallClass, CallRequest, CompletionSink, Dispatched, Host, TransportError};
use basal_proto::CallKind;
use std::path::PathBuf;
use std::sync::Arc;

struct NoDispatch;
impl Host for NoDispatch {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> Result<Dispatched, TransportError> {
        panic!("retention must never dispatch a call")
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.5
    }
}

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn production_retention_catches_up_full_batches_without_waiting_a_whole_interval() {
    // Exercise each retained backlog independently so another full batch cannot
    // hide a missing catch-up signal. Successful runs retain no call payloads,
    // so they no longer need a separate payload-deletion backlog.
    for (run_rows, history_rows) in [(140, 0), (0, 140)] {
        let now = 365 * 24 * 60 * 60 * 1000;
        let temp = Temp(std::env::temp_dir().join(format!(
            "basal-retention-backlog-{}-{run_rows}-{history_rows}",
            std::process::id()
        )));
        let clock = Clock::manual(now);
        let rt = Runtime::new(
            Arc::new(
                Store::open(temp.0.join("core.db"), basal_core::Durability::throwaway()).unwrap(),
            ),
            Arc::new(NoDispatch),
            Arc::new(basal_host::catalog::MockCatalog::new()),
            Arc::new(NoHooks),
            None,
            Config {
                clock: clock.clone(),
                install_gate: InstallGate::Off,
                ..Config::default()
            },
        )
        .expect("runtime startup");
        rt.store().write(|tx| {
            tx.execute("INSERT INTO flows(flow_id,created_at) VALUES ('flow',0)", [])?;
            let snapshot = basal_core::runs::snapshot(tx,"flow",Some(1),&[0;32],"return 1;","{}")?;
            for i in 0..run_rows {
                let id = format!("old-{i:03}");
                tx.execute("INSERT INTO run_history(run_id,flow_id,flow_version,trigger_id,trigger_kind,trigger,admitted_at,admit_seq,ended_at,state,result_size,call_counts,writes_sent,tokens_charged,cost_microusd,broken,activation_count,script_started,phase,deadline_exempt,history_lost,commit_seq,snapshot_id) VALUES(?1,'flow',1,?1,'manual','null',0,?2,0,'succeeded',0,'{}',0,0,0,0,1,1,'script',0,0,1,?3)", rusqlite::params![id, i, snapshot])?;
            }
            for _ in 0..history_rows {
                tx.execute("INSERT INTO outbox(at,kind,flow_id,body,delivered_at) VALUES (0,'delivered','flow','{}',0)", [])?;
            }
            Ok(())
        }).unwrap();
        let remaining = || {
            rt.store()
                .read(|c| {
                    Ok((
                        c.query_row("SELECT count(*) FROM run_history", [], |r| {
                            r.get::<_, i64>(0)
                        })?,
                        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))?,
                    ))
                })
                .unwrap()
        };
        rt.enforce_deadlines().unwrap();
        let expected_runs = if run_rows == 0 { 0 } else { 12 };
        let expected_history = if history_rows == 0 { 0 } else { 12 };
        assert_eq!(
            remaining(),
            (expected_runs, expected_history),
            "one pass must remain bounded"
        );
        clock.set(now + 1000);
        rt.enforce_deadlines().unwrap();
        assert_eq!(
            remaining(),
            (0, 0),
            "any full batch must prompt catch-up rather than cap pruning at 128 rows an hour"
        );
        rt.quiesce();
    }
}
