mod common;

use basal_core::{ActivationEnd, Admission, Durability, InstallRequest, NoHooks, Runtime, Store};
use basal_module::{
    caller::Caller,
    dryrun::{DryRunConfig, DryRunRequest, DryRunner, DryTrigger, Mode},
    pool::Binding,
};
use basal_proto::JsonText;
use common::*;
use serde_json::{Value, json};
use std::sync::Arc;

#[test]
fn module_projections_read_a_real_log_backed_flow() {
    let mut f = fixture(
        "log-reader-projections",
        Options {
            dry_run_max_fires: Some(0),
            ..Default::default()
        },
    );
    let directory = f.dir.join("staged");
    std::fs::create_dir_all(&directory).unwrap();
    let config = f.module.rt.config().clone();
    f.module.rt = Runtime::new_staged_scratch(
        Arc::new(Store::open(directory.join("store.db"), Durability::throwaway()).unwrap()),
        Arc::new(f.mock.clone()),
        Arc::new(f.catalog.clone()),
        Arc::new(NoHooks),
        None,
        config,
        false,
    )
    .unwrap();
    let manifest = json!({"id":"reader","version":1,"purpose":"Read staged flow projections","trigger":{"schedule":{"interval":"1h"}}});
    let installed = f
        .module
        .rt
        .install(&InstallRequest {
            script: "await kv.set('x', 7); return await kv.get('x');".into(),
            manifest: manifest.to_string(),
            author: "operator".into(),
            loop_override: false,
        })
        .unwrap();
    f.module
        .rt
        .approve("reader", 1, &installed.code_hash, "reader test")
        .unwrap();
    let Admission::Admitted { run_id } = f
        .module
        .rt
        .admit_trigger(
            "reader",
            "first",
            JsonText::new("{\"kind\":\"manual\"}").unwrap(),
        )
        .unwrap()
    else {
        panic!("run was not admitted")
    };
    let mut worker = f
        .module
        .pool
        .acquire(Binding::Flow("reader".into()))
        .unwrap();
    worker.serve_run(&run_id);
    assert_eq!(
        f.module.rt.activate(&run_id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { value: "7".into() }
    );
    drop(worker);
    let list = call(&f, &Caller::Operator, "flow.list", json!({})).unwrap();
    assert_eq!(list["flows"][0]["last_run"]["run_id"], run_id);
    assert_eq!(list["flows"][0]["last_run"]["state"], "succeeded");
    let health = call(
        &f,
        &Caller::Operator,
        "flow.health",
        json!({"flow_ids":["reader"]}),
    )
    .unwrap();
    assert_eq!(health["flows"][0]["last_run"]["outcome"], "succeeded");
    let overview = call(&f, &Caller::Operator, "flow.overview", json!({})).unwrap();
    assert_eq!(overview["flows"][0]["last_run"]["outcome"], "succeeded");
    let details = call(
        &f,
        &Caller::Operator,
        "flow.details",
        json!({"flow_id":"reader"}),
    )
    .unwrap();
    assert_eq!(details["runs"][0]["run_id"], run_id);
    assert_eq!(details["runs"][0]["trigger"], "manual");
    assert!(
        call(&f, &Caller::Operator, "diagnostics.findings", Value::Null).unwrap()["findings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    call(&f, &Caller::Operator, "flow.drain", json!({})).unwrap();
    assert!(f.module.rt.health().unwrap().draining);
    f.module
        .rt
        .store()
        .read(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('runs','journal') OR name LIKE 'rig_%'", [], |r| r
                    .get::<_, u64>(0))?,
                0
            );
            assert_eq!(c.query_row("SELECT COUNT(*) FROM run_history", [], |r| r.get::<_,u64>(0))?,1);
            Ok(())
        })
        .unwrap();
}

#[test]
fn staged_dry_run_uses_its_own_log_and_retains_success_call_detail() {
    let f = fixture("log-scratch", Options::default());
    let scratch = f.dir.join("log-dry-runs");
    let runner = DryRunner::new(
        Arc::new(f.catalog.clone()),
        Arc::new(f.mock.clone()),
        f.module.pool.clone(),
        DryRunConfig::new(&scratch),
        f.module.rt.config().clone(),
        f.module.metrics.clone(),
    );
    let manifest = json!({"id":"scratch-reader","version":1,"purpose":"Read retained dry-run calls","trigger":{"schedule":{"interval":"1h"}}});
    let reply = runner
        .run(&DryRunRequest {
            script: "await kv.set('x', 7); return await kv.get('x');".into(),
            manifest: manifest.to_string(),
            author: "operator".into(),
            loop_override: false,
            mode: Mode::Capture,
            trigger: DryTrigger::Synthetic(json!({})),
            now_ms: f.clock.now_ms(),
        })
        .unwrap();
    assert_eq!(reply["runs"][0]["state"], "succeeded");
    assert_eq!(
        reply["runs"][0]["calls"].as_array().unwrap().len(),
        2,
        "retain_detail preserves even successful calls until the disposable runtime is dropped"
    );
    assert_eq!(reply["runs"][0]["result"], json!(7));
    assert_eq!(
        std::fs::read_dir(&scratch).unwrap().count(),
        0,
        "scratch cleanup removes the SQLite file and its own run-log directory"
    );
    f.module
        .rt
        .store()
        .read(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM run_history", [], |r| r
                    .get::<_, u64>(0))?,
                0
            );
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM live_runs", [], |r| r.get::<_, u64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
}
