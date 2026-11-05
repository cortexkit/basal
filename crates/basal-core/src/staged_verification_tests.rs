use super::*;
use crate::{
    Boundary, Hooks, Step,
    run_log::{self, RunLog},
    run_records::Record,
    run_store_sql as sql,
};

#[derive(Debug, Clone, Copy)]
enum RestoreCase {
    Absent,
    Foreign,
    Older,
    Newer,
}

struct Cut(Boundary);
impl Hooks for Cut {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        crate::run_store::lock_order::assert_lock_free();
        if boundary == &self.0 {
            Step::Crash
        } else {
            Step::Continue
        }
    }
}

fn log_config(root: &std::path::Path, store: &Store, hooks: Arc<dyn Hooks>) -> run_log::Config {
    run_log::Config {
        directory: root.join("run-log"),
        identity: run_log::Identity {
            store_id: store.store_id().as_bytes().try_into().unwrap(),
            log_epoch: 1,
        },
        lease_epoch: store.lease_epoch(),
        target_size: run_log::Config::TARGET_SIZE,
        hooks,
    }
}

fn restore_case(case: RestoreCase, cut: Option<Boundary>) {
    let f = Fixture::new(false);
    let mut ids = Vec::new();
    for flow in ["cancel-lost", "retrigger-lost"] {
        f.install(flow, 1, json!({"ops":[{"module":"test","op":"write"}]}));
        let id = f.admit(flow, flow);
        f.host.accept.store(true, Ordering::SeqCst);
        let mut worker = ScriptedWorker::new(vec![
            op(0, "write", json!({})),
            WorkerMessage::Blocked { awaiting: vec![0] },
            suspended(vec![0]),
        ]);
        let activated = f.rt.activate(&id, &mut worker).unwrap();
        assert!(
            matches!(activated, ActivationEnd::Suspended { .. }),
            "{activated:?}"
        );
        ids.push(id);
    }
    assert_eq!(f.host.sent.lock().unwrap().len(), 2);
    // The production Broca table still references SQLite's journal table until
    // execution switches to run-log identities. Model its durable saved handles
    // against the new identities without creating journal rows merely to satisfy
    // that temporary foreign key.
    f.rt.store().write(|tx| {
        tx.execute("DROP TABLE broca_calls", [])?;
        tx.execute_batch("CREATE TABLE broca_calls(send_id TEXT PRIMARY KEY,run_id TEXT,position INTEGER,snapshot TEXT)")?;
        for id in &ids {
            tx.execute("INSERT INTO broca_calls VALUES(?1,?1,0,?2)", rusqlite::params![id, json!({"params":"{}","handle":format!("handle:{id}"),"acknowledged":false}).to_string()])?;
        }
        Ok(())
    }).unwrap();
    let Fixture {
        rt,
        root,
        host,
        catalog,
        clock,
    } = f;
    rt.quiesce();
    drop(rt);
    let store = Arc::new(Store::open(root.join("store.db"), Durability::default()).unwrap());
    let before_epoch = store.read(sql::meta).unwrap().log_epoch;
    let original_rows = store
        .read(|c| Ok(c.query_row("SELECT count(*) FROM live_runs", [], |r| r.get::<_, u64>(0))?))
        .unwrap();
    assert_eq!(original_rows, 2);
    let epoch_dir = root.join("run-log").join(format!("e{before_epoch}"));
    match case {
        RestoreCase::Absent => std::fs::remove_dir_all(root.join("run-log")).unwrap(),
        RestoreCase::Foreign => {
            // A complete foreign epoch directory is restore evidence, rather
            // than damage within the current epoch's durable prefix.
            std::fs::rename(&epoch_dir, root.join("run-log/e99")).unwrap();
        }
        RestoreCase::Older => {
            let hwm = store.read(sql::meta).unwrap().log_hwm.lsn;
            // The second admission commits a watermark inside a valid record
            // stream. Rewind to the first admission's known record boundary.
            let first = store
                .read(|c| Ok(sql::live(c, &ids[0])?.unwrap().admit_lsn.lsn))
                .unwrap();
            assert!(hwm > first);
            std::fs::OpenOptions::new()
                .write(true)
                .open(epoch_dir.join(format!("{:016x}.seg", first.sequence)))
                .unwrap()
                .set_len(first.offset)
                .unwrap();
        }
        RestoreCase::Newer => {
            let spare = store
                .read(|c| sql::segments(c, before_epoch))
                .unwrap()
                .into_iter()
                .find(|s| s.state == run_log::SegmentState::Reserved)
                .unwrap();
            std::fs::copy(
                epoch_dir.join(format!("{:016x}.seg", spare.seq)),
                epoch_dir.join("0000000000000099.seg"),
            )
            .unwrap();
        }
    }
    if let Some(boundary) = cut {
        assert!(matches!(
            RunLog::<Record>::open_restoring(
                log_config(&root, &store, Arc::new(Cut(boundary))),
                store.clone()
            ),
            Err(run_log::Error::Cut)
        ));
    }
    let (log, recovered) = RunLog::<Record>::open_restoring(
        log_config(&root, &store, Arc::new(NoHooks)),
        store.clone(),
    )
    .unwrap();
    assert!(recovered.records.is_empty());
    assert_eq!(store.read(sql::meta).unwrap().log_epoch, before_epoch + 1);
    drop(log);
    let meta = store.read(sql::meta).unwrap();
    let (log, recovered) = RunLog::<Record>::open_restoring(
        log_config(&root, &store, Arc::new(NoHooks)),
        store.clone(),
    )
    .unwrap();
    assert_eq!(
        store.read(sql::meta).unwrap(),
        meta,
        "repeat boot must not reinitialize or advance epoch"
    );
    let log = Arc::new(log);
    let hosts = Arc::new(
        crate::run_host::RecoveredHosts::recover(
            store.clone(),
            log.clone(),
            Arc::new(log.watermark()),
            &recovered.records,
            run_driver::Budgets {
                dispatch_window_ms: 60_000,
                token_window_ms: 86_400_000,
                dispatches: 1000,
                tokens: u64::MAX,
                cost_micros: u64::MAX,
                cost_window: None,
            },
            Arc::new(NoHooks),
            clock.clone(),
        )
        .unwrap(),
    );
    assert!(hosts.registry().snapshot().is_empty());
    let rt = Runtime::new_staged(
        hosts.clone(),
        host.clone(),
        catalog,
        Arc::new(NoHooks),
        None,
        config(&clock),
    )
    .unwrap();
    rt.recover().unwrap();
    assert!(rt.startable().unwrap().is_empty());
    for id in &ids {
        let run = rt.run(id).unwrap();
        assert_eq!(
            run.error_kind.as_deref(),
            Some("history_lost"),
            "{case:?}: diagnostic projection"
        );
        assert!(store.read(|c| sql::history_lost(c, id)).unwrap());
    }
    assert_eq!(
        host.sent.lock().unwrap().len(),
        2,
        "restore must never automatically resend an unkeyed write"
    );
    assert!(
        rt.reconcile_history_lost(&ids[0], false, "operator")
            .unwrap()
            .is_none()
    );
    let replacement = rt
        .reconcile_history_lost(&ids[1], true, "operator")
        .unwrap()
        .unwrap();
    let replacement_id = replacement.run_id().unwrap();
    assert_ne!(replacement_id, ids[1]);
    assert!(hosts.registry().get(replacement_id).is_some());
    let registered = hosts.registry().snapshot().len() as u64;
    store
        .read(|c| {
            for id in &ids {
                assert!(sql::live(c, id)?.is_none());
                let history = sql::history(c, id)?.unwrap();
                assert!(history.history_lost);
                assert_eq!(history.state, RunState::Cancelled);
                assert!(history.error_detail.unwrap().contains("under-count"));
                let obligation = sql::obligation(c, id, 0)?.unwrap();
                assert_eq!(
                    obligation.handle.as_deref(),
                    Some(format!("handle:{id}").as_str())
                );
            }
            assert_eq!(
                c.query_row(
                    "SELECT count(*) FROM live_runs WHERE history_lost=0",
                    [],
                    |r| r.get::<_, u64>(0)
                )?,
                registered
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(
        host.sent.lock().unwrap().len(),
        2,
        "retrigger only admits a replacement; it does not resend the old call"
    );
}

#[test]
fn each_restore_cause_holds_lost_slots_until_cancel_or_retrigger() {
    for case in [
        RestoreCase::Absent,
        RestoreCase::Foreign,
        RestoreCase::Older,
        RestoreCase::Newer,
    ] {
        restore_case(case, None);
    }
}

pub(crate) fn crash_case(boundary: Boundary) {
    restore_case(RestoreCase::Absent, Some(boundary));
}
