use super::*;
use crate::{
    run_log::{Lsn, RunLog},
    run_records::Record,
    run_store_sql::{self as sql, GcFloor},
};

fn log(rt: &Runtime) -> &RunLog<Record> {
    rt.shared
        .staged_hosts
        .as_ref()
        .unwrap()
        .log
        .file_log()
        .expect("production runtime must own a file-backed log")
}

fn floor(rt: &Runtime) -> GcFloor {
    rt.store().read(|c| Ok(sql::meta(c)?.gc_floor)).unwrap()
}

fn segment(root: &std::path::Path, epoch: u64, sequence: u64) -> PathBuf {
    root.join("run-log")
        .join(format!("e{epoch}"))
        .join(format!("{sequence:016x}.seg"))
}

fn finish(f: &Fixture, trigger: &str) -> String {
    let id = f.admit("finished", trigger);
    let mut worker = ScriptedWorker::new(vec![finished()]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    id
}

fn seal(rt: &Runtime) -> u64 {
    let sequence = log(rt).synced_lsn().sequence;
    assert!(log(rt).rotate().unwrap().sequence > sequence);
    sequence
}

#[test]
fn runtime_maintenance_reclaims_below_live_starts_on_retention_cadence() {
    let f = Fixture::new(false);
    f.install("finished", 1, json!({}));
    f.install("live", 1, json!({}));
    let mut sealed = Vec::new();
    let mut closed = Vec::new();
    for i in 0..3 {
        closed.push(finish(&f, &format!("closed-{i}")));
        sealed.push(seal(&f.rt));
    }

    f.host.accept.store(true, Ordering::SeqCst);
    let live = f.admit("live", "waiting");
    let mut worker = ScriptedWorker::new(vec![
        op(0, "query", json!({})),
        WorkerMessage::Blocked { awaiting: vec![0] },
        suspended(vec![0]),
    ]);
    assert_eq!(
        f.rt.activate(&live, &mut worker).unwrap(),
        ActivationEnd::Suspended { awaited: vec![0] }
    );
    let start: Lsn =
        f.rt.shared
            .staged_hosts
            .as_ref()
            .unwrap()
            .registry()
            .reclaim_starts()[&live];
    assert_eq!(seal(&f.rt), start.sequence);
    finish(&f, "after-live");
    let later = seal(&f.rt);
    let active = log(&f.rt).synced_lsn().sequence;
    let before = floor(&f.rt);
    assert!(sealed.iter().all(|seq| *seq < start.sequence));
    for &seq in &sealed {
        assert!(segment(&f.root, before.log_epoch, seq).exists());
    }

    f.rt.enforce_deadlines().unwrap();
    let after = floor(&f.rt);
    assert!(after.first_retained > before.first_retained);
    assert_eq!(after.first_retained, start.sequence);
    assert_eq!(after.live_starts.get(&live), Some(&start));
    for &seq in &sealed {
        assert!(!segment(&f.root, after.log_epoch, seq).exists());
    }
    for seq in [start.sequence, later, active] {
        assert!(segment(&f.root, after.log_epoch, seq).exists());
    }
    for id in closed {
        assert_eq!(f.rt.run(&id).unwrap().state, RunState::Succeeded);
    }

    // Closing the last live run makes its sealed segment eligible, but it
    // must wait for the same maintenance timer rather than self-waking.
    f.rt.cancel(&live, "operator").unwrap();
    f.rt.enforce_deadlines().unwrap();
    assert_eq!(floor(&f.rt), after);
    assert!(segment(&f.root, after.log_epoch, start.sequence).exists());
    f.rt.store()
        .write(|tx| {
            tx.execute("UPDATE schedules SET state='disabled'", [])?;
            Ok(())
        })
        .unwrap();
    let due = f.rt.shared.retention_next.lock().unwrap().unwrap();
    assert!(due > f.clock.now_ms());
    assert_eq!(f.rt.next_wake_at(None).unwrap(), Some(due));
    f.clock.set(due);
    f.rt.enforce_deadlines().unwrap();
    assert_eq!(floor(&f.rt).first_retained, active);
    assert!(!segment(&f.root, after.log_epoch, start.sequence).exists());
    assert!(!segment(&f.root, after.log_epoch, later).exists());
    assert!(segment(&f.root, after.log_epoch, active).exists());
    assert_eq!(f.rt.next_wake_at(None).unwrap(), Some(due + 3_600_000));
}

#[test]
fn production_runtime_first_maintenance_reclaims_sealed_backlog() {
    let mut f = Fixture::new(false);
    Arc::make_mut(&mut f.rt.config).rate.max_runs = 94;
    f.install("finished", 1, json!({}));
    let mut sealed = Vec::new();
    for i in 0..94 {
        finish(&f, &format!("backlog-{i}"));
        sealed.push(seal(&f.rt));
    }
    let active = log(&f.rt).synced_lsn().sequence;
    let before = floor(&f.rt);
    let store = f.rt.shared.store.clone();
    let Fixture {
        rt,
        root,
        host,
        catalog,
        clock,
    } = f;
    drop(rt);

    // Reopen through the ordinary production constructor, with no test-only
    // replacement for the record log or the maintenance service.
    let rt = Runtime::new(
        store,
        host,
        catalog,
        Arc::new(NoHooks),
        None,
        config(&clock),
    )
    .unwrap();
    assert_eq!(floor(&rt), before);
    for &seq in &sealed {
        assert!(segment(&root, before.log_epoch, seq).exists());
    }
    assert_eq!(rt.next_wake_at(None).unwrap(), Some(clock.now_ms()));
    assert!(rt.enforce_deadlines().unwrap().is_empty());
    let after = floor(&rt);
    assert_eq!(after.first_retained, active);
    assert!(after.first_retained > before.first_retained);
    assert!(after.live_starts.is_empty());
    for &seq in &sealed {
        assert!(
            !segment(&root, after.log_epoch, seq).exists(),
            "startup maintenance left sealed segment {seq}"
        );
    }
    assert!(segment(&root, after.log_epoch, active).exists());
    assert_eq!(
        rt.next_wake_at(None).unwrap(),
        Some(clock.now_ms() + 3_600_000)
    );
}
