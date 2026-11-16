use super::*;
use crate::{CoreError, Durability, Store};
use std::future::Future;
use std::task::Context;

fn store(name: &str) -> Store {
    let directory = crate::test_support::timestamp_scratch("overview-changes", name);
    Store::open(directory.join("store.db"), Durability::throwaway()).unwrap()
}

fn changed(subscription: &mut OverviewSubscription) -> bool {
    std::pin::pin!(subscription.changed())
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_ready()
}

#[test]
fn overview_commits_filter_tables_and_remain_asleep_without_subscribers() {
    let store = store("filter");
    let signal = store.overview_changes();
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO flows(flow_id,created_at) VALUES ('synthetic',1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(
        signal.dirty.load(Ordering::Relaxed),
        "without watches the flag is not inspected or cleared"
    );
    assert_eq!(signal.state.lock().unwrap().generation, 0);
    let mut subscription = signal.subscribe();
    assert!(
        !signal.take_dirty(),
        "the first subscription covers old writes with its initial read"
    );
    assert!(!changed(&mut subscription));
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO meta(key,value) VALUES ('synthetic','unobserved')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(
        !changed(&mut subscription),
        "unobserved tables must not wake the overview"
    );
    store
        .write(|tx| {
            tx.execute(
                "UPDATE flows SET state='disabled', disabled_by='operator:synthetic' WHERE flow_id='synthetic'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(changed(&mut subscription));
    assert!(
        !changed(&mut subscription),
        "one committed transaction publishes one generation"
    );
}

#[test]
fn overview_rolled_back_write_publishes_no_wake() {
    let store = store("rollback");
    let signal = store.overview_changes();
    let mut subscription = signal.subscribe();
    signal.take_dirty();
    assert!(!changed(&mut subscription));
    let result: crate::Result<()> = store.write(|tx| {
        tx.execute(
            "INSERT INTO flows(flow_id,created_at) VALUES ('rolled-back',1)",
            [],
        )?;
        Err(CoreError::Invalid("synthetic rollback".into()))
    });
    assert!(result.is_err());
    assert!(!changed(&mut subscription));
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO meta(key,value) VALUES ('after-rollback','unobserved')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(
        !changed(&mut subscription),
        "a later commit must not publish the rolled-back flag"
    );
}

#[test]
fn overview_reconcile_transitions_wake_but_ordinary_records_do_not() {
    use crate::run_records::{Body, CallIdentity, Record, RecordLog, Records, Resolution};
    use crate::run_store::RunStore;
    struct Log;
    impl RecordLog for Log {
        fn append(&self, _: Record) -> crate::Result<crate::run_log::Lsn> {
            Ok(Default::default())
        }
        fn sync_through(&self, _: crate::run_log::Lsn) -> crate::Result<()> {
            Ok(())
        }
        fn reclaim(&self, _: BTreeMap<String, crate::run_log::Lsn>) -> crate::Result<usize> {
            Ok(0)
        }
    }
    let store = Arc::new(store("reconcile"));
    let registry = RunStore::new_staged(store.clone(), Arc::new(crate::NoHooks));
    let run = registry
        .insert(
            "run-synthetic".into(),
            "flow-synthetic".into(),
            0,
            Records::new("run-synthetic".into(), 0, Arc::new(Log)),
        )
        .unwrap();
    let mut subscription = store.overview_changes().subscribe();
    let append = |body| {
        run.enter()
            .unwrap()
            .finish(|records| records.append(0, body).map(|_| ()))
            .unwrap()
    };
    append(Body::Issued(CallIdentity::new(
        basal_proto::HostCall {
            position: 0,
            kind: basal_proto::CallKind::Op {
                module: "synthetic".into(),
                op: "write".into(),
            },
            args: basal_proto::JsonText::null(),
        },
        crate::StoredClass::Mutation,
    )));
    assert!(
        !changed(&mut subscription),
        "ordinary identity append is quiet"
    );
    append(Body::Unknown {
        position: 0,
        reason: "synthetic".into(),
    });
    assert!(
        changed(&mut subscription),
        "entering NeedsReconcile wakes the overview"
    );
    append(Body::Woken);
    assert!(
        !changed(&mut subscription),
        "ordinary append while needing reconcile is quiet"
    );
    append(Body::Unknown {
        position: 0,
        reason: "synthetic retry".into(),
    });
    assert!(
        !changed(&mut subscription),
        "remaining in NeedsReconcile is quiet"
    );
    append(Body::Resolution(Resolution {
        position: 0,
        attempts: 1,
        idempotency_key: None,
        choice: "not_applied".into(),
        observed_outcome: None,
        usage: None,
        elicitation_id: "synthetic".into(),
    }));
    assert!(
        changed(&mut subscription),
        "leaving NeedsReconcile wakes the overview"
    );
}
