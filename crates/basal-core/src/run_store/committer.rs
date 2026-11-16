use super::{TerminalWrite, lock_order};
use crate::{
    CoreError, Result, Store,
    hooks::{Boundary, Hooks, Step},
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The longest a terminal group stays open for closers that have announced
/// they are about to commit. A group with no such closer commits at once.
const GROUP_WAIT: Duration = Duration::from_millis(25);
/// A savepoint that keeps failing is a deterministic fault in that run's
/// projection, not contention. Retrying it forever would spin the committer
/// while its closer holds the run's actor and its flow's accounting lock, so
/// after this many attempts the error goes back to the closer instead.
const MAX_ATTEMPTS: u32 = 3;

struct Pending {
    run_id: String,
    flow: String,
    write: Box<dyn TerminalWrite>,
    reply: mpsc::SyncSender<Result<()>>,
    attempts: u32,
}

enum Message {
    Commit(Pending),
    /// An announced closer of this flow returned without committing.
    Withdrawn { flow: String },
}

/// Closers that announced a commit the committer thread has not yet received,
/// counted per flow.
type Announced = Arc<Mutex<BTreeMap<String, usize>>>;

pub(super) struct Committer {
    sender: Option<mpsc::Sender<Message>>,
    announced: Announced,
    thread: Option<JoinHandle<()>>,
}

/// A closer that has started closing a run and will commit its terminal
/// write soon, unless it returns early. While it is outstanding, a group
/// forming for another flow waits for it (up to [`GROUP_WAIT`]) so the two
/// share one transaction. Dropping it uncommitted withdraws the announcement.
pub(super) struct Closing<'a> {
    committer: &'a Committer,
    flow: Option<String>,
}

impl Closing<'_> {
    pub(super) fn commit(mut self, run_id: String, write: Box<dyn TerminalWrite>) -> Result<()> {
        let flow = self.flow.take().expect("an announcement commits once");
        self.committer.commit(run_id, flow, write)
    }
}

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        if let Some(flow) = self.flow.take()
            && let Some(sender) = &self.committer.sender
        {
            let _ = sender.send(Message::Withdrawn { flow });
        }
    }
}

impl Committer {
    pub(super) fn new(store: Arc<Store>, hooks: Arc<dyn Hooks>) -> Self {
        Self::with_log(store, hooks, None)
    }
    pub(super) fn with_log(
        store: Arc<Store>,
        hooks: Arc<dyn Hooks>,
        log: Option<Arc<dyn crate::run_admission::AdmissionLog>>,
    ) -> Self {
        Self::start(store, hooks, log, GROUP_WAIT)
    }
    fn start(
        store: Arc<Store>,
        hooks: Arc<dyn Hooks>,
        log: Option<Arc<dyn crate::run_admission::AdmissionLog>>,
        group_wait: Duration,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let announced = Announced::default();
        let shared = announced.clone();
        let thread =
            std::thread::spawn(move || run(store, hooks, receiver, log, &shared, group_wait));
        Self {
            sender: Some(sender),
            announced,
            thread: Some(thread),
        }
    }
    /// Call when a closer starts closing a run of `flow`, before it waits for
    /// the run's dispatches, accounting lock and log sync.
    pub(super) fn announce(&self, flow: &str) -> Closing<'_> {
        *self
            .announced
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(flow.to_owned())
            .or_default() += 1;
        Closing {
            committer: self,
            flow: Some(flow.to_owned()),
        }
    }
    fn commit(&self, run_id: String, flow: String, write: Box<dyn TerminalWrite>) -> Result<()> {
        let (reply, result) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .unwrap()
            .send(Message::Commit(Pending {
                run_id,
                flow,
                write,
                reply,
                attempts: 0,
            }))
            .map_err(|_| CoreError::Unavailable("terminal committer stopped".into()))?;
        result
            .recv()
            .map_err(|_| CoreError::Unavailable("terminal committer stopped".into()))?
    }
}

impl Drop for Committer {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn at(store: &Store, hooks: &dyn Hooks, group: &[Pending], boundary: Boundary) -> Result<()> {
    lock_order::assert_lock_free();
    for pending in group {
        if hooks.at(&pending.run_id, &boundary) == Step::Crash {
            store.cut();
            return Err(CoreError::Cut);
        }
    }
    Ok(())
}

fn run(
    store: Arc<Store>,
    hooks: Arc<dyn Hooks>,
    receiver: mpsc::Receiver<Message>,
    log: Option<Arc<dyn crate::run_admission::AdmissionLog>>,
    announced: &Mutex<BTreeMap<String, usize>>,
    group_wait: Duration,
) {
    // Each received message settles one announcement of its flow. Messages
    // queued without an announcement (tests) find no count to settle.
    let receive = |message: Message, group: &mut Vec<Pending>| {
        let flow = match &message {
            Message::Commit(pending) => &pending.flow,
            Message::Withdrawn { flow } => flow,
        };
        let mut counts = announced.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(count) = counts.get_mut(flow) {
            *count -= 1;
            if *count == 0 {
                counts.remove(flow);
            }
        }
        drop(counts);
        if let Message::Commit(pending) = message {
            group.push(pending);
        }
    };
    // Another flow's closer is still on its way to commit. A closer of a flow
    // already in the group cannot be: each group member's closer holds its
    // flow's accounting lock until its reply, so waiting for one would only
    // delay this group by the whole window.
    let others_announced = |group: &[Pending]| {
        let flows: BTreeSet<&str> = group.iter().map(|p| p.flow.as_str()).collect();
        announced
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .any(|flow| !flows.contains(flow.as_str()))
    };
    let mut retry = Vec::new();
    loop {
        let mut group = std::mem::take(&mut retry);
        if group.is_empty() {
            match receiver.recv() {
                Ok(first) => receive(first, &mut group),
                Err(_) => return,
            }
            if group.is_empty() {
                continue;
            }
        }
        // Take every commit that has already arrived, then keep the group open
        // only while another flow's closer has announced its commit. With
        // none, a lone completion commits at once instead of waiting out the
        // window; concurrent completions still share one transaction.
        let until = Instant::now() + group_wait;
        loop {
            while let Ok(message) = receiver.try_recv() {
                receive(message, &mut group);
            }
            if !others_announced(&group) {
                break;
            }
            let Some(left) = until.checked_duration_since(Instant::now()) else {
                break;
            };
            match receiver.recv_timeout(left) {
                Ok(message) => receive(message, &mut group),
                Err(_) => break,
            }
        }
        let committed = at(&store, hooks.as_ref(), &group, Boundary::RunStoreBuilt).and_then(|()| {
            store.write(|tx| {
                if let Some(log) = &log { crate::run_store_sql::advance_log_hwm(tx, log.synced_lsn())?; }
                let mut passed = Vec::with_capacity(group.len());
                for pending in &group {
                    tx.execute_batch("SAVEPOINT terminal_run")?;
                    match pending.write.apply(tx) {
                        Ok(()) => { tx.execute_batch("RELEASE terminal_run")?; passed.push(Ok(())); }
                        Err(CoreError::Cut) => return Err(CoreError::Cut),
                        Err(error) => {
                            tx.execute_batch("ROLLBACK TO terminal_run; RELEASE terminal_run")?;
                            tracing::warn!(run_id = pending.run_id, attempt = pending.attempts + 1, %error, "terminal savepoint rolled back");
                            passed.push(Err(error));
                        }
                    }
                }
                if let Some(log) = &log { crate::run_store_sql::advance_log_hwm(tx, log.synced_lsn())?; }
                Ok(passed)
            })
        }).and_then(|passed| {
            lock_order::assert_lock_free();
            for (pending, passed) in group.iter().zip(&passed) {
                if passed.is_ok() && hooks.at(&pending.run_id, &Boundary::RunStoreCommitted) == Step::Crash {
                    store.cut();
                    return Err(CoreError::Cut);
                }
            }
            Ok(passed)
        });
        match committed {
            Ok(passed) => {
                for (mut pending, passed) in group.into_iter().zip(passed) {
                    match passed {
                        Ok(()) => {
                            let _ = pending.reply.send(Ok(()));
                        }
                        Err(error) if pending.attempts + 1 >= MAX_ATTEMPTS => {
                            let _ = pending.reply.send(Err(error));
                        }
                        Err(_) => {
                            pending.attempts += 1;
                            retry.push(pending);
                        }
                    }
                }
            }
            Err(error) => {
                for pending in group {
                    let _ = pending.reply.send(Err(error.clone()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn queued_group_has_one_savepoint_per_run_and_retries_only_the_failure() {
        let dir = crate::test_support::timestamp_scratch("run-store", "queued-group");
        let store =
            Arc::new(Store::open(dir.join("core.db"), crate::Durability::throwaway()).unwrap());
        store
            .write(|tx| {
                tx.execute_batch("CREATE TABLE charges(run_id TEXT PRIMARY KEY, amount INTEGER)")?;
                Ok(())
            })
            .unwrap();
        struct Write(&'static str, AtomicBool);
        impl TerminalWrite for Write {
            fn apply(&self, tx: &rusqlite::Transaction<'_>) -> Result<()> {
                tx.execute("INSERT INTO charges VALUES(?1,3)", [self.0])?;
                if self.1.swap(false, Ordering::SeqCst) {
                    return Err(CoreError::Invalid("retry".into()));
                }
                Ok(())
            }
        }
        #[derive(Default)]
        struct Trace(Mutex<Vec<(String, Boundary)>>);
        impl Hooks for Trace {
            fn at(&self, run_id: &str, boundary: &Boundary) -> Step {
                lock_order::assert_lock_free();
                self.0
                    .lock()
                    .unwrap()
                    .push((run_id.into(), boundary.clone()));
                Step::Continue
            }
        }
        let hooks = Arc::new(Trace::default());
        let (sender, receiver) = mpsc::channel();
        let (a, ar) = mpsc::sync_channel(1);
        let (b, br) = mpsc::sync_channel(1);
        // Prequeue both runs before starting the worker: group membership does
        // not depend on the scheduler racing the 25 ms grouping deadline.
        sender
            .send(Message::Commit(Pending {
                run_id: "a".into(),
                flow: "flow".into(),
                write: Box::new(Write("a", AtomicBool::new(true))),
                reply: a,
                attempts: 0,
            }))
            .unwrap();
        sender
            .send(Message::Commit(Pending {
                run_id: "b".into(),
                flow: "flow".into(),
                write: Box::new(Write("b", AtomicBool::new(false))),
                reply: b,
                attempts: 0,
            }))
            .unwrap();
        drop(sender);
        let (s, h) = (store.clone(), hooks.clone());
        let worker = std::thread::spawn(move || run(s, h, receiver, None, &Announced::default(), GROUP_WAIT));
        br.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        ar.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        worker.join().unwrap();
        assert_eq!(
            *hooks.0.lock().unwrap(),
            vec![
                ("a".into(), Boundary::RunStoreBuilt),
                ("b".into(), Boundary::RunStoreBuilt),
                ("b".into(), Boundary::RunStoreCommitted),
                ("a".into(), Boundary::RunStoreBuilt),
                ("a".into(), Boundary::RunStoreCommitted),
            ]
        );
        store
            .read(|c| {
                assert_eq!(
                    c.query_row::<i64, _, _>("SELECT sum(amount) FROM charges", [], |r| r.get(0))?,
                    6
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(GROUP_WAIT, Duration::from_millis(25));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_savepoint_that_always_fails_returns_its_error_after_bounded_attempts() {
        let dir = crate::test_support::timestamp_scratch("run-store", "always-fails");
        let store =
            Arc::new(Store::open(dir.join("core.db"), crate::Durability::throwaway()).unwrap());
        struct Fails(Arc<Mutex<u32>>);
        impl TerminalWrite for Fails {
            fn apply(&self, _tx: &rusqlite::Transaction<'_>) -> Result<()> {
                *self.0.lock().unwrap() += 1;
                Err(CoreError::Invalid("projection rejected".into()))
            }
        }
        let calls = Arc::new(Mutex::new(0));
        let (sender, receiver) = mpsc::channel();
        let (reply, result) = mpsc::sync_channel(1);
        sender
            .send(Message::Commit(Pending {
                run_id: "a".into(),
                flow: "flow".into(),
                write: Box::new(Fails(calls.clone())),
                reply,
                attempts: 0,
            }))
            .unwrap();
        drop(sender);
        let s = store.clone();
        let worker = std::thread::spawn(move || {
            run(
                s,
                Arc::new(crate::NoHooks),
                receiver,
                None,
                &Announced::default(),
                GROUP_WAIT,
            )
        });
        let outcome = result.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert_eq!(
            outcome,
            Err(CoreError::Invalid("projection rejected".into()))
        );
        assert_eq!(*calls.lock().unwrap(), MAX_ATTEMPTS);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[derive(Default)]
    struct Order(Mutex<Vec<(String, Boundary)>>);
    impl Hooks for Order {
        fn at(&self, run_id: &str, boundary: &Boundary) -> Step {
            self.0
                .lock()
                .unwrap()
                .push((run_id.into(), boundary.clone()));
            Step::Continue
        }
    }
    struct Noop;
    impl TerminalWrite for Noop {
        fn apply(&self, _: &rusqlite::Transaction<'_>) -> Result<()> {
            Ok(())
        }
    }
    /// A committer whose group window is far longer than any test, so a
    /// commit that returns promptly cannot have waited the window out.
    fn committer(name: &str) -> (Committer, Arc<Order>, std::path::PathBuf) {
        let dir = crate::test_support::timestamp_scratch("run-store", name);
        let store =
            Arc::new(Store::open(dir.join("core.db"), crate::Durability::throwaway()).unwrap());
        let hooks = Arc::new(Order::default());
        let committer = Committer::start(store, hooks.clone(), None, Duration::from_secs(60));
        (committer, hooks, dir)
    }
    /// Commits on another thread and reports whether it finished in time.
    fn commit_within<'scope, 'env>(
        scope: &'scope std::thread::Scope<'scope, 'env>,
        closing: Closing<'env>,
        run_id: &str,
    ) -> mpsc::Receiver<Result<()>> {
        let (done, finished) = mpsc::channel();
        let run_id = run_id.to_owned();
        scope.spawn(move || {
            let _ = done.send(closing.commit(run_id, Box::new(Noop)));
        });
        finished
    }
    const PROMPT: Duration = Duration::from_secs(10);

    #[test]
    fn a_lone_completion_commits_without_waiting_out_the_group_window() {
        let (committer, hooks, dir) = committer("lone-completion");
        std::thread::scope(|scope| {
            let done = commit_within(scope, committer.announce("flow"), "a");
            assert_eq!(done.recv_timeout(PROMPT), Ok(Ok(())));
        });
        assert_eq!(
            *hooks.0.lock().unwrap(),
            vec![
                ("a".into(), Boundary::RunStoreBuilt),
                ("a".into(), Boundary::RunStoreCommitted),
            ]
        );
        drop(committer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn announced_completions_of_other_flows_share_one_transaction() {
        let (committer, hooks, dir) = committer("announced-group");
        let a = committer.announce("flow-a");
        let b = committer.announce("flow-b");
        std::thread::scope(|scope| {
            let first = commit_within(scope, a, "a");
            // The second closer is still on its way; the group waits for it.
            std::thread::sleep(Duration::from_millis(50));
            assert!(first.try_recv().is_err());
            let second = commit_within(scope, b, "b");
            assert_eq!(first.recv_timeout(PROMPT), Ok(Ok(())));
            assert_eq!(second.recv_timeout(PROMPT), Ok(Ok(())));
        });
        assert_eq!(
            *hooks.0.lock().unwrap(),
            vec![
                ("a".into(), Boundary::RunStoreBuilt),
                ("b".into(), Boundary::RunStoreBuilt),
                ("a".into(), Boundary::RunStoreCommitted),
                ("b".into(), Boundary::RunStoreCommitted),
            ]
        );
        drop(committer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_same_flow_or_withdrawn_announcement_does_not_hold_the_group() {
        let (committer, hooks, dir) = committer("same-flow");
        // A second closer of the same flow waits for the first one's
        // accounting lock, so it cannot join; the first commits alone.
        let later = committer.announce("flow");
        let withdrawn = committer.announce("other");
        std::thread::scope(|scope| {
            let first = commit_within(scope, committer.announce("flow"), "a");
            std::thread::sleep(Duration::from_millis(50));
            drop(withdrawn);
            assert_eq!(first.recv_timeout(PROMPT), Ok(Ok(())));
            let second = commit_within(scope, later, "b");
            assert_eq!(second.recv_timeout(PROMPT), Ok(Ok(())));
        });
        assert_eq!(
            *hooks.0.lock().unwrap(),
            vec![
                ("a".into(), Boundary::RunStoreBuilt),
                ("a".into(), Boundary::RunStoreCommitted),
                ("b".into(), Boundary::RunStoreBuilt),
                ("b".into(), Boundary::RunStoreCommitted),
            ]
        );
        drop(committer);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
