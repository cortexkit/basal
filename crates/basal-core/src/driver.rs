//! One log-backed activation of one run on one worker.
//!
//! Causal recoverability is the crash-safety guarantee: anything leaving basal
//! is backed by a durable run-log prefix or a committed SQLite transaction that
//! contains every value it depended on. Remote intent is synced before dispatch.
//! Outcomes, delivery order and synchronous replies may be lazy until an external
//! effect requires their prefix. Recovery replays only the durable prefix; clock,
//! random and kv values beyond it may differ on re-execution. A superseded
//! activation cannot authorize a dispatch or mutate the run actor.

use crate::{
    CoreError, Result,
    channel::WorkerChannel,
    runs::Lease,
    runtime::{ActivationEnd, Runtime},
};

impl Runtime {
    /// Drives one activation on the supplied worker after the install gate.
    pub fn activate(&self, run_id: &str, worker: &mut dyn WorkerChannel) -> Result<ActivationEnd> {
        match self.claim(run_id)? {
            Ok(lease) => Ok(self.drive(lease, worker)),
            Err(end) => Ok(end),
        }
    }
    /// Starts an activation using the configured worker source.
    pub fn resume(&self, run_id: &str) -> Result<ActivationEnd> {
        let source = self
            .shared
            .source
            .clone()
            .ok_or_else(|| CoreError::Invalid("no worker source configured".into()))?;
        let lease = match self.claim(run_id)? {
            Ok(lease) => lease,
            Err(end) => return Ok(end),
        };
        let mut worker = match source.worker() {
            Ok(worker) => worker,
            Err(error) => {
                let live = self
                    .store()
                    .read(|c| crate::run_store_sql::live(c, run_id))?
                    .ok_or_else(|| CoreError::NoSuchRun(run_id.into()))?;
                self.staged_driver(run_id)?
                    .owned(live, lease.generation)?
                    .requeue(false)?;
                self.shared.staged_owners.lock().unwrap().remove(run_id);
                return Err(CoreError::Worker(error.to_string()));
            }
        };
        Ok(self.drive(lease, worker.as_mut()))
    }
    fn claim(&self, run_id: &str) -> Result<std::result::Result<Lease, ActivationEnd>> {
        self.staged_claim(run_id)
    }
    pub fn drive(&self, lease: Lease, worker: &mut dyn WorkerChannel) -> ActivationEnd {
        self.staged_drive(lease, worker)
    }
}
