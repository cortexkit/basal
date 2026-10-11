//! An idempotent mock of core's run-scope management operations. Tests still
//! bind and call the real basal module; this double supplies core authority.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};

use basal_host::run_scope::{CatalogEntry, Close, Closed, Open, Opened, Scope};
use basal_host::transport::WireError;

#[derive(Default)]
struct State {
    opens: BTreeMap<String, (Open, Opened)>,
    closed: Vec<Close>,
    hold_next: bool,
    held: Option<String>,
}

#[derive(Default)]
pub struct RunScopes {
    state: Mutex<State>,
    changed: Condvar,
}

impl RunScopes {
    pub fn hold_next_open(&self) {
        self.state.lock().unwrap().hold_next = true;
    }

    pub fn open(&self, request: Open, catalog: Vec<CatalogEntry>) -> Result<Opened, WireError> {
        let mut state = self.state.lock().unwrap();
        if let Some((old, opened)) = state.opens.get(&request.run_id) {
            if old != &request {
                return Err(WireError::Refused {
                    code: "run_id_conflict".into(),
                    message: "run belongs to another invocation".into(),
                });
            }
            return Ok(opened.clone());
        }
        let opened = Opened {
            scope: Scope {
                reference: request.run_id.clone(),
                epoch: 19,
            },
            expires_at_ms: request.expires_at_ms,
            catalog,
        };
        state
            .opens
            .insert(request.run_id.clone(), (request.clone(), opened.clone()));
        if state.hold_next {
            state.hold_next = false;
            state.held = Some(request.run_id.clone());
            self.changed.notify_all();
            let (released, timeout) = self
                .changed
                .wait_timeout_while(state, std::time::Duration::from_secs(60), |state| {
                    state.held.as_deref() == Some(&request.run_id)
                })
                .unwrap();
            // A handler running this synchronous mock on a Tokio worker can
            // starve the withdrawal that releases it. Fail the artificial hold
            // instead of leaving the test runtime blocked forever.
            drop(released);
            assert!(!timeout.timed_out(), "held core open was never released");
            return Ok(opened);
        }
        drop(state);
        Ok(opened)
    }

    pub fn close(&self, request: Close) -> Closed {
        let mut state = self.state.lock().unwrap();
        let already_closed = state.closed.contains(&request);
        if !already_closed {
            state.closed.push(request.clone());
        }
        if state.held.as_deref() == Some(&request.run_id) {
            state.held = None;
            self.changed.notify_all();
        }
        Closed {
            closed: !already_closed,
            already_closed,
        }
    }

    pub fn release(&self) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poison) if std::thread::panicking() => {
                // A failed await_held poisons the lock. Release still has to
                // unblock providers when a fixture drops during that failure.
                eprintln!("run-scope fixture: releasing poisoned state during unwinding");
                poison.into_inner()
            }
            Err(poison) => panic!("run-scope fixture state poisoned: {poison}"),
        };
        state.held = None;
        self.changed.notify_all();
    }

    pub fn await_held(&self) {
        let (state, timeout) = self
            .changed
            .wait_timeout_while(
                self.state.lock().unwrap(),
                std::time::Duration::from_secs(30),
                |state| state.held.is_none(),
            )
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "core never held an open: {} scopes",
            state.opens.len()
        );
    }
}
