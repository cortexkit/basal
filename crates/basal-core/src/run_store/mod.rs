//! Staged in-memory run ownership. Production construction stays on the old driver
//! until cutover; records, terminal projections and worker control are adapters.

mod committer;
pub mod lock_order;
#[cfg(test)]
mod tests;

use crate::{CoreError, Result, Store, hooks::Hooks, run_log::Lsn};
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
        })
    }
}

pub struct Entrant<R: RunRecords> {
    run: Arc<Run<R>>,
    ticket: Option<Arc<AtomicU8>>,
}
impl<R: RunRecords> Entrant<R> {
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
        actor.entrants -= 1;
        if actor.entrants == 0 {
            self.run.drained.notify_all();
        }
    }
}

pub struct Activation<'a, R> {
    run: &'a Arc<Run<R>>,
    actor: &'a mut Actor<R>,
    tickets: Vec<Arc<AtomicU8>>,
}
impl<R> Drop for Activation<'_, R> {
    fn drop(&mut self) {
        for ticket in &self.tickets {
            if ticket.swap(HANDED_OFF, Ordering::AcqRel) == ABANDONED {
                self.actor.entrants -= 1;
            }
        }
        if self.actor.entrants == 0 {
            self.run.drained.notify_all();
        }
    }
}
impl<R: RunRecords> Activation<'_, R> {
    pub fn records(&mut self) -> &mut R {
        &mut self.actor.records
    }
    /// Call in the critical section that appends Intent or authorizes resend,
    /// before handing the entrant to a dispatch thread.
    pub fn dispatch_entrant(&mut self) -> Entrant<R> {
        self.actor.entrants += 1;
        let ticket = Arc::new(AtomicU8::new(REGISTERING));
        self.tickets.push(ticket.clone());
        Entrant {
            run: self.run.clone(),
            ticket: Some(ticket),
        }
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
    registry: Mutex<BTreeMap<String, Arc<Run<R>>>>,
    flows: Mutex<BTreeMap<String, Arc<Ordered<Accounting>>>>,
    committer: committer::Committer,
}

impl<R: RunRecords> RunStore<R> {
    pub fn new_staged(store: Arc<Store>, hooks: Arc<dyn Hooks>) -> Self {
        Self {
            registry: Mutex::new(BTreeMap::new()),
            flows: Mutex::new(BTreeMap::new()),
            committer: committer::Committer::new(store, hooks),
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
        let mut registry = self.registry.lock().unwrap();
        if registry.contains_key(&id) {
            return Err(CoreError::Invalid("run already registered".into()));
        }
        let accounting = self
            .flows
            .lock()
            .unwrap()
            .entry(flow.clone())
            .or_insert_with(|| Arc::new(Ordered::new(Accounting::default(), Lock::Accounting)))
            .clone();
        let run = Arc::new(Run {
            id: id.clone(),
            flow,
            routing: AtomicU8::new(Routing::Open as u8),
            actor: Ordered::new(
                Actor {
                    generation,
                    entrants: 0,
                    records,
                },
                Lock::Actor,
            ),
            drained: Condvar::new(),
            closer: Mutex::new(()),
            accounting,
        });
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
        store.write(f)
    }
    pub fn close(
        &self,
        run: &Arc<Run<R>>,
        builder: &dyn TerminalBuilder<R>,
        driver: &dyn CloseDriver,
    ) -> Result<()> {
        let _closer = run
            .closer
            .try_lock()
            .map_err(|_| CoreError::Unavailable("another closer is active".into()))?;
        {
            let mut actor = run.actor.lock();
            if run.routing() == Routing::Closed {
                return Ok(());
            }
            if run.routing() == Routing::Open {
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
        let write = builder.build(&run.id, &actor.records, reservations)?;
        self.committer.commit(run.id.clone(), write)?;
        accounting.runs.remove(&run.id);
        run.routing.store(Routing::Closed as u8, Ordering::Release);
        self.registry.lock().unwrap().remove(&run.id);
        drop(accounting);
        drop(actor);
        lock_order::assert_lock_free();
        driver.closed(&run.id);
        Ok(())
    }
}
