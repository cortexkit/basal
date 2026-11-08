//! In-memory ownership of live runs: one actor per run, owning its run-log
//! records. Records, terminal projections and worker control are adapters.

mod committer;
pub mod lock_order;
#[cfg(test)]
pub(crate) mod tests;

use crate::{
    CoreError, Result, Store,
    hooks::{Boundary, Hooks, Step},
    run_log::Lsn,
};
use lock_order::{Held, Lock};

const REGISTERING: u8 = 0;
const ABANDONED: u8 = 1;
const HANDED_OFF: u8 = 2;
use rusqlite::{Connection, Transaction};
use std::{
    collections::BTreeMap,
    ops::{Deref, DerefMut},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicU8, Ordering},
    },
};

/// Records and replay are owned by the records adapter, not the registry.
pub trait RunRecords: Send + 'static {
    type Identity;
    type Refusal;
    fn append_identity(&mut self, identity: &Self::Identity) -> Result<Lsn>;
    fn sync_through(&mut self, cut: Lsn) -> Result<()>;
    fn append_refusal(&mut self, refusal: Self::Refusal) -> Result<()>;
    fn all_calls_released(&self) -> bool;
}

/// An owned, retryable terminal projection. It must perform SQLite work only:
/// summary, overlay, folds, obligations, tombstones, detail, latches and outbox.
pub trait TerminalWrite: Send + 'static {
    fn apply(&self, tx: &Transaction<'_>) -> Result<()>;
}

pub trait TerminalBuilder<R>: Send + Sync {
    fn can_close(&self, _: &R) -> bool {
        true
    }
    fn prepare(&self, _: &mut R) -> Result<()> {
        Ok(())
    }
    fn is_success(&self) -> bool;
    fn build(
        &self,
        run_id: &str,
        records: &R,
        reservations: &[Reservation],
    ) -> Result<Box<dyn TerminalWrite>>;
}

/// Driver calls are made only after releasing the actor and accounting locks.
pub trait CloseDriver: Send + Sync {
    /// Signal, then join or kill the activation worker before returning.
    fn stop_worker(&self, run_id: &str) -> Result<()>;
    /// Release the slot, wake the next run, then mark log records reclaimable.
    fn closed(&self, run_id: &str);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Routing {
    Open,
    Closing,
    Closed,
}

/// The accounting adapter chooses the metric and window encoding. This is not
/// a catalogue of operations or log record kinds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Window(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub position: u64,
    pub window: Window,
    pub audit_at: i64,
    pub amount: u64,
}

#[derive(Default)]
struct Accounting {
    runs: BTreeMap<String, Vec<Reservation>>,
}

/// SQLite readers must return the committed count in the same units as amount.
/// The reservation is installed before the accounting lock is released.
pub trait Budget {
    fn committed(&self, conn: &Connection, flow: &str, window: &Window) -> Result<u64>;
}

struct Ordered<T> {
    raw: Mutex<T>,
    rank: Lock,
}
impl<T> Ordered<T> {
    fn new(value: T, rank: Lock) -> Self {
        Self {
            raw: Mutex::new(value),
            rank,
        }
    }
    fn lock(&self) -> Guard<'_, T> {
        let order = lock_order::enter(self.rank);
        Guard {
            raw: Some(self.raw.lock().unwrap_or_else(|p| p.into_inner())),
            order: Some(order),
            rank: self.rank,
        }
    }
}
struct Guard<'a, T> {
    raw: Option<MutexGuard<'a, T>>,
    order: Option<Held>,
    rank: Lock,
}
impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.raw.as_ref().unwrap()
    }
}
impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.raw.as_mut().unwrap()
    }
}
impl<T> Guard<'_, T> {
    fn wait(mut self, condition: &Condvar) -> Self {
        self.order.take();
        self.raw = Some(
            condition
                .wait(self.raw.take().unwrap())
                .unwrap_or_else(|p| p.into_inner()),
        );
        self.order = Some(lock_order::enter(self.rank));
        self
    }
}

struct Actor<R> {
    generation: u64,
    entrants: usize,
    records: R,
    dispatches: std::collections::BTreeSet<u64>,
}
pub struct Run<R> {
    id: String,
    flow: String,
    routing: AtomicU8,
    actor: Ordered<Actor<R>>,
    drained: Condvar,
    closer: Mutex<()>,
    accounting: Arc<Ordered<Accounting>>,
}

impl<R: RunRecords> Run<R> {
    /// Gate checks happen lock-free. Only their winning generation may publish
    /// an activation; a racing claim invalidates the older gate answer.
    pub fn claim<T>(
        self: &Arc<Self>,
        expected_generation: u64,
        f: impl FnOnce(u64, &mut Activation<'_, R>) -> Result<T>,
    ) -> Result<T> {
        let mut actor = self.actor.lock();
        if actor.generation != expected_generation || self.routing() != Routing::Open {
            return Err(self.owner_lost(expected_generation));
        }
        let generation = expected_generation
            .checked_add(1)
            .ok_or_else(|| CoreError::Corrupt("generation exhausted".into()))?;
        actor.generation = generation;
        f(
            generation,
            &mut Activation {
                run: self,
                actor: &mut actor,
                tickets: Vec::new(),
            },
        )
    }
    pub(crate) fn close_busy(&self) -> Result<bool> {
        match self.closer.try_lock() {
            Ok(_) => Ok(false),
            Err(std::sync::TryLockError::WouldBlock) => Ok(true),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                Err(CoreError::Corrupt("run closer lock poisoned".into()))
            }
        }
    }
    pub fn routing(&self) -> Routing {
        match self.routing.load(Ordering::Acquire) {
            0 => Routing::Open,
            1 => Routing::Closing,
            _ => Routing::Closed,
        }
    }
    fn owner_lost(&self, generation: u64) -> CoreError {
        CoreError::OwnerLost {
            run_id: self.id.clone(),
            generation,
        }
    }
    /// Every activation critical section checks both ownership fences before
    /// exposing records. Dispatch entrants are created through this same guard.
    pub fn activate<T>(
        self: &Arc<Self>,
        generation: u64,
        f: impl FnOnce(&mut Activation<'_, R>) -> Result<T>,
    ) -> Result<T> {
        let mut actor = self.actor.lock();
        if actor.generation != generation || self.routing() != Routing::Open {
            return Err(self.owner_lost(generation));
        }
        f(&mut Activation {
            run: self,
            actor: &mut actor,
            tickets: Vec::new(),
        })
    }
    /// Closing callbacks retry; Closed callbacks must use the obligations path.
    /// The routing check and counter increment are atomic with respect to close.
    pub fn enter(self: &Arc<Self>) -> std::result::Result<Entrant<R>, Routing> {
        let routing = self.routing();
        if routing != Routing::Open {
            return Err(routing);
        }
        let mut actor = self.actor.lock();
        let routing = self.routing();
        if routing != Routing::Open {
            return Err(routing);
        }
        actor.entrants += 1;
        Ok(Entrant {
            run: self.clone(),
            ticket: None,
            position: None,
        })
    }
}

impl Run<crate::run_records::Records> {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn retain_detail_for_test(&self, store: Arc<Store>) -> Result<()> {
        self.actor.lock().records.retain_detail(store)
    }
    /// Copy live reader state under the actor, before opening SQLite. Readers
    /// may also observe Closing; they must prefer a committed history row.
    pub fn reader_snapshot(&self) -> (String, String, u64, crate::run_records::State) {
        let actor = self.actor.lock();
        (
            self.id.clone(),
            self.flow.clone(),
            actor.generation,
            actor.records.state().clone(),
        )
    }

    pub(crate) fn closed_hint(&self, commit_seq: u64) {
        let mut actor = self.actor.lock();
        let generation = actor.generation;
        // SQLite already committed; a failed hint cannot undo terminal success.
        if let Err(error) = actor
            .records
            .append(generation, crate::run_records::Body::Closed { commit_seq })
        {
            tracing::warn!(run_id = self.id, %error, "could not append Closed hint");
        }
    }
    /// Maintenance takes the same actor as activations and callbacks. Copying
    /// state and appending its checkpoint are one indivisible critical section.
    pub fn snapshot_if_due(
        &self,
        now_ms: i64,
        active_segments: &[u64],
        hooks: &dyn Hooks,
    ) -> Result<Option<Lsn>> {
        let (log, cut) = {
            let mut actor = self.actor.lock();
            if self.routing() != Routing::Open
                || !actor.records.snapshot_due(now_ms, active_segments)
            {
                return Ok(None);
            }
            let generation = actor.generation;
            actor.records.append_snapshot(generation)?
        };
        lock_order::assert_lock_free();
        if hooks.at(&self.id, &Boundary::MidSnapshot) == Step::Crash {
            self.actor.lock().records.poison();
            return Err(CoreError::Cut);
        }
        // Appends after the checkpoint are safe: recovery applies them after
        // its cutoff. The old start stays pinned until the sync succeeds.
        if let Err(error) = log.sync_through(cut) {
            self.actor.lock().records.poison();
            return Err(error);
        }
        self.actor.lock().records.snapshot_synced(cut);
        Ok(Some(cut))
    }
}

pub struct Entrant<R: RunRecords> {
    run: Arc<Run<R>>,
    ticket: Option<Arc<AtomicU8>>,
    position: Option<u64>,
}
impl<R: RunRecords> Entrant<R> {
    /// Copy a reader's version and data under one actor lock. A claim can
    /// advance the generation even when it writes no execution record.
    pub fn inspect<T>(&self, f: impl FnOnce(u64, &R) -> Result<T>) -> Result<T> {
        let actor = self.run.actor.lock();
        f(actor.generation, &actor.records)
    }
    pub(crate) fn account<T>(
        &self,
        f: impl FnOnce(&mut R) -> Result<(T, Vec<Reservation>)>,
    ) -> Result<T> {
        let mut actor = self.run.actor.lock();
        let mut accounting = self.run.accounting.lock();
        let (result, rows) = f(&mut actor.records)?;
        accounting.runs.insert(self.run.id.clone(), rows);
        Ok(result)
    }
    /// A previously registered entrant can finish while Closing. It does not
    /// acquire new activation ownership or authorize a new dispatch.
    pub fn finish<T>(&self, f: impl FnOnce(&mut R) -> Result<T>) -> Result<T> {
        f(&mut self.run.actor.lock().records)
    }
    /// Append and sync an arrival before making its settlement visible. The
    /// original audit window survives refunds and late usage adjustments.
    pub fn settle<T>(
        &self,
        position: u64,
        window: &Window,
        amount: u64,
        append: impl FnOnce(&mut R) -> Result<T>,
    ) -> Result<T> {
        let mut actor = self.run.actor.lock();
        let mut accounting = self.run.accounting.lock();
        let row = accounting
            .runs
            .get_mut(&self.run.id)
            .and_then(|rows| {
                rows.iter_mut()
                    .find(|r| r.position == position && &r.window == window)
            })
            .ok_or_else(|| CoreError::Invalid("reservation missing".into()))?;
        let result = append(&mut actor.records)?;
        row.amount = amount;
        Ok(result)
    }
}
impl<R: RunRecords> Drop for Entrant<R> {
    fn drop(&mut self) {
        // A dispatch may be abandoned before its activation releases the
        // actor (including error unwinding). Let that activation decrement it
        // instead of trying to recursively lock the actor on the same thread.
        if self.ticket.as_ref().is_some_and(|ticket| {
            ticket
                .compare_exchange(REGISTERING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        }) {
            return;
        }
        let mut actor = self.run.actor.lock();
        if let Some(position) = self.position {
            actor.dispatches.remove(&position);
        }
        actor.entrants -= 1;
        if actor.entrants == 0 {
            self.run.drained.notify_all();
        }
    }
}

pub struct Activation<'a, R> {
    run: &'a Arc<Run<R>>,
    actor: &'a mut Actor<R>,
    tickets: Vec<(Arc<AtomicU8>, Option<u64>)>,
}
impl<R> Drop for Activation<'_, R> {
    fn drop(&mut self) {
        for (ticket, position) in &self.tickets {
            if ticket.swap(HANDED_OFF, Ordering::AcqRel) == ABANDONED {
                self.actor.entrants -= 1;
                if let Some(position) = position {
                    self.actor.dispatches.remove(position);
                }
            }
        }
        if self.actor.entrants == 0 {
            self.run.drained.notify_all();
        }
    }
}
impl<R: RunRecords> Activation<'_, R> {
    /// Rebuild this run's contribution atomically with its journal mutation.
    /// The callback may read SQLite under the accounting lock, but cannot call
    /// a host. Returning an error leaves the accounting contribution unchanged.
    pub(crate) fn account<T>(
        &mut self,
        f: impl FnOnce(&mut R, &[Reservation]) -> Result<(T, Vec<Reservation>)>,
    ) -> Result<T> {
        let mut accounting = self.run.accounting.lock();
        let others: Vec<_> = accounting
            .runs
            .iter()
            .filter(|(id, _)| *id != &self.run.id)
            .flat_map(|(_, rows)| rows.iter().cloned())
            .collect();
        let (result, rows) = f(&mut self.actor.records, &others)?;
        accounting.runs.insert(self.run.id.clone(), rows);
        Ok(result)
    }
    pub fn records(&mut self) -> &mut R {
        &mut self.actor.records
    }
    /// Call in the critical section that appends Intent or authorizes resend,
    /// before handing the entrant to a dispatch thread.
    pub fn dispatch_entrant(&mut self) -> Entrant<R> {
        self.actor.entrants += 1;
        let ticket = Arc::new(AtomicU8::new(REGISTERING));
        self.tickets.push((ticket.clone(), None));
        Entrant {
            run: self.run.clone(),
            ticket: Some(ticket),
            position: None,
        }
    }
    pub(crate) fn is_dispatching(&self, position: u64) -> bool {
        self.actor.dispatches.contains(&position)
    }
    pub(crate) fn dispatch_entrant_for(&mut self, position: u64) -> Entrant<R> {
        self.actor.dispatches.insert(position);
        let mut entrant = self.dispatch_entrant();
        entrant.position = Some(position);
        self.tickets.last_mut().unwrap().1 = Some(position);
        entrant
    }
    pub fn reserve(
        &mut self,
        store: &Store,
        budget: &dyn Budget,
        reservation: Reservation,
        limit: u64,
    ) -> Result<bool> {
        let mut accounting = self.run.accounting.lock();
        // A replay of the same reservation is not a second charge.
        if accounting
            .runs
            .get(&self.run.id)
            .is_some_and(|rows| rows.contains(&reservation))
        {
            return Ok(true);
        }
        let committed =
            store.read(|conn| budget.committed(conn, &self.run.flow, &reservation.window))?;
        let total = accounting
            .runs
            .values()
            .flatten()
            .filter(|r| r.window == reservation.window)
            .try_fold(committed, |n, r| n.checked_add(r.amount))
            .and_then(|n| n.checked_add(reservation.amount));
        if total.is_none_or(|total| total > limit) {
            return Ok(false);
        }
        accounting
            .runs
            .entry(self.run.id.clone())
            .or_default()
            .push(reservation);
        Ok(true)
    }
    /// Sync the call's identity and all preceding records before opening SQLite.
    /// The saturation adapter commits the saturated-window mark, flow disable,
    /// owner-notice outbox row and re-enable decision card together.
    pub fn saturate<T>(
        &mut self,
        store: &Store,
        identity: &R::Identity,
        refusal: R::Refusal,
        transaction: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let _accounting = self.run.accounting.lock();
        let cut = self.actor.records.append_identity(identity)?;
        lock_order::assert_sync_allowed();
        self.actor.records.sync_through(cut)?;
        let result = store.write(transaction)?;
        self.actor.records.append_refusal(refusal)?;
        Ok(result)
    }
    pub fn saturate_dispatch(
        &mut self,
        store: &Store,
        identity: &R::Identity,
        refusal: R::Refusal,
        now: i64,
        limits: &crate::rate::RateLimits,
    ) -> Result<crate::rate::Take> {
        let flow = self.run.flow.clone();
        self.saturate(store, identity, refusal, |tx| {
            crate::rate::saturate_dispatch(tx, &flow, now, limits)
        })
    }
    /// Settlements and proven-unsent refunds retain their original audit window.
    pub fn settle(&mut self, position: u64, window: &Window, amount: u64) -> Result<()> {
        let mut accounting = self.run.accounting.lock();
        let row = accounting
            .runs
            .get_mut(&self.run.id)
            .and_then(|rows| {
                rows.iter_mut()
                    .find(|r| r.position == position && &r.window == window)
            })
            .ok_or_else(|| CoreError::Invalid("reservation missing".into()))?;
        row.amount = amount;
        Ok(())
    }
}

pub struct RunStore<R> {
    retain_detail: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "test-support"))]
    rig_projection: std::sync::atomic::AtomicBool,
    registry: Mutex<BTreeMap<String, Arc<Run<R>>>>,
    flows: Mutex<BTreeMap<String, Arc<Ordered<Accounting>>>>,
    committer: committer::Committer,
    log: Option<Arc<dyn crate::run_admission::AdmissionLog>>,
}

impl RunStore<crate::run_records::Records> {
    /// Ephemeral dry-run inspection keeps closed actor snapshots in memory.
    /// It never enables the test-only SQLite rig projection.
    pub(crate) fn retain_for_scratch(&self) {
        self.retain_detail.store(true, Ordering::Release);
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn retain_detail_for_test(&self, store: Arc<Store>) -> Result<()> {
        crate::rig_projection::create(&store)?;
        self.rig_projection.store(true, Ordering::Release);
        self.retain_detail.store(true, Ordering::Release);
        for run in self.snapshot() {
            run.actor.lock().records.retain_detail(store.clone())?;
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn retains_detail(&self) -> bool {
        self.rig_projection.load(Ordering::Acquire)
    }
    /// Copy replay anchors without holding an actor while the writer enters
    /// SQLite. A concurrent snapshot can only advance an anchor, so the earlier
    /// start copied here remains safe.
    pub fn reclaim_starts(&self) -> BTreeMap<String, Lsn> {
        self.snapshot()
            .into_iter()
            .filter_map(|run| {
                let actor = run.actor.lock();
                actor
                    .records
                    .required_start()
                    .map(|lsn| (run.id.clone(), lsn))
            })
            .collect()
    }

    /// Registry maintenance never holds the registry mutex while taking actors
    /// or waiting for the writer's group sync.
    pub fn snapshot_due_runs(
        &self,
        now_ms: i64,
        active_segments: &[u64],
        hooks: &dyn Hooks,
    ) -> Result<Vec<Lsn>> {
        self.snapshot()
            .into_iter()
            .filter_map(
                |run| match run.snapshot_if_due(now_ms, active_segments, hooks) {
                    Ok(Some(lsn)) => Some(Ok(lsn)),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                },
            )
            .collect()
    }
}

impl<R: RunRecords> RunStore<R> {
    /// Creates the run registry with log watermark maintenance for terminal
    /// commits and late-completion settlement transactions.
    pub fn new_bound(
        store: Arc<Store>,
        hooks: Arc<dyn Hooks>,
        log: Arc<dyn crate::run_admission::AdmissionLog>,
    ) -> Self {
        Self {
            retain_detail: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "test-support"))]
            rig_projection: std::sync::atomic::AtomicBool::new(false),
            registry: Mutex::new(BTreeMap::new()),
            flows: Mutex::new(BTreeMap::new()),
            committer: committer::Committer::with_log(store, hooks, Some(log.clone())),
            log: Some(log),
        }
    }

    pub fn new_staged(store: Arc<Store>, hooks: Arc<dyn Hooks>) -> Self {
        Self {
            retain_detail: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "test-support"))]
            rig_projection: std::sync::atomic::AtomicBool::new(false),
            registry: Mutex::new(BTreeMap::new()),
            flows: Mutex::new(BTreeMap::new()),
            committer: committer::Committer::new(store, hooks),
            log: None,
        }
    }
    /// Admission/recovery publishes a complete run before any host can attach.
    pub fn insert(
        &self,
        id: String,
        flow: String,
        generation: u64,
        records: R,
    ) -> Result<Arc<Run<R>>> {
        self.insert_accounted(id, flow, generation, records, Vec::new())
    }
    pub(crate) fn insert_accounted(
        &self,
        id: String,
        flow: String,
        generation: u64,
        records: R,
        reservations: Vec<Reservation>,
    ) -> Result<Arc<Run<R>>> {
        let accounting = self
            .flows
            .lock()
            .unwrap()
            .entry(flow.clone())
            .or_insert_with(|| Arc::new(Ordered::new(Accounting::default(), Lock::Accounting)))
            .clone();
        let mut contribution = accounting.lock();
        let mut registry = self.registry.lock().unwrap();
        if registry.contains_key(&id) {
            return Err(CoreError::Invalid("run already registered".into()));
        }
        let run = Arc::new(Run {
            id: id.clone(),
            flow,
            routing: AtomicU8::new(Routing::Open as u8),
            actor: Ordered::new(
                Actor {
                    generation,
                    entrants: 0,
                    records,
                    dispatches: std::collections::BTreeSet::new(),
                },
                Lock::Actor,
            ),
            drained: Condvar::new(),
            closer: Mutex::new(()),
            accounting: accounting.clone(),
        });
        contribution.runs.insert(id.clone(), reservations);
        registry.insert(id, run.clone());
        Ok(run)
    }
    pub fn get(&self, id: &str) -> Option<Arc<Run<R>>> {
        self.registry.lock().unwrap().get(id).cloned()
    }
    /// No registry mutex is held while taking an actor or SQLite connection.
    pub fn snapshot(&self) -> Vec<Arc<Run<R>>> {
        self.registry.lock().unwrap().values().cloned().collect()
    }
    pub fn audit_at(&self, flow: &str) -> Vec<i64> {
        let accounting = self.flows.lock().unwrap().get(flow).cloned();
        accounting
            .map(|a| {
                a.lock()
                    .runs
                    .values()
                    .flatten()
                    .map(|r| r.audit_at)
                    .collect()
            })
            .unwrap_or_default()
    }
    /// Closed-obligation settlement uses the flow lock, never an actor.
    pub fn accounting_write<T>(
        &self,
        flow: &str,
        store: &Store,
        f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let accounting = self
            .flows
            .lock()
            .unwrap()
            .entry(flow.to_owned())
            .or_insert_with(|| Arc::new(Ordered::new(Accounting::default(), Lock::Accounting)))
            .clone();
        let _accounting = accounting.lock();
        match &self.log {
            Some(log) => crate::run_store_sql::write_synced(store, log.as_ref(), f),
            None => store.write(f),
        }
    }
    pub(crate) fn accounting_read<T>(
        &self,
        flow: &str,
        store: &Store,
        f: impl FnOnce(&rusqlite::Connection, &BTreeMap<String, Vec<Reservation>>) -> Result<T>,
    ) -> Result<T> {
        let accounting = self
            .flows
            .lock()
            .unwrap()
            .entry(flow.into())
            .or_insert_with(|| Arc::new(Ordered::new(Accounting::default(), Lock::Accounting)))
            .clone();
        let accounting = accounting.lock();
        store.read(|conn| f(conn, &accounting.runs))
    }
    /// Hold the requested flows in sorted order through one SQLite snapshot.
    /// The locks form one accounting rank: callers must not acquire any other
    /// accounting lock until the entire batch has been released.
    pub(crate) fn accounting_read_many<T>(
        &self,
        flow_ids: &[&str],
        store: &Store,
        f: impl FnOnce(
            &rusqlite::Connection,
            &BTreeMap<String, &BTreeMap<String, Vec<Reservation>>>,
        ) -> Result<T>,
    ) -> Result<T> {
        let accounting: Vec<_> = {
            let mut flows = self.flows.lock().unwrap();
            flow_ids
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|flow| {
                    let lock = flows
                        .entry(flow.into())
                        .or_insert_with(|| {
                            Arc::new(Ordered::new(Accounting::default(), Lock::Accounting))
                        })
                        .clone();
                    (flow.to_owned(), lock)
                })
                .collect()
        };
        let _order = lock_order::enter(Lock::Accounting);
        let guards: Vec<_> = accounting
            .iter()
            .map(|(_, lock)| lock.raw.lock().unwrap_or_else(|p| p.into_inner()))
            .collect();
        let runs = accounting
            .iter()
            .zip(&guards)
            .map(|((flow, _), guard)| (flow.clone(), &guard.runs))
            .collect();
        store.read(|conn| f(conn, &runs))
    }
    pub fn close(
        &self,
        run: &Arc<Run<R>>,
        builder: &dyn TerminalBuilder<R>,
        driver: &dyn CloseDriver,
    ) -> Result<()> {
        self.close_impl(run, builder, driver, None)
    }
    pub(crate) fn close_owned(
        &self,
        run: &Arc<Run<R>>,
        generation: u64,
        builder: &dyn TerminalBuilder<R>,
        driver: &dyn CloseDriver,
    ) -> Result<()> {
        self.close_impl(run, builder, driver, Some(generation))
    }
    fn close_impl(
        &self,
        run: &Arc<Run<R>>,
        builder: &dyn TerminalBuilder<R>,
        driver: &dyn CloseDriver,
        generation: Option<u64>,
    ) -> Result<()> {
        // Another closer (a deadline, cancel or removal) supersedes the run's
        // owner: an owned close that loses the race has lost the run, which is
        // what the owner must report. A caller closing without ownership may
        // retry.
        let _closer = run.closer.try_lock().map_err(|_| match generation {
            Some(generation) => run.owner_lost(generation),
            None => CoreError::Unavailable("another closer is active".into()),
        })?;
        {
            let mut actor = run.actor.lock();
            if let Some(generation) = generation {
                let expected = if run.routing() == Routing::Closing {
                    generation.checked_add(1)
                } else {
                    Some(generation)
                };
                if expected != Some(actor.generation) {
                    return Err(run.owner_lost(generation));
                }
            }
            if run.routing() == Routing::Closed {
                return Ok(());
            }
            if run.routing() == Routing::Open {
                if !builder.can_close(&actor.records) {
                    return Err(CoreError::Unavailable("run changed before close".into()));
                }
                actor.generation = actor
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| CoreError::Corrupt("generation exhausted".into()))?;
                run.routing.store(Routing::Closing as u8, Ordering::Release);
            }
        }
        lock_order::assert_lock_free();
        driver.stop_worker(&run.id)?;
        let mut actor = run.actor.lock();
        while actor.entrants != 0 {
            actor = actor.wait(&run.drained);
        }
        let mut accounting = run.accounting.lock();
        if builder.is_success() && !actor.records.all_calls_released() {
            return Err(CoreError::Invalid(
                "success requires every call released".into(),
            ));
        }
        let reservations = accounting
            .runs
            .get(&run.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        builder.prepare(&mut actor.records)?;
        let write = builder.build(&run.id, &actor.records, reservations)?;
        self.committer.commit(run.id.clone(), write)?;
        accounting.runs.remove(&run.id);
        run.routing.store(Routing::Closed as u8, Ordering::Release);
        if !self.retain_detail.load(Ordering::Acquire) {
            self.registry.lock().unwrap().remove(&run.id);
        }
        drop(accounting);
        drop(actor);
        lock_order::assert_lock_free();
        driver.closed(&run.id);
        Ok(())
    }
}
