//! Debug checks for the new run path. Connection checks also cover legacy callers.
//!
//! The only allowed order is Broca per-send gate, then run actor, then flow
//! accounting lock, then the SQLite connection (reads included, since the store
//! has one connection). No thread waits for a log sync while holding SQLite,
//! and no host or sink call is made while holding any of these locks.

use std::cell::RefCell;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lock {
    SendGate,
    Actor,
    Accounting,
    Sqlite,
}

thread_local! {
    static HELD: RefCell<Vec<Lock>> = const { RefCell::new(Vec::new()) };
}

/// An ordering token, not a mutex. Keep it for exactly the lock's lifetime.
pub struct Held {
    lock: Lock,
    // Lock ownership must not move between threads.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

pub fn enter(lock: Lock) -> Held {
    if cfg!(debug_assertions) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            assert!(
                held.last().is_none_or(|last| *last < lock),
                "lock order: {held:?} -> {lock:?}"
            );
            held.push(lock);
        });
    }
    Held {
        lock,
        _thread: std::marker::PhantomData,
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if cfg!(debug_assertions) {
            HELD.with(|held| assert_eq!(held.borrow_mut().pop(), Some(self.lock)));
        }
    }
}

pub fn assert_sync_allowed() {
    if cfg!(debug_assertions) {
        HELD.with(|held| {
            assert!(
                !held.borrow().contains(&Lock::Sqlite),
                "log sync under SQLite"
            )
        });
    }
}

pub(crate) fn assert_actor_held() {
    if cfg!(debug_assertions) {
        HELD.with(|held| {
            assert!(
                held.borrow().contains(&Lock::Actor),
                "snapshot outside actor"
            )
        });
    }
}

pub fn assert_lock_free() {
    if cfg!(debug_assertions) {
        HELD.with(|held| {
            assert!(
                held.borrow().iter().all(|lock| *lock == Lock::SendGate),
                "external call under a run/store lock"
            )
        });
    }
}
