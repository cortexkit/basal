//! Fixtures shared by core and module integration tests.

pub mod logs;

use std::sync::atomic::{AtomicU64, Ordering};

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create a process-local scratch directory with a time and sequence suffix.
/// This only creates the directory; it never removes a pre-existing path.
/// Each call returns a directory no other call has.
pub fn timestamp_scratch(prefix: &str, name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    create_scratch_dir(prefix, name, nanos, &SCRATCH_COUNTER)
}

/// A protocol peer for deterministic runtime tests. Completed frames wait for
/// every issued call's Deliver; suspended frames intentionally do not.
pub struct ScriptedWorker {
    welcome: basal_proto::Welcome,
    messages: std::collections::VecDeque<basal_proto::WorkerMessage>,
    pending: std::collections::BTreeSet<u64>,
    long_running: std::collections::BTreeSet<u64>,
    activation: u64,
    pub sent: Vec<basal_proto::ParentMessage>,
    pub killed: bool,
}

impl ScriptedWorker {
    pub fn new(messages: Vec<basal_proto::WorkerMessage>) -> Self {
        Self {
            welcome: basal_proto::Welcome {
                protocol_version: basal_proto::PROTOCOL_VERSION,
                engine: "runtime-test".into(),
                prelude_hash: basal_proto::PreludeHash([0; 32]),
                codemode_prelude_hash: basal_proto::PreludeHash([0; 32]),
                confinement: basal_proto::Confinement::None,
            },
            messages: messages.into(),
            pending: Default::default(),
            long_running: Default::default(),
            activation: 0,
            sent: Vec::new(),
            killed: false,
        }
    }
}

impl crate::channel::WorkerChannel for ScriptedWorker {
    fn welcome(&self) -> &basal_proto::Welcome {
        &self.welcome
    }
    fn send(
        &mut self,
        message: &basal_proto::ParentMessage,
    ) -> std::result::Result<(), crate::ChannelError> {
        match message {
            basal_proto::ParentMessage::Activate(request) => {
                self.activation = request.activation_id
            }
            basal_proto::ParentMessage::Deliver(outcome) => {
                self.pending.remove(&outcome.position);
            }
            basal_proto::ParentMessage::LongRunning { positions } => {
                self.long_running.extend(positions.iter().copied())
            }
            _ => {}
        }
        self.sent.push(message.clone());
        Ok(())
    }
    fn recv(
        &mut self,
        timeout: std::time::Duration,
    ) -> std::result::Result<basal_proto::WorkerMessage, crate::ChannelError> {
        if matches!(self.messages.front(), Some(basal_proto::WorkerMessage::Finished { result:basal_proto::ActivationResult::Suspended { awaited }, .. }) if awaited.iter().any(|p| !self.long_running.contains(p)))
        {
            std::thread::sleep(timeout.min(std::time::Duration::from_millis(1)));
            return Err(crate::ChannelError::Timeout);
        }
        if !self.pending.is_empty()
            && matches!(
                self.messages.front(),
                Some(basal_proto::WorkerMessage::Finished {
                    result: basal_proto::ActivationResult::Completed { .. },
                    ..
                })
            )
        {
            std::thread::sleep(timeout.min(std::time::Duration::from_millis(1)));
            return Ok(basal_proto::WorkerMessage::Blocked {
                awaiting: self.pending.iter().copied().collect(),
            });
        }
        let mut message = self
            .messages
            .pop_front()
            .ok_or(crate::ChannelError::Closed)?;
        match &mut message {
            basal_proto::WorkerMessage::HostCall(call) => {
                self.pending.insert(call.position);
            }
            basal_proto::WorkerMessage::Blocked { awaiting } => {
                self.pending.extend(awaiting.iter().copied())
            }
            basal_proto::WorkerMessage::Finished { activation_id, .. } if *activation_id == 0 => {
                *activation_id = self.activation
            }
            _ => {}
        }
        Ok(message)
    }
    fn receiver(&mut self) -> Box<dyn crate::channel::WorkerReceiver> {
        panic!("scripted peer uses timed receives")
    }
    fn fixture_inline_recv(&self) -> bool {
        true
    }
    fn kill(&mut self) {
        self.killed = true;
    }
}

fn scratch_path(prefix: &str, name: &str, nanos: u128, sequence: u64) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "basal-{prefix}-{name}-{}-{nanos}-{sequence}",
        std::process::id()
    ))
}

fn create_scratch_dir(
    prefix: &str,
    name: &str,
    nanos: u128,
    counter: &AtomicU64,
) -> std::path::PathBuf {
    loop {
        let sequence = counter.fetch_add(1, Ordering::Relaxed);
        let dir = scratch_path(prefix, name, nanos, sequence);
        match std::fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("create scratch directory {}: {error}", dir.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{create_scratch_dir, scratch_path};
    use std::collections::HashSet;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicU64;
    use std::thread;

    fn now_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after Unix epoch")
            .as_nanos()
    }

    #[test]
    fn concurrent_calls_create_distinct_directories() {
        // Every thread uses the same timestamp, so only the shared sequence
        // counter can keep their directory names apart. The barrier releases
        // all threads together so the calls really overlap.
        const THREADS: usize = 16;
        let nanos = now_nanos();
        let counter = AtomicU64::new(0);
        let start = Barrier::new(THREADS);
        let dirs = thread::scope(|scope| {
            let handles = (0..THREADS)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        create_scratch_dir("test-support", "concurrent", nanos, &counter)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("scratch directory thread"))
                .collect::<Vec<_>>()
        });

        assert!(dirs.iter().all(|dir| dir.is_dir()));
        assert_eq!(dirs.iter().collect::<HashSet<_>>().len(), dirs.len());

        for dir in dirs {
            std::fs::remove_dir_all(dir).expect("remove scratch directory");
        }
    }

    #[test]
    fn existing_candidates_are_skipped() {
        let nanos = now_nanos();
        let counter = AtomicU64::new(0);
        let existing = scratch_path("test-support", "pre-existing", nanos, 0);
        std::fs::create_dir(&existing).expect("create pre-existing candidate");

        let created = create_scratch_dir("test-support", "pre-existing", nanos, &counter);

        assert_ne!(created, existing);
        assert!(existing.is_dir());
        assert!(created.is_dir());
        std::fs::remove_dir_all(existing).expect("remove pre-existing candidate");
        std::fs::remove_dir_all(created).expect("remove created directory");
    }
}
