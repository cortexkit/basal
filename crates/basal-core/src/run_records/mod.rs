//! The staged run-log journal. Production construction remains on the SQLite path.
//! A RunStore actor owns each `Records`; only the log writer is shared across runs.

mod codec;
mod state;
#[cfg(test)]
pub(crate) mod tests;
mod types;

use std::sync::Arc;

pub use state::{CallState, Claim, State};
pub use types::*;

use crate::{
    CoreError, Result,
    run_log::{self, LogRecord, Lsn, RecoveredRecord, RunLog},
    run_store::RunRecords,
};

impl LogRecord for Record {
    fn run_id(&self) -> Option<&str> {
        Some(&self.run_id)
    }
    fn record_type(&self) -> u8 {
        match self.body {
            Body::Claimed { .. } => 3,
            Body::Activated => 4,
            Body::Requeued { .. } => 5,
            Body::Fingerprint { .. } => 6,
            Body::ScriptStarted => 7,
            Body::Intent(_) => 8,
            Body::Issued(_) => 9,
            Body::Accepted { .. } => 10,
            Body::Arrived(_) => 11,
            Body::Released { .. } => 12,
            Body::Sync(_) => 13,
            Body::Deferred(_) => 14,
            Body::Unknown { .. } => 15,
            Body::ScopeCut { .. } => 16,
            Body::EventBody { .. } => 17,
            Body::Parked { .. } => 18,
            Body::Woken => 19,
            Body::Resolution(_) => 20,
            Body::Snapshot { .. } => 21,
            Body::Closed { .. } => 22,
        }
    }
    fn decode(record_type: u8, payload: &[u8]) -> run_log::Result<Self> {
        let (record, remaining): (Self, _) = postcard::take_from_bytes(payload)
            .map_err(|_| run_log::Error::Corrupt("invalid run record"))?;
        if record.record_type() != record_type || !remaining.is_empty() {
            return Err(run_log::Error::Corrupt(
                "run record type mismatch or trailing bytes",
            ));
        }
        Ok(record)
    }
}

/// The writer interface also permits deterministic durability tests without SQLite.
/// Implementations must not publish success until append/sync has completed.
pub trait RecordLog: Send + std::marker::Sync + 'static {
    fn append(&self, record: Record) -> Result<Lsn>;
    fn sync_through(&self, lsn: Lsn) -> Result<()>;
    /// Includes group syncs requested by other runs sharing this log.
    fn synced_lsn(&self) -> Lsn {
        Lsn::default()
    }
}

fn log_error(error: run_log::Error) -> CoreError {
    match error {
        run_log::Error::Cut => CoreError::Cut,
        other => CoreError::Store(format!("run log: {other}")),
    }
}

impl RecordLog for RunLog<Record> {
    fn synced_lsn(&self) -> Lsn {
        RunLog::synced_lsn(self)
    }
    fn append(&self, record: Record) -> Result<Lsn> {
        RunLog::append(self, vec![record]).map_err(log_error)
    }
    fn sync_through(&self, lsn: Lsn) -> Result<()> {
        RunLog::sync_through(self, lsn)
            .map(|_| ())
            .map_err(log_error)
    }
}

pub struct Records {
    #[cfg(any(test, feature = "test-support"))]
    detail_store: Option<Arc<crate::Store>>,
    run_id: String,
    log: Arc<dyn RecordLog>,
    state: State,
    last_lsn: Lsn,
    required_start: Option<Lsn>,
    poisoned: bool,
    identity: Option<crate::run_store_sql::RunIdentity>,
    intent_cuts: std::collections::BTreeMap<u64, Lsn>,
    confirmed_sync: Lsn,
}

impl Records {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
    pub fn revision(&self) -> Lsn {
        self.last_lsn
    }
    pub fn new(run_id: String, admitted_ms: i64, log: Arc<dyn RecordLog>) -> Self {
        Self {
            #[cfg(any(test, feature = "test-support"))]
            detail_store: None,
            run_id,
            log,
            state: State::new(admitted_ms),
            last_lsn: Lsn::default(),
            required_start: None,
            poisoned: false,
            identity: None,
            intent_cuts: Default::default(),
            confirmed_sync: Lsn::default(),
        }
    }
    pub fn state(&self) -> &State {
        &self.state
    }
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn retain_detail(&mut self, store: Arc<crate::Store>) -> Result<()> {
        crate::rig_projection::write(&store, &self.run_id, &self.state)?;
        self.detail_store = Some(store);
        Ok(())
    }
    pub(crate) fn bind_identity(&mut self, identity: crate::run_store_sql::RunIdentity) {
        self.identity = Some(identity);
    }
    pub(crate) fn identity(&self) -> Option<&crate::run_store_sql::RunIdentity> {
        self.identity.as_ref()
    }
    pub(crate) fn check_healthy(&self) -> Result<()> {
        if self.poisoned {
            Err(CoreError::Cut)
        } else {
            Ok(())
        }
    }
    pub(crate) fn durable_intent(&self, position: u64) -> bool {
        self.intent_cuts
            .get(&position)
            .is_some_and(|cut| *cut <= self.confirmed_sync.max(self.log.synced_lsn()))
    }
    /// Reclamation must retain this start point until a newer snapshot is synced.
    pub fn required_start(&self) -> Option<Lsn> {
        self.required_start
    }
    pub fn last_lsn(&self) -> Lsn {
        self.last_lsn
    }

    pub(crate) fn preview(&self, generation: u64, body: &Body) -> Result<State> {
        let mut next = self.state.clone();
        next.apply(generation, body)?;
        Ok(next)
    }

    /// The activation releases only awaited arrivals, in mailbox arrival order.
    /// The returned delivery is safe to send after the actor has been released.
    pub fn release_next(
        &mut self,
        generation: u64,
        awaited: &[u64],
    ) -> Result<Option<basal_proto::Outcome>> {
        let next = self
            .state
            .calls
            .values()
            .filter(|call| {
                call.delivery_order.is_none() && awaited.contains(&call.identity.position)
            })
            .filter_map(|call| call.arrival.as_ref())
            .min_by_key(|arrival| arrival.arrival_seq)
            .cloned();
        let Some(arrival) = next else {
            return Ok(None);
        };
        let delivery_order = self.state.next_delivery_order;
        self.append(
            generation,
            Body::Released {
                position: arrival.position,
                delivery_order,
            },
        )?;
        Ok(Some(basal_proto::Outcome {
            position: arrival.position,
            settlement: arrival.settlement,
            value: arrival.value,
            delivery_order,
        }))
    }

    /// Clock and random replies draw from exactly the same counter as releases.
    pub fn reply_sync(
        &mut self,
        generation: u64,
        call: basal_proto::HostCall,
        value: basal_proto::JsonText,
        clock_ms: Option<i64>,
    ) -> Result<basal_proto::Outcome> {
        let delivery_order = self.state.next_delivery_order;
        let position = call.position;
        self.append(
            generation,
            Body::Sync(Sync {
                call: CallIdentity::new(call, crate::model::StoredClass::Sync),
                delivery_order,
                value: value.clone(),
                clock_ms,
            }),
        )?;
        Ok(basal_proto::Outcome {
            position,
            settlement: basal_proto::Settlement::Fulfilled,
            value,
            delivery_order,
        })
    }

    pub fn append(&mut self, generation: u64, body: Body) -> Result<Lsn> {
        let (lsn, sync) = self.append_pending(generation, body)?;
        if sync {
            self.sync_through(lsn)?;
        }
        Ok(lsn)
    }

    /// The driver registers its dispatch entrant before releasing the actor,
    /// then syncs this cut before handing anything to the host. Separating the
    /// append from the wait also permits a lock-free crash hook between them.
    pub(crate) fn append_pending(&mut self, generation: u64, body: Body) -> Result<(Lsn, bool)> {
        if self.poisoned {
            return Err(CoreError::Cut);
        }
        let sync = self.state.requires_sync(&body)?;
        // Validate before writing; failed appends must not change the overlay.
        let mut next = self.state.clone();
        next.apply(generation, &body)?;
        let intent_position = match &body {
            Body::Intent(intent) => Some(intent.call.position),
            _ => None,
        };
        let lsn = self.log.append(Record {
            run_id: self.run_id.clone(),
            generation,
            body,
        })?;
        self.state = next;
        if let Some(position) = intent_position {
            self.intent_cuts.entry(position).or_insert(lsn);
        }
        self.last_lsn = lsn;
        self.required_start.get_or_insert(lsn);
        #[cfg(any(test, feature = "test-support"))]
        if let Some(store) = &self.detail_store {
            crate::rig_projection::write(store, &self.run_id, &self.state)?;
        }
        Ok((lsn, sync))
    }

    /// A terminal transaction exports the entire state, including lazy values.
    /// The closer holds the actor and accounting lock, but not SQLite.
    pub(crate) fn sync_terminal(&self) -> Result<()> {
        if self.poisoned {
            return Err(CoreError::Cut);
        }
        self.log.sync_through(self.last_lsn)
    }

    /// `active_segments` is the retained sequence list, oldest first, without the spare.
    pub fn snapshot_due(&self, now_ms: i64, active_segments: &[u64]) -> bool {
        self.required_start.is_some_and(|start| {
            active_segments
                .get(2)
                .is_some_and(|third| start.sequence <= *third)
                || now_ms.saturating_sub(self.state.admitted_ms) > 10 * 60 * 1000
        })
    }

    // The caller holds the run actor across both copying and appending. Sync is
    // deliberately separate so the crash hook can run without an actor lock.
    pub(crate) fn append_snapshot(&mut self, generation: u64) -> Result<(Arc<dyn RecordLog>, Lsn)> {
        crate::run_store::lock_order::assert_actor_held();
        if self.poisoned {
            return Err(CoreError::Cut);
        }
        let body = Body::Snapshot {
            cutoff_lsn: self.last_lsn,
            next_delivery_order: self.state.next_delivery_order,
            next_arrival_seq: self.state.next_arrival_seq,
            state: Box::new(self.state.clone()),
        };
        let lsn = self.log.append(Record {
            run_id: self.run_id.clone(),
            generation,
            body,
        })?;
        crate::run_store::lock_order::assert_actor_held();
        self.last_lsn = lsn;
        Ok((self.log.clone(), lsn))
    }
    pub(crate) fn snapshot_synced(&mut self, lsn: Lsn) {
        self.required_start = Some(self.required_start.map_or(lsn, |old| old.max(lsn)));
    }
    pub(crate) fn poison(&mut self) {
        self.poisoned = true;
    }

    /// Start at the latest checkpoint, including when all pre-checkpoint segments
    /// have been reclaimed. Never reapply the records already folded into it.
    pub fn recover(
        run_id: String,
        admitted_ms: i64,
        log: Arc<dyn RecordLog>,
        records: &[RecoveredRecord<Record>],
    ) -> Result<Self> {
        let relevant: Vec<_> = records
            .iter()
            .filter(|r| r.record.run_id == run_id)
            .collect();
        if relevant.windows(2).any(|pair| pair[0].lsn >= pair[1].lsn) {
            return Err(CoreError::Corrupt(
                "run records are not in LSN order".into(),
            ));
        }
        let mut result = Self::new(run_id, admitted_ms, log);
        let snapshot = relevant
            .iter()
            .rev()
            .find(|r| matches!(r.record.body, Body::Snapshot { .. }));
        let cutoff = if let Some(snapshot) = snapshot {
            let Body::Snapshot {
                cutoff_lsn,
                next_delivery_order,
                next_arrival_seq,
                state,
            } = &snapshot.record.body
            else {
                unreachable!()
            };
            if *cutoff_lsn >= snapshot.lsn
                || *next_delivery_order != state.next_delivery_order
                || *next_arrival_seq != state.next_arrival_seq
                || state.admitted_ms != admitted_ms
            {
                return Err(CoreError::Corrupt(
                    "invalid snapshot cutoff or counters".into(),
                ));
            }
            result.state = *state.clone();
            result.required_start = Some(snapshot.lsn);
            result.last_lsn = snapshot.lsn;
            Some(*cutoff_lsn)
        } else {
            None
        };
        for record in relevant {
            if cutoff.is_some_and(|cutoff| record.lsn <= cutoff) {
                continue;
            }
            if matches!(record.record.body, Body::Snapshot { .. }) {
                continue;
            }
            result
                .state
                .apply(record.record.generation, &record.record.body)?;
            result.last_lsn = result.last_lsn.max(record.lsn);
            result.required_start.get_or_insert(record.lsn);
        }
        // Log recovery validates frames and syncs the retained prefix before
        // returning it. Snapshot identities remain durable even when their
        // original segments have been reclaimed.
        result.intent_cuts = result
            .state
            .calls
            .iter()
            .filter(|(_, call)| call.intent.is_some())
            .map(|(position, _)| (*position, Lsn::default()))
            .collect();
        result.confirmed_sync = result.last_lsn;
        Ok(result)
    }
}

impl RunRecords for Records {
    type Identity = Record;
    type Refusal = Record;
    fn append_identity(&mut self, identity: &Record) -> Result<Lsn> {
        if identity.run_id != self.run_id
            || !matches!(
                identity.body,
                Body::Intent(_) | Body::Issued(_) | Body::Sync(_)
            )
        {
            return Err(CoreError::Invalid(
                "expected this run's call identity".into(),
            ));
        }
        self.append(identity.generation, identity.body.clone())
    }
    fn sync_through(&mut self, cut: Lsn) -> Result<()> {
        if self.poisoned {
            return Err(CoreError::Cut);
        }
        if let Err(error) = self.log.sync_through(cut) {
            self.poisoned = true;
            return Err(error);
        }
        self.confirmed_sync = self.confirmed_sync.max(cut);
        Ok(())
    }
    fn append_refusal(&mut self, mut refusal: Record) -> Result<()> {
        if refusal.run_id != self.run_id || !matches!(refusal.body, Body::Arrived(_)) {
            return Err(CoreError::Invalid(
                "expected this run's refusal arrival".into(),
            ));
        }
        if let Body::Arrived(arrival) = &mut refusal.body {
            arrival.durability.refusal_committed = true;
        }
        self.append(refusal.generation, refusal.body).map(|_| ())
    }
    fn all_calls_released(&self) -> bool {
        self.state
            .calls
            .values()
            .all(|call| call.sync.is_some() || call.delivery_order.is_some())
    }
}
