use super::*;

#[test]
fn new_gate_backoffs_reap_terminal_and_pruned_runs() {
    let fixture = crate::runtime_authorization_regressions::TestRuntime::new();
    let rt = &fixture.rt;
    for id in ["done", "gone", "waiting"] {
        rt.store().write(|tx| {
            tx.execute("INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at) VALUES (?1,'f',?1,1,'null','return 1','{}',zeroblob(32),'pending',1)", [id])?;
            Ok(())
        }).unwrap();
        rt.back_off(id).unwrap();
    }
    rt.store()
        .write(|tx| {
            tx.execute("UPDATE runs SET state='cancelled' WHERE run_id='done'", [])?;
            tx.execute("DELETE FROM runs WHERE run_id='gone'", [])?;
            Ok(())
        })
        .unwrap();
    rt.back_off("waiting").unwrap();
    let table = rt.shared.gate_backoff.lock().unwrap();
    assert_eq!(table.len(), 1);
    assert_eq!(table["waiting"].failures, 2);
}

#[test]
fn removed_instance_gate_reports_removal_not_revocation() {
    let fixture = crate::runtime_authorization_regressions::TestRuntime::new();
    let rt = &fixture.rt;
    rt.store().write(|tx| {
        tx.execute_batch("INSERT INTO flows(flow_id,package,removed,created_at) VALUES ('instance','package',1,1); INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,owner,generation,admitted_at) VALUES ('run','instance','trigger',1,'null','return 1','{}',zeroblob(32),'running','parent',1,1);")?;
        Ok(())
    }).unwrap();
    let lease = Lease {
        run_id: "run".into(),
        owner: "parent".into(),
        generation: 1,
    };
    let run = rt.store().read(|conn| runs::load(conn, "run")).unwrap();
    let Gate::Closed(ActivationEnd::Revoked { cause, .. }) =
        rt.check_install(&lease, &run).unwrap()
    else {
        panic!("removed instance activated");
    };
    assert_eq!(cause, RevokeCause::Removed);
}

#[test]
fn removed_cancellation_rechecks_mark_in_its_write_transaction() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    for migration in crate::schema::MIGRATIONS {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO flows(flow_id,package,removed,created_at) VALUES ('instance','package',1,1); INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,owner,generation,admitted_at) VALUES ('run','instance','trigger',1,'null','return 1','{}',zeroblob(32),'running','parent',1,1);").unwrap();
    let lease = Lease {
        run_id: "run".into(),
        owner: "parent".into(),
        generation: 1,
    };
    assert!(crate::packages::removed(&conn, "instance").unwrap());
    // The instance was read as removed above, but reconciliation restores
    // it before the cancelling write transaction starts. That earlier read
    // can't authorize the cancellation: the transaction must check the
    // removed mark again and leave the run running.
    conn.execute("UPDATE flows SET removed=0 WHERE flow_id='instance'", [])
        .unwrap();
    let tx = conn.transaction().unwrap();
    assert!(!cancel_removed(&tx, &lease, "instance", "removed").unwrap());
    assert!(lease.holds(&tx).unwrap());
    assert_eq!(runs::state(&tx, "run").unwrap(), crate::RunState::Running);
    tx.commit().unwrap();
}
