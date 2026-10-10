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
            state = self
                .changed
                .wait_while(state, |state| {
                    state.held.as_deref() == Some(&request.run_id)
                })
                .unwrap();
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
        self.state.lock().unwrap().held = None;
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
