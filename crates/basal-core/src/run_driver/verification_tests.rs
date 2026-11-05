use super::*;
use std::sync::mpsc;

struct Pause {
    boundary: Boundary,
    reached: mpsc::Sender<()>,
    resume: Mutex<mpsc::Receiver<()>>,
}
impl Hooks for Pause {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        lock_order::assert_lock_free();
        if *boundary == self.boundary {
            self.reached.send(()).unwrap();
            self.resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        Step::Continue
    }
}

fn pause(boundary: Boundary) -> (Arc<Pause>, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (reached, at) = mpsc::channel();
    let (resume, wait) = mpsc::channel();
    (
        Arc::new(Pause {
            boundary,
            reached,
            resume: Mutex::new(wait),
        }),
        at,
        resume,
    )
}

#[test]
fn dispatch_return_at_the_closing_cut_is_in_the_terminal_projection() {
    let f = Fixture::new();
    let (hooks, reached, resume) = pause(Boundary::DriverDispatched);
    let driver = Driver::new_staged(
        f.store.clone(),
        f.registry.clone(),
        f.service.clone(),
        hooks,
        budgets(),
    );
    let Claim::Active(a) = driver.claim(f.live.clone(), 0).unwrap() else {
        panic!("gate closed")
    };
    let Issued::Dispatch(dispatch) = a.issue(call(0, "write")).unwrap() else {
        panic!("not remote")
    };
    let returning = std::thread::spawn(move || dispatch.send());
    reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let (stopped_tx, stopped_rx) = mpsc::channel();
    *f.service.stopped.lock().unwrap() = Some(stopped_tx);
    let terminal = f.terminal(RunState::Cancelled);
    let (done_tx, done_rx) = mpsc::channel();
    let closer = std::thread::spawn(move || done_tx.send(driver.cancel(terminal)).unwrap());
    stopped_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let run = f.registry.get(&f.live.run_id).unwrap();
    assert_eq!(run.routing(), crate::run_store::Routing::Closing);
    assert!(matches!(
        run.enter(),
        Err(crate::run_store::Routing::Closing)
    ));
    assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    resume.send(()).unwrap();
    returning.join().unwrap().unwrap();
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    closer.join().unwrap();
    f.store
        .read(|c| {
            let call = sql::call(c, &f.live.run_id, 0)?.unwrap();
            assert_eq!(call.outcome, sql::CallOutcome::Fulfilled);
            let detail: crate::run_records::CallState =
                serde_json::from_value(call.detail).unwrap();
            assert_eq!(detail.arrival.unwrap().value.as_str(), "42");
            assert_eq!(
                sql::history(c, &f.live.run_id)?.unwrap().state,
                RunState::Cancelled
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(f.service.sent.lock().unwrap().len(), 1);
    assert_eq!(*f.service.acks.lock().unwrap(), vec![0]);
}

#[test]
fn cancel_racing_a_committed_group_cannot_project_or_fold_twice() {
    let f = Fixture::new();
    let (hooks, reached, resume) = pause(Boundary::RunStoreCommitted);
    let registry = Arc::new(RunStore::new_staged(f.store.clone(), hooks));
    let driver = Driver::new_staged(
        f.store.clone(),
        registry.clone(),
        f.service.clone(),
        Arc::new(NoHooks),
        budgets(),
    );
    driver
        .register(
            &f.live,
            Records::new(f.live.run_id.clone(), 10, f.log.clone()),
        )
        .unwrap();
    let Claim::Active(a) = driver.claim(f.live.clone(), 0).unwrap() else {
        panic!("gate closed")
    };
    f.send(&a, 0, "keyed");
    let terminal = f.terminal(RunState::Failed);
    let (done_tx, done_rx) = mpsc::channel();
    let finishing = std::thread::spawn(move || done_tx.send(a.finish(terminal)).unwrap());
    reached.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        registry.get(&f.live.run_id).unwrap().routing(),
        crate::run_store::Routing::Closing
    );
    assert!(
        matches!(driver.cancel(f.terminal(RunState::Cancelled)), Err(CoreError::Unavailable(message)) if message == "another closer is active")
    );
    resume.send(()).unwrap();
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    finishing.join().unwrap();
    assert!(registry.get(&f.live.run_id).is_none());
    f.store
        .read(|c| {
            assert_eq!(
                sql::history(c, &f.live.run_id)?.unwrap().state,
                RunState::Failed
            );
            assert_eq!(
                c.query_row("SELECT count(*) FROM terminal_effects", [], |r| r
                    .get::<_, u64>(0))?,
                1
            );
            assert_eq!(
                c.query_row(
                    "SELECT dispatches FROM rate_windows WHERE flow_id='flow'",
                    [],
                    |r| r.get::<_, u64>(0)
                )?,
                1
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn stale_recovery_snapshot_cannot_retry_a_completed_or_accepted_dispatch() {
    for (op, accepted) in [
        ("query", false),
        ("keyed", false),
        ("write", false),
        ("query", true),
        ("keyed", true),
        ("write", true),
    ] {
        let f = Fixture::new();
        let a = f.claim(0);
        if accepted {
            f.service
                .replies
                .lock()
                .unwrap()
                .push_back(Reply::Accepted("handle".into()));
        }
        let Issued::Dispatch(dispatch) = a.issue(call(0, op)).unwrap() else {
            panic!("not remote")
        };
        // Finish the in-flight send after recovery copied its snapshot.
        let recovered = a
            .recover_dispatches_after_snapshot(false, || dispatch.send().unwrap())
            .unwrap();
        assert!(
            recovered.is_empty(),
            "{op}, accepted={accepted}: stale scan authorized a duplicate"
        );
        assert_eq!(f.service.sent.lock().unwrap().len(), 1);
        assert_eq!(
            a.state().unwrap().calls[&0]
                .intent
                .as_ref()
                .unwrap()
                .attempts,
            1
        );
        assert!(a.state().unwrap().calls[&0].unknown.is_none());
    }
}
