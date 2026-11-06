#![cfg(target_os = "linux")]

use basal_core::{Durability, Store};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Copy(PathBuf);
impl Drop for Copy {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
        let home = self.0.with_extension("home");
        if home.exists() {
            std::fs::remove_dir_all(home).unwrap();
        }
    }
}

fn invoke(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_basal-cutover-rehearse"))
        .arg("--store")
        .arg(root)
        .args(args)
        .env("HOME", root.with_extension("home"))
        .env("XDG_DATA_HOME", root.with_extension("home").join("data"))
        .output()
        .unwrap()
}

fn summary(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON: {e}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn cutover_rehearse_cli_crash_resume() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!("rehearse-cli-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let copy = Copy(root);
    let root = &copy.0;
    let store = Store::open(root.join("store.db"), Durability::throwaway()).unwrap();
    // As in the core migration fixture, start with legacy rows and 4 KiB pages.
    store
        .read(|c| {
            c.pragma_update(None, "journal_mode", "DELETE")?;
            c.pragma_update(None, "page_size", 4096)?;
            c.execute_batch("VACUUM")?;
            c.pragma_update(None, "journal_mode", "WAL")?;
            Ok(())
        })
        .unwrap();
    store.write(|tx| {
        let hash = basal_core::ids::code_hash("return 7;", "{}");
        tx.execute("INSERT INTO run_snapshots(snapshot_id,flow_id,flow_version,code_hash,script,manifest) VALUES(1,'flow',1,?1,'return 7;','{}')", [hash.as_slice()])?;
        for (id, state, ended, seq) in [("success", "succeeded", Some(10), 1), ("live", "suspended", None, 2)] {
            tx.execute("INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,snapshot_id,code_hash,state,admitted_at,ended_at,flow_version,admit_seq,deadline_ms,deadline_at,self_input,params_input,grant_guards,script_started,generation,awaited,result) VALUES(?1,'flow',?1,0,'{\"kind\":\"manual\"}',1,?2,?3,1,?4,1,?5,10000,10001,'{}','{}','[]',1,1,'[]',?6)", rusqlite::params![id, hash.as_slice(), state, ended, seq, ended.map(|_| "7")])?;
        }
        Ok(())
    }).unwrap();
    drop(store);

    let refused = invoke(root, &[]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        summary(&refused)["error"]
            .as_str()
            .unwrap()
            .contains("REHEARSAL_COPY")
    );
    assert!(!root.join("run-log").exists());
    std::fs::write(root.join("REHEARSAL_COPY"), b"").unwrap();

    let outside = root.with_extension("home").join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let temp = outside.join(".basal-123-456.tmp");
    std::fs::write(&temp, b"must survive").unwrap();
    let target = outside.join("target.txt");
    std::fs::write(&target, b"original").unwrap();
    let request = json!({"args":{"path":target},"grant":{"roots":[outside]}}).to_string();
    let store = Store::open(root.join("store.db"), Durability::throwaway()).unwrap();
    store.write(|tx| {
        tx.execute("INSERT INTO journal(run_id,position,kind_code,module,op,args_digest,idempotency_key,class,dispatch,attempts,handle,issued_generation,args_bytes,request_bytes) VALUES('live',0,?1,NULL,NULL,?2,'unsafe','keyed_mutation','accepted',1,'unsafe',1,2,?3)", rusqlite::params![i64::from(basal_proto::Primitive::FsWrite.code()), [0u8;32].as_slice(), request.len()])?;
        tx.execute("INSERT INTO journal_payloads VALUES('live',0,'{}',?1,NULL)", [&request])?;
        Ok(())
    }).unwrap();
    drop(store);
    let refused = invoke(root, &[]);
    assert_eq!(refused.status.code(), Some(1));
    let refusal = summary(&refused);
    assert!(
        refusal["error"]
            .as_str()
            .unwrap()
            .contains("fs.write sweep")
    );
    assert_eq!(refusal["unsafe_fs_write_cleanup"][0]["path"], json!(target));
    assert_eq!(refusal["before"]["migration_phase"], "sweep");
    assert!(!root.join("run-log").exists());
    assert_eq!(std::fs::read(&temp).unwrap(), b"must survive");
    assert_eq!(std::fs::read(&target).unwrap(), b"original");
    let store = Store::open(root.join("store.db"), Durability::throwaway()).unwrap();
    store
        .write(|tx| {
            tx.execute("DELETE FROM journal_payloads WHERE run_id='live'", [])?;
            tx.execute("DELETE FROM journal WHERE run_id='live'", [])?;
            Ok(())
        })
        .unwrap();
    drop(store);

    // A before-commit cut must leave the old phase durable, even after schema work.
    let crash = invoke(root, &["--crash-at", "create:before"]);
    assert_eq!(
        crash.status.code(),
        Some(86),
        "{}",
        String::from_utf8_lossy(&crash.stderr)
    );
    let crash_summary = summary(&crash);
    assert_eq!(crash_summary["status"], "crashed");
    assert_eq!(crash_summary["before"]["page_size"], 4096);
    assert_eq!(crash_summary["before"]["table_rows"]["runs"], 2);
    assert_eq!(crash_summary["crash"]["boundary"], "MigrationCreateReady");

    let resumed = invoke(root, &["--measure-reads"]);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let report = summary(&resumed);
    assert_eq!(report["resumed_from"], "create");
    assert_eq!(report["after"]["migration_phase"], "done");
    assert_eq!(report["after"]["page_size"], 1024);
    assert_eq!(report["after"]["journal_mode"], "wal");
    assert_eq!(report["after"]["integrity_check"], json!(["ok"]));
    assert_eq!(report["after"]["foreign_key_check"], json!([]));
    assert_eq!(report["after"]["migration_audit_gone"], true);
    assert_eq!(report["after"]["table_rows"]["run_history"], 1);
    assert_eq!(report["after"]["table_rows"]["live_runs"], 1);
    assert_eq!(report["after"]["table_rows"]["obligations"], 0);
    assert_eq!(report["after"]["table_rows"]["broca_calls"], 0);
    assert!(report["after"]["table_rows"].get("runs").is_none());
    assert_eq!(report["post_check_failures"], json!([]));
    let phases: Vec<_> = report["phase_logs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["phase"].as_str())
        .collect();
    assert_eq!(
        phases,
        [
            "create",
            "finished_runs",
            "live_runs",
            "broca_calls",
            "drop",
            "page_size",
            "finish"
        ]
    );
    assert!(String::from_utf8_lossy(&resumed.stderr).contains("duration_ms"));
    for handler in [
        "flow.list",
        "flow.overview",
        "flow.details",
        "diagnostics.findings",
    ] {
        assert_eq!(report["reads"][handler]["status"], "skipped");
        assert!(
            report["reads"][handler]["reason"]
                .as_str()
                .unwrap()
                .contains("private")
        );
    }
    let proxy = &report["reads"]["single_run_detail_proxy"];
    assert_eq!(proxy["function"], "basal_core::run_store_sql::history");
    assert_eq!(proxy["samples"], 30);
    assert_eq!(proxy["flow_id"], "flow");
    assert_eq!(proxy["run_id"], "success");
    assert_eq!(proxy["retained_history_rows"], 1);
    assert!(proxy["p50_ms"].as_f64().unwrap() <= proxy["p95_ms"].as_f64().unwrap());
    assert!(proxy["p95_ms"].as_f64().unwrap() <= proxy["max_ms"].as_f64().unwrap());

    let repeated = invoke(root, &[]);
    assert!(repeated.status.success());
    assert_eq!(summary(&repeated)["resumed_from"], "done");
    assert_eq!(
        summary(&repeated)["after"]["table_rows"],
        report["after"]["table_rows"]
    );

    // The marker never overrides the live-directory exclusion, including aliases.
    let home = root.with_extension("home");
    let live = home.join("data/cortexkit/basal/copy");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::write(live.join("REHEARSAL_COPY"), b"").unwrap();
    std::fs::write(live.join("store.db"), b"do not open").unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&live, &alias).unwrap();
    let refused = Command::new(env!("CARGO_BIN_EXE_basal-cutover-rehearse"))
        .arg("--store")
        .arg(&alias)
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join("data"))
        .output()
        .unwrap();
    assert!(
        summary(&refused)["error"]
            .as_str()
            .unwrap()
            .contains("live basal")
    );
    assert_eq!(
        std::fs::read(live.join("store.db")).unwrap(),
        b"do not open"
    );
}
