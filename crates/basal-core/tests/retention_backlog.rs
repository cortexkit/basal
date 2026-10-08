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
    let now = 365 * 24 * 60 * 60 * 1000;
    let temp =
        Temp(std::env::temp_dir().join(format!("basal-retention-backlog-{}", std::process::id())));
    let clock = Clock::manual(now);
    let rt = Runtime::new(
        Arc::new(
            Store::open(
                temp.0.join("core.db"),
                basal_core::Durability { fullfsync: false },
            )
            .unwrap(),
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
    );
    rt.store().write(|tx| {
        tx.execute("INSERT INTO flows(flow_id,created_at) VALUES ('flow',0)", [])?;
        for i in 0..140 {
            let id = format!("old-{i:03}");
            tx.execute("INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at,ended_at,admit_seq) VALUES (?1,'flow',?1,1,'null','return 1;','{}',zeroblob(32),'succeeded',0,0,?2)", rusqlite::params![id, i])?;
            tx.execute("INSERT INTO outbox(at,kind,flow_id,body,delivered_at) VALUES (0,'delivered','flow','{}',0)", [])?;
        }
        Ok(())
    }).unwrap();
    let remaining = || {
        rt.store()
            .read(|c| Ok(c.query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))?))
            .unwrap()
    };
    rt.enforce_deadlines().unwrap();
    assert_eq!(remaining(), 12, "one pass must remain bounded");
    let remaining_history = || {
        rt.store()
            .read(|c| Ok(c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))?))
            .unwrap()
    };
    assert_eq!(
        remaining_history(),
        12,
        "history cleanup must remain bounded too"
    );
    clock.set(now + 1000);
    rt.enforce_deadlines().unwrap();
    assert_eq!(
        remaining(),
        0,
        "a full batch must schedule prompt catch-up rather than cap pruning at 128 runs an hour"
    );
    assert_eq!(
        remaining_history(),
        0,
        "history must catch up on the same bounded cadence"
    );
    rt.quiesce();
}
