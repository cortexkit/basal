mod common;

use basal_core::{
    NoHooks, Runtime,
    admission::Admission,
    run_driver::Budgets,
    run_host::RecoveredHosts,
    run_log::{Config, Identity, RunLog},
    run_records::Record,
    run_store_sql as sql,
};
use basal_module::caller::Caller;
use basal_proto::JsonText;
use common::*;
use serde_json::{Value, json};
use std::sync::Arc;

const OWNER: &str = "SYNAPSE";

#[test]
fn restored_runs_appear_in_diagnostics_and_operator_can_cancel_or_retrigger_without_resend() {
    for (resolution, disabled) in [("cancel", false), ("retrigger", false), ("retrigger", true)] {
        let mut f = fixture(&format!("restore-{resolution}"), Options::default());
        let store = Arc::new(
            basal_core::Store::open(
                f.dir.join("restored/store.db"),
                basal_core::Durability::throwaway(),
            )
            .unwrap(),
        );
        let runtime = Runtime::new(
            store.clone(),
            Arc::new(f.mock.clone()),
            Arc::new(f.catalog.clone()),
            Arc::new(NoHooks),
            None,
            f.module.rt.config().clone(),
        )
        .unwrap();
        let manifest = events_manifest("restore-flow");
        let installed = runtime
            .install(&basal_core::InstallRequest {
                script: "return 1;".into(),
                manifest: manifest.to_string(),
                author: OWNER.into(),
                loop_override: false,
            })
            .unwrap();
        runtime
            .approve("restore-flow", 1, &installed.code_hash, "restore fixture")
            .unwrap();
        let Admission::Admitted { run_id } = runtime
            .admit_trigger(
                "restore-flow",
                "trigger",
                JsonText::new("{\"private_payload\":\"not-in-diagnostics\"}").unwrap(),
            )
            .unwrap()
        else {
            panic!("admitted")
        };
        let meta = store.read(sql::meta).unwrap();
        let config = Config {
            directory: f.dir.join("restored/run-log"),
            identity: Identity {
                store_id: store.store_id().as_bytes().try_into().unwrap(),
                log_epoch: meta.log_epoch,
            },
            lease_epoch: store.lease_epoch(),
            target_size: Config::TARGET_SIZE,
            hooks: Arc::new(NoHooks),
        };
        // Drop the runtime that owns this log before simulating backup loss.
        // The module engine still owns its separate, unused fixture runtime.
        drop(runtime);
        std::fs::remove_dir_all(&config.directory).unwrap();
        let (log, recovered) = RunLog::<Record>::open_restoring(config, store.clone()).unwrap();
        let log = Arc::new(log);
        let hosts = Arc::new(
            RecoveredHosts::recover(
                store.clone(),
                log.clone(),
                Arc::new(log.watermark()),
                &recovered.records,
                Budgets {
                    dispatch_window_ms: 1000,
                    token_window_ms: 1000,
                    dispatches: 100,
                    tokens: 100,
                    cost_micros: 100,
                    cost_window: None,
                },
                Arc::new(NoHooks),
                f.clock.clone(),
            )
            .unwrap(),
        );
        assert!(
            hosts.registry().snapshot().is_empty(),
            "a lost run must not resume"
        );
        f.module.rt = Runtime::new_staged(
            hosts.clone(),
            Arc::new(f.mock.clone()),
            Arc::new(f.catalog.clone()),
            Arc::new(NoHooks),
            None,
            basal_core::runtime::Config {
                clock: f.clock.clone(),
                auto_resume: false,
                ..Default::default()
            },
        )
        .unwrap();
        let findings = call(&f, &Caller::Operator, "diagnostics.findings", Value::Null).unwrap();
        let finding = findings["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["check"] == "history_lost")
            .unwrap();
        assert_eq!(finding["count"], 1);
        assert_eq!(finding["samples"][0]["run_id"], run_id);
        assert!(!findings.to_string().contains("not-in-diagnostics"));
        assert!(
            call(
                &f,
                &agent(OWNER),
                "flow.reconcile",
                json!({"run_id":run_id,"resolution":resolution})
            )
            .unwrap_err()
            .contains("not_permitted")
        );
        if disabled {
            store
                .write(|tx| {
                    tx.execute(
                        "UPDATE flows SET state='disabled',disabled_by='operator',disabled_at=0 WHERE flow_id='restore-flow'",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let reply = call(
            &f,
            &Caller::Operator,
            "flow.reconcile",
            json!({"run_id":run_id,"resolution":resolution}),
        )
        .unwrap();
        assert_eq!(reply["state"], "cancelled");
        assert!(store.read(|c| sql::live(c, &run_id)).unwrap().is_none());
        let history = store.read(|c| sql::history(c, &run_id)).unwrap().unwrap();
        assert!(history.history_lost);
        assert!(history.error_detail.unwrap().contains("accounting"));
        assert_eq!(
            store
                .read(|c| Ok(c.query_row(
                    "SELECT count(*) FROM run_calls WHERE run_id=?1",
                    [&run_id],
                    |r| r.get::<_, i64>(0)
                )?))
                .unwrap(),
            0
        );
        if resolution == "retrigger" && !disabled {
            let new_id = reply["admission"]["run_id"].as_str().unwrap();
            assert_ne!(new_id, run_id);
            assert!(new_id.starts_with(&format!("run-{}-", store.store_id())));
            let live = store.read(|c| sql::live(c, new_id)).unwrap().unwrap();
            assert_eq!(live.attempt, 2);
            assert_eq!(live.trigger_id, "trigger");
            assert!(!store.read(|c| sql::history_lost(c, new_id)).unwrap());
        } else if disabled {
            assert_eq!(reply["admission"]["state"], "disabled");
            assert_eq!(
                store
                    .read(|c| Ok(
                        c.query_row("SELECT count(*) FROM live_runs", [], |r| r.get::<_, u64>(0))?
                    ))
                    .unwrap(),
                0
            );
        }
        let findings = call(&f, &Caller::Operator, "diagnostics.findings", Value::Null).unwrap();
        assert!(
            !findings["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["check"] == "history_lost")
        );
        assert!(
            call(
                &f,
                &Caller::Operator,
                "flow.reconcile",
                json!({"run_id":run_id,"resolution":"cancel"})
            )
            .is_err()
        );
        assert!(
            f.mock.effects().is_empty(),
            "restore and reconciliation must never dispatch an old unkeyed write"
        );
        drop(hosts);
        drop(log);
        drop(store);
    }
}
