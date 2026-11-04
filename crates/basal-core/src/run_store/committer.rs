use super::{TerminalWrite, lock_order};
use crate::{
    CoreError, Result, Store,
    hooks::{Boundary, Hooks, Step},
};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const GROUP_WAIT: Duration = Duration::from_millis(25);

struct Pending {
    run_id: String,
    write: Box<dyn TerminalWrite>,
    reply: mpsc::SyncSender<Result<()>>,
}

pub(super) struct Committer {
    sender: Option<mpsc::Sender<Pending>>,
    thread: Option<JoinHandle<()>>,
}

impl Committer {
    pub(super) fn new(store: Arc<Store>, hooks: Arc<dyn Hooks>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || run(store, hooks, receiver));
        Self {
            sender: Some(sender),
            thread: Some(thread),
        }
    }
    pub(super) fn commit(&self, run_id: String, write: Box<dyn TerminalWrite>) -> Result<()> {
        let (reply, result) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .unwrap()
            .send(Pending {
                run_id,
                write,
                reply,
            })
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

fn run(store: Arc<Store>, hooks: Arc<dyn Hooks>, receiver: mpsc::Receiver<Pending>) {
    let mut retry = Vec::new();
    loop {
        let mut group = std::mem::take(&mut retry);
        if group.is_empty() {
            match receiver.recv() {
                Ok(first) => group.push(first),
                Err(_) => return,
            }
        }
        let until = Instant::now() + GROUP_WAIT;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match receiver.recv_timeout(left) {
                Ok(pending) => group.push(pending),
                Err(_) => break,
            }
        }
        let committed = at(&store, hooks.as_ref(), &group, Boundary::RunStoreBuilt).and_then(|()| {
            store.write(|tx| {
                let mut passed = Vec::with_capacity(group.len());
                for pending in &group {
                    tx.execute_batch("SAVEPOINT terminal_run")?;
                    match pending.write.apply(tx) {
                        Ok(()) => { tx.execute_batch("RELEASE terminal_run")?; passed.push(true); }
                        Err(CoreError::Cut) => return Err(CoreError::Cut),
                        Err(error) => {
                            tx.execute_batch("ROLLBACK TO terminal_run; RELEASE terminal_run")?;
                            tracing::warn!(run_id = pending.run_id, %error, "terminal savepoint rolled back; retrying");
                            passed.push(false);
                        }
                    }
                }
                Ok(passed)
            })
        }).and_then(|passed| {
            lock_order::assert_lock_free();
            for (pending, passed) in group.iter().zip(&passed) {
                if *passed && hooks.at(&pending.run_id, &Boundary::RunStoreCommitted) == Step::Crash {
                    store.cut();
                    return Err(CoreError::Cut);
                }
            }
            Ok(passed)
        });
        match committed {
            Ok(passed) => {
                for (pending, passed) in group.into_iter().zip(passed) {
                    if passed {
                        let _ = pending.reply.send(Ok(()));
                    } else {
                        retry.push(pending);
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
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

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
            .send(Pending {
                run_id: "a".into(),
                write: Box::new(Write("a", AtomicBool::new(true))),
                reply: a,
            })
            .unwrap();
        sender
            .send(Pending {
                run_id: "b".into(),
                write: Box::new(Write("b", AtomicBool::new(false))),
                reply: b,
            })
            .unwrap();
        drop(sender);
        let (s, h) = (store.clone(), hooks.clone());
        let worker = std::thread::spawn(move || run(s, h, receiver));
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
}
