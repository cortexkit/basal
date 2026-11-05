//! Activation driver for the staged RunStore path. No production constructor
//! currently selects it. Unlike the SQLite driver, worker deliveries may
//! be lazy: every value exported to a host or terminal transaction is protected
//! by a synced causal prefix, not necessarily each worker reply.
//!
//! Lock order at driver entry points. Host methods and ordinary crash hooks run
//! after these locks are released. The at_run observer receives the records under
//! the actor mutex and must not reacquire an actor or call a host.
//!
//! | Entry point | Locks, in order | Log waits |
//! | --- | --- | --- |
//! | register | accounting, registry | none |
//! | claim | actor, accounting | none |
//! | issue / authorize | actor, accounting, SQLite read | after SQLite closes |
//! | local / sync / release / park / requeue | actor | required records only |
//! | dispatch return / wake | actor, accounting where needed | required records only |
//! | terminal build | actor, accounting | before committer opens SQLite |
//! | terminal commit | SQLite, on committer thread | none |

pub(crate) mod accounting;
mod local;
mod terminal;
#[cfg(test)]
mod tests;
mod worker;

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use basal_proto::{HostCall, JsonText, Outcome, Settlement};

use crate::{
    CoreError, Result, Store,
    hooks::{Boundary, Hooks, Step},
    model::StoredClass,
    run_records::{
        Arrival, ArrivalDurability, Body, CallIdentity, Intent, KvEffect, Records, State,
    },
    run_store::{CloseDriver, Entrant, Run, RunRecords, RunStore},
    run_store_sql::LiveRun,
};

pub use accounting::Budgets;
pub use terminal::{Terminal, TerminalEffects};

/// Install/authorization and host integration are supplied by the embedding
/// runtime. Host methods run without an actor, accounting lock or SQLite
/// connection. Preparation enforces the install-approved manifest selected by
/// run_identities.snapshot_id in run_snapshots, and freezes the request. It must
/// not send the call or commit a value derived from lazy state. The explicitly
/// transactional dispatch_check is SQLite-only; now_ms is a pure clock read.
pub trait Services: CloseDriver + Send + Sync + 'static {
    /// Verify the exact version and code hash in run_identities against core's
    /// install status. Deferred and revoked answers must not activate a worker.
    fn install(&self, run: &LiveRun) -> Result<Gate>;
    fn prepare(&self, run: &LiveRun, call: &HostCall, state: &State) -> Result<Prepared>;
    fn dispatch(&self, run: &LiveRun, intent: &Intent) -> Result<Reply>;
    fn dispatch_committed(&self, run: &LiveRun, position: u64, handle: Option<&str>);
    fn refusal_committed(&self, run: &LiveRun, position: u64);
    /// The final zero-write probe runs in a lease-fenced Store::write immediately
    /// before dispatch. It checks runtime stopping and revocation using only
    /// control-plane state, without a host call.
    fn dispatch_check(&self, tx: &rusqlite::Transaction<'_>, run: &LiveRun) -> Result<()>;
    fn now_ms(&self) -> i64;
    fn saturated(&self, _: &rusqlite::Transaction<'_>, _: &LiveRun, _: &str) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Open,
    Deferred,
    Revoked,
}

pub enum Prepared {
    Revoked {
        class: StoredClass,
        reason: String,
    },
    /// No request exists yet. Retry preparation, not dispatch, after the timer.
    Deferred {
        class: StoredClass,
        cause: String,
        retry_not_before: i64,
    },
    Remote(Box<Intent>),
    Local {
        value: JsonText,
        effect: Option<KvEffect>,
    },
    /// Authorization passed; the driver serves the KV call from SQLite plus
    /// the run's overlay, applying the same size limits to their combined view.
    Kv(crate::kv::KvLimits),
    Sync {
        value: JsonText,
        clock_ms: Option<i64>,
    },
    Refused {
        class: StoredClass,
        value: JsonText,
        notify: bool,
    },
}

pub enum Reply {
    Revoked(String),
    Refused(Box<basal_host::flow_refusal::FlowRefusal>),
    Accepted(String),
    Completed(Arrival),
    Deferred(crate::run_records::Deferred),
    Unknown(String),
}

pub struct Driver {
    store: Arc<Store>,
    registry: Arc<RunStore<Records>>,
    services: Arc<dyn Services>,
    hooks: Arc<dyn Hooks>,
    budgets: Budgets,
}

pub struct Activation {
    driver: Arc<Driver>,
    run: Arc<Run<Records>>,
    live: LiveRun,
    pub generation: u64,
}

pub enum Claim {
    Active(Box<Activation>),
    Deferred,
    Revoked,
}

/// A dispatch owns its counted entrant from the Intent append until its reply
/// is recorded. Dropping an unsent dispatch cannot leave a closer waiting.
pub struct Dispatch {
    driver: Arc<Driver>,
    live: LiveRun,
    generation: u64,
    intent: Intent,
    entrant: Entrant<Records>,
    deadline_at: i64,
}

pub enum Issued {
    Dispatch(Box<Dispatch>),
    Reply(Outcome),
    Pending,
}

fn record_revoked(
    records: &mut Records,
    generation: u64,
    identity: CallIdentity,
    reason: &str,
    audit_at: i64,
) -> Result<()> {
    let position = identity.position;
    if let Some(call) = records.state().calls.get(&position) {
        if call.identity != identity {
            return Err(CoreError::Worker("revoked call identity changed".into()));
        }
    } else {
        records.append(generation, Body::Issued(identity))?;
    }
    let value = crate::kv::rejection("version_revoked", reason).value;
    records.append(
        generation,
        Body::Deferred(crate::run_records::Deferred {
            position,
            retry_not_before: i64::MAX,
            cause: value.as_str().to_owned(),
            audit_at,
            proven_unsent: true,
        }),
    )?;
    records.append(
        generation,
        Body::Arrived(Arrival {
            position,
            arrival_seq: records.state().next_arrival_seq,
            outcome: "rejected".into(),
            settlement: Settlement::Rejected,
            value_hash: crate::ids::payload_hash(Settlement::Rejected, &value),
            value,
            handle: None,
            usage: Some(crate::run_records::Usage {
                input_tokens: 0,
                output_tokens: 0,
            }),
            cost_micros: Some(0),
            kv_effect: None,
            durability: ArrivalDurability {
                refusal_committed: true,
                ..Default::default()
            },
        }),
    )?;
    Ok(())
}

fn control_failure(state: &State) -> Option<(&'static str, String)> {
    for call in state.calls.values() {
        if !call
            .arrival
            .as_ref()
            .is_some_and(|a| a.settlement == Settlement::Rejected)
        {
            continue;
        }
        let Some(deferred) = &call.deferred else {
            continue;
        };
        if let Ok(refusal) =
            serde_json::from_str::<basal_host::flow_refusal::FlowRefusal>(&deferred.cause)
            && refusal.reason == basal_host::flow_refusal::RefusalReason::AgentRetired
        {
            return Some(("agent_retired", refusal.message().to_owned()));
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&deferred.cause)
            && value["code"] == "version_revoked"
        {
            return Some((
                "version_revoked",
                value["message"]
                    .as_str()
                    .unwrap_or("version revoked")
                    .into(),
            ));
        }
    }
    None
}

impl Driver {
    pub(crate) fn owned(self: &Arc<Self>, live: LiveRun, generation: u64) -> Result<Activation> {
        let run = self
            .registry
            .get(&live.run_id)
            .ok_or_else(|| CoreError::OwnerLost {
                run_id: live.run_id.clone(),
                generation,
            })?;
        run.activate(generation, |_| Ok(()))?;
        Ok(Activation {
            driver: self.clone(),
            run,
            live,
            generation,
        })
    }
    /// Timers and the host integration call this after making an awaited call
    /// ready. The readiness check and Woken append share the run actor.
    pub fn wake(&self, run_id: &str) -> Result<bool> {
        let Some(run) = self.registry.get(run_id) else {
            return Ok(false);
        };
        let Ok(entrant) = run.enter() else {
            return Ok(false);
        };
        entrant.finish(|records| {
            let state = records.state();
            let ready = state.awaited.iter().any(|p| {
                state.calls.get(p).is_some_and(|c| {
                    c.arrival.is_some()
                        || c.unknown.is_some()
                        || c.deferred
                            .as_ref()
                            .is_some_and(|d| d.retry_not_before <= self.services.now_ms())
                })
            });
            if state.parked && ready {
                records.append(state.generation, Body::Woken)?;
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }
    /// Control transitions (including a revoked install gate) close without
    /// inventing an activation or first-claim deadline.
    pub fn cancel(&self, terminal: Terminal) -> Result<()> {
        if terminal.state != crate::model::RunState::Cancelled {
            return Err(CoreError::Invalid(
                "cancel requires a cancelled terminal".into(),
            ));
        }
        let run = self
            .registry
            .get(&terminal.live.run_id)
            .ok_or_else(|| CoreError::NoSuchRun(terminal.live.run_id.clone()))?;
        self.registry
            .close(&run, &terminal, self.services.as_ref())?;
        run.closed_hint(terminal.commit_seq());
        Ok(())
    }
    /// Admission and recovery register the complete accounting contribution
    /// before exposing a run, including runs that have not been activated yet.
    pub fn register(&self, live: &LiveRun, mut records: Records) -> Result<Arc<Run<Records>>> {
        if self
            .store
            .read(|c| crate::run_store_sql::history_lost(c, &live.run_id))?
        {
            return Err(CoreError::Invalid(
                "history_lost runs cannot activate".into(),
            ));
        }
        if let Some(existing) = self.registry.get(&live.run_id) {
            return Ok(existing);
        }
        let identity = self.store.read(|conn| {
            crate::run_store_sql::identity(conn, &live.run_id)?
                .ok_or_else(|| CoreError::Corrupt("live run has no identity".into()))
        })?;
        records.bind_identity(identity);
        let rows = accounting::rows(records.state(), &self.budgets)?;
        let inserted = self.registry.insert_accounted(
            live.run_id.clone(),
            live.flow_id.clone(),
            records.state().generation,
            records,
            rows,
        );
        match inserted {
            Ok(run) => {
                #[cfg(any(test, feature = "test-support"))]
                if self.registry.retains_detail() {
                    run.retain_detail_for_test(self.store.clone())?;
                }
                Ok(run)
            }
            Err(error) => self.registry.get(&live.run_id).ok_or(error),
        }
    }
    pub fn new_staged(
        store: Arc<Store>,
        registry: Arc<RunStore<Records>>,
        services: Arc<dyn Services>,
        hooks: Arc<dyn Hooks>,
        budgets: Budgets,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            registry,
            services,
            hooks,
            budgets,
        })
    }

    fn at(&self, run_id: &str, boundary: Boundary) -> Result<()> {
        crate::run_store::lock_order::assert_lock_free();
        if self.hooks.at(run_id, &boundary) == Step::Crash {
            self.store.cut();
            return Err(CoreError::Cut);
        }
        Ok(())
    }

    fn at_run(
        &self,
        live: &LiveRun,
        boundary: Boundary,
        position: Option<u64>,
        state: &State,
    ) -> Result<()> {
        if self
            .hooks
            .at_run(&live.run_id, &live.flow_id, &boundary, position, state)
            == Step::Crash
        {
            self.store.cut();
            return Err(CoreError::Cut);
        }
        Ok(())
    }

    /// The caller holds the flow slot while asking the install gate. A deferred
    /// gate answer publishes neither Claimed nor Activated, even if another run
    /// forces a group sync while the gate is outstanding.
    pub fn claim(self: &Arc<Self>, live: LiveRun, generation: u64) -> Result<Claim> {
        if self
            .store
            .read(|c| crate::run_store_sql::history_lost(c, &live.run_id))?
        {
            return Err(CoreError::Invalid(
                "history_lost runs cannot activate".into(),
            ));
        }
        let run = self
            .registry
            .get(&live.run_id)
            .ok_or_else(|| CoreError::NoSuchRun(live.run_id.clone()))?;
        match self.services.install(&live)? {
            Gate::Deferred => return Ok(Claim::Deferred),
            Gate::Revoked => return Ok(Claim::Revoked),
            Gate::Open => {}
        }
        let now = self.services.now_ms();
        let generation = run.claim(generation, |next, a| {
            let records = a.records();
            if records.state().claim.is_none() {
                records.append(
                    next,
                    Body::Claimed {
                        first_claim_ms: now,
                        deadline_at: now.saturating_add(live.deadline_ms),
                    },
                )?;
            }
            records.append(next, Body::Activated)?;
            a.account(|records, _| Ok(((), accounting::rows(records.state(), &self.budgets)?)))?;
            Ok(next)
        })?;
        Ok(Claim::Active(Box::new(Activation {
            driver: self.clone(),
            run,
            live,
            generation,
        })))
    }
}

impl Activation {
    pub fn state(&self) -> Result<State> {
        self.run
            .activate(self.generation, |a| Ok(a.records().state().clone()))
    }

    pub fn deadline_passed(&self) -> Result<bool> {
        Ok(self
            .state()?
            .claim
            .is_some_and(|claim| self.driver.services.now_ms() >= claim.deadline_at))
    }

    pub fn fingerprint(&self, value: String) -> Result<bool> {
        self.run.activate(self.generation, |a| {
            if a.records()
                .state()
                .fingerprint
                .as_ref()
                .is_some_and(|old| old != &value)
            {
                return Ok(false);
            }
            a.records()
                .append(self.generation, Body::Fingerprint { value })?;
            Ok(true)
        })
    }

    pub fn script_started(&self) -> Result<()> {
        self.run.activate(self.generation, |a| {
            a.records()
                .append(self.generation, Body::ScriptStarted)
                .map(|_| ())
        })
    }

    /// New remote requests are authorized by Services. Existing identities are
    /// checked rather than prepared again, preserving frozen request bytes.
    pub fn issue(&self, call: HostCall) -> Result<Issued> {
        self.issue_inner(call, false)
    }

    fn issue_for_worker(&self, call: HostCall) -> Result<Issued> {
        // Direct driver callers retain the budget-error API. A script instead
        // receives a durable typed rejection that it can catch like other calls.
        self.issue_inner(call, true)
    }

    fn issue_inner(&self, call: HostCall, refuse_budgets: bool) -> Result<Issued> {
        if call.kind.is_sleep()
            || call.kind.is_shell()
            || matches!(call.kind, basal_proto::CallKind::Tool { .. })
        {
            return Err(CoreError::Worker(
                "call is not available to the flow profile".into(),
            ));
        }
        let state = self.state()?;
        if let Some((kind, detail)) = control_failure(&state) {
            return Err(CoreError::Worker(format!("{kind}:{detail}")));
        }
        if state.calls.contains_key(&call.position) {
            state.check_issued(&call)?;
            return Err(CoreError::Worker("worker reissued a recorded call".into()));
        }
        let prepared = self.driver.services.prepare(&self.live, &call, &state)?;
        let prepared = match prepared {
            Prepared::Kv(limits) => {
                local::prepare(&self.driver.store, &self.live, &state, &call, limits)?
            }
            other => other,
        };
        match prepared {
            Prepared::Revoked { class, reason } => {
                self.run.activate(self.generation, |a| {
                    a.account(|records, _| {
                        record_revoked(
                            records,
                            self.generation,
                            CallIdentity::new(call, class),
                            &reason,
                            self.driver.services.now_ms(),
                        )?;
                        Ok(((), accounting::rows(records.state(), &self.driver.budgets)?))
                    })
                })?;
                Ok(Issued::Pending)
            }
            Prepared::Deferred {
                class,
                cause,
                retry_not_before,
            } => {
                self.defer_preparation(call, class, cause, retry_not_before)?;
                Ok(Issued::Pending)
            }
            Prepared::Remote(mut intent) => {
                if intent.call != CallIdentity::new(call, intent.call.class)
                    || matches!(intent.call.class, StoredClass::Local | StoredClass::Sync)
                    || intent.attempts != 1
                {
                    return Err(CoreError::Invalid(
                        "prepared remote identity changed".into(),
                    ));
                }
                intent.idempotency_key = Some(crate::ids::idempotency_key(
                    &self.live.flow_id,
                    &self.live.run_id,
                    intent.call.position,
                ));
                if intent
                    .dispatch_count_window
                    .as_ref()
                    .is_none_or(|c| c.count != 1)
                {
                    return Err(CoreError::Invalid(
                        "remote call must count one dispatch".into(),
                    ));
                }
                self.authorize_or_refuse(*intent, refuse_budgets)
                    .map(|dispatch| match dispatch {
                        Some(dispatch) => Issued::Dispatch(Box::new(dispatch)),
                        None => Issued::Pending,
                    })
            }
            Prepared::Sync { value, clock_ms } => self.run.activate(self.generation, |a| {
                let clock_ms = clock_ms.map(|sample| {
                    a.records()
                        .state()
                        .calls
                        .values()
                        .filter_map(|c| c.sync.as_ref().and_then(|s| s.clock_ms))
                        .max()
                        .unwrap_or(sample)
                        .max(sample)
                });
                let value = clock_ms.map_or(value, |ms| {
                    JsonText::new(ms.to_string()).expect("integer JSON")
                });
                a.records()
                    .reply_sync(self.generation, call, value, clock_ms)
                    .map(Issued::Reply)
            }),
            Prepared::Local { value, effect } => {
                self.local(
                    call,
                    value,
                    effect,
                    false,
                    StoredClass::Local,
                    Settlement::Fulfilled,
                )?;
                Ok(Issued::Pending)
            }
            Prepared::Refused {
                class,
                value,
                notify,
            } => {
                let position = call.position;
                self.local(call, value, None, notify, class, Settlement::Rejected)?;
                if notify {
                    self.driver.services.refusal_committed(&self.live, position);
                }
                Ok(Issued::Pending)
            }
            Prepared::Kv(_) => unreachable!("KV preparation is resolved above"),
        }
    }

    fn local(
        &self,
        call: HostCall,
        value: JsonText,
        effect: Option<KvEffect>,
        notify: bool,
        class: StoredClass,
        settlement: Settlement,
    ) -> Result<()> {
        self.run.activate(self.generation, |a| {
            let records = a.records();
            let position = call.position;
            if records.state().calls.contains_key(&position) {
                records.state().check_issued(&call)?;
            } else {
                records.append(
                    self.generation,
                    Body::Issued(CallIdentity::new(call, class)),
                )?;
            }
            let arrival = Arrival {
                position,
                arrival_seq: records.state().next_arrival_seq,
                outcome: if settlement == Settlement::Fulfilled {
                    "fulfilled"
                } else {
                    "rejected"
                }
                .into(),
                settlement,
                value_hash: crate::ids::payload_hash(settlement, &value),
                value,
                handle: None,
                usage: None,
                cost_micros: None,
                kv_effect: effect,
                durability: ArrivalDurability {
                    refusal_committed: notify,
                    ..Default::default()
                },
            };
            records
                .append(self.generation, Body::Arrived(arrival))
                .map(|_| ())
        })
    }

    fn defer_preparation(
        &self,
        call: HostCall,
        class: StoredClass,
        cause: String,
        retry_not_before: i64,
    ) -> Result<()> {
        if matches!(class, StoredClass::Local | StoredClass::Sync) {
            return Err(CoreError::Invalid("local preparation cannot defer".into()));
        }
        self.run.activate(self.generation, |a| {
            let records = a.records();
            let position = call.position;
            if !records.state().calls.contains_key(&position) {
                records.append(
                    self.generation,
                    Body::Issued(CallIdentity::new(call, class)),
                )?;
            }
            records.append(
                self.generation,
                Body::Deferred(crate::run_records::Deferred {
                    position,
                    retry_not_before,
                    cause,
                    audit_at: self.driver.services.now_ms(),
                    proven_unsent: true,
                }),
            )?;
            Ok(())
        })
    }

    fn refuse_budget(&self, identity: CallIdentity, reason: &str) -> Result<()> {
        let code = if reason.starts_with("dispatch") {
            crate::authorize::codes::DISPATCH_BUDGET
        } else if reason.starts_with("token") {
            crate::authorize::codes::TOKEN_CAP
        } else {
            "budget_exhausted"
        };
        self.run.activate(self.generation, |a| {
            a.account(|records, _| {
                if !records.state().calls.contains_key(&identity.position) {
                    records.append(self.generation, Body::Issued(identity.clone()))?;
                }
                // Saturation can disable the flow and enqueue an owner notice.
                // That decision may depend on local, clock or random values not
                // yet synced to disk. Sync all preceding records before writing
                // the decision to SQLite so recovery retains those inputs.
                records.sync_terminal()?;
                self.driver
                    .store
                    .write(|tx| self.driver.services.saturated(tx, &self.live, reason))?;
                let value = crate::kv::rejection(code, reason).value;
                records.append(
                    self.generation,
                    Body::Arrived(Arrival {
                        position: identity.position,
                        arrival_seq: records.state().next_arrival_seq,
                        outcome: "rejected".into(),
                        settlement: Settlement::Rejected,
                        value_hash: crate::ids::payload_hash(Settlement::Rejected, &value),
                        value,
                        handle: None,
                        usage: None,
                        cost_micros: None,
                        kv_effect: None,
                        durability: ArrivalDurability {
                            refusal_committed: true,
                            ..Default::default()
                        },
                    }),
                )?;
                Ok(((), accounting::rows(records.state(), &self.driver.budgets)?))
            })
        })
    }

    fn authorize_or_refuse(
        &self,
        intent: Intent,
        refuse_budgets: bool,
    ) -> Result<Option<Dispatch>> {
        let identity = intent.call.clone();
        match self.authorize(intent) {
            Ok(dispatch) => Ok(Some(dispatch)),
            Err(CoreError::Invalid(reason))
                if refuse_budgets && reason.ends_with("budget exhausted") =>
            {
                self.refuse_budget(identity, &reason)?;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn authorize(&self, intent: Intent) -> Result<Dispatch> {
        let (entrant, cut, deadline_at) = self.run.activate(self.generation, |a| {
            if a.is_dispatching(intent.call.position) {
                return Err(CoreError::Unavailable("call already dispatching".into()));
            }
            let cut = a.account(|records, others| {
                let body = Body::Intent(intent.clone());
                let proposed = records.preview(self.generation, &body)?;
                let rows = accounting::rows(&proposed, &self.driver.budgets)?;
                self.driver.store.read(|conn| {
                    accounting::check(
                        conn,
                        &self.live.flow_id,
                        &self.live.run_id,
                        &self.driver.budgets,
                        others,
                        &rows,
                    )
                })?;
                let (cut, _) = records.append_pending(self.generation, body)?;
                Ok((cut, rows))
            })?;
            // No closer may pass between the possibly-sent fact and this count.
            let deadline_at = a
                .records()
                .state()
                .claim
                .as_ref()
                .ok_or_else(|| CoreError::Corrupt("dispatch has no claim".into()))?
                .deadline_at;
            self.driver.at_run(
                &self.live,
                Boundary::DriverAppended,
                Some(intent.call.position),
                a.records().state(),
            )?;
            Ok((
                a.dispatch_entrant_for(intent.call.position),
                cut,
                deadline_at,
            ))
        })?;
        self.driver
            .at(&self.live.run_id, Boundary::DriverAppended)?;
        entrant.finish(|records| {
            records.sync_through(cut)?;
            self.driver.at_run(
                &self.live,
                Boundary::DriverSynced,
                Some(intent.call.position),
                records.state(),
            )
        })?;
        self.driver.at(&self.live.run_id, Boundary::DriverSynced)?;
        Ok(Dispatch {
            driver: self.driver.clone(),
            live: self.live.clone(),
            generation: self.generation,
            intent,
            entrant,
            deadline_at,
        })
    }

    /// A durable unkeyed Intent is possibly sent, including a crash before the
    /// actual send. Only proven-unsent deferrals may retry it automatically.
    pub fn recover_dispatches(&self) -> Result<Vec<Dispatch>> {
        self.recover_dispatches_inner(false)
    }

    fn recover_dispatches_for_worker(&self) -> Result<Vec<Dispatch>> {
        self.recover_dispatches_inner(true)
    }

    fn recover_dispatches_inner(&self, refuse_budgets: bool) -> Result<Vec<Dispatch>> {
        let mut dispatches = Vec::new();
        for call in self.state()?.calls.values() {
            if call.arrival.is_some() || call.sync.is_some() || call.accepted.is_some() {
                continue;
            }
            let Some(intent) = &call.intent else {
                if let Some(deferred) = &call.deferred {
                    if self.deadline_passed()? {
                        let value = serde_json::from_str::<basal_host::flow_refusal::FlowRefusal>(
                            &deferred.cause,
                        )
                        .map(|r| r.outcome().value)
                        .unwrap_or_else(|_| {
                            crate::kv::rejection("route_unavailable", &deferred.cause).value
                        });
                        self.local(
                            HostCall {
                                position: call.identity.position,
                                kind: call.identity.kind.clone(),
                                args: call.identity.args.clone(),
                            },
                            value,
                            None,
                            true,
                            call.identity.class,
                            Settlement::Rejected,
                        )?;
                        continue;
                    }
                    if deferred.retry_not_before > self.driver.services.now_ms() {
                        continue;
                    }
                }
                let call = HostCall {
                    position: call.identity.position,
                    kind: call.identity.kind.clone(),
                    args: call.identity.args.clone(),
                };
                let state = self.state()?;
                let prepared = match self.driver.services.prepare(&self.live, &call, &state)? {
                    Prepared::Kv(limits) => {
                        local::prepare(&self.driver.store, &self.live, &state, &call, limits)?
                    }
                    other => other,
                };
                match prepared {
                    Prepared::Revoked { class, reason } => {
                        self.run.activate(self.generation, |a| {
                            a.account(|records, _| {
                                record_revoked(
                                    records,
                                    self.generation,
                                    CallIdentity::new(call, class),
                                    &reason,
                                    self.driver.services.now_ms(),
                                )?;
                                Ok(((), accounting::rows(records.state(), &self.driver.budgets)?))
                            })
                        })?;
                    }
                    Prepared::Deferred {
                        class,
                        cause,
                        retry_not_before,
                    } => self.defer_preparation(call, class, cause, retry_not_before)?,
                    Prepared::Remote(intent)
                        if call.position == intent.call.position
                            && CallIdentity::new(call.clone(), intent.call.class)
                                == intent.call
                            && intent.attempts == 1 =>
                    {
                        if let Some(dispatch) = self.authorize_or_refuse(*intent, refuse_budgets)? {
                            dispatches.push(dispatch);
                        }
                    }
                    Prepared::Local { value, effect } => self.local(
                        call,
                        value,
                        effect,
                        false,
                        StoredClass::Local,
                        Settlement::Fulfilled,
                    )?,
                    Prepared::Refused {
                        class,
                        value,
                        notify,
                    } => {
                        let position = call.position;
                        self.local(call, value, None, notify, class, Settlement::Rejected)?;
                        if notify {
                            self.driver.services.refusal_committed(&self.live, position);
                        }
                    }
                    _ => {
                        return Err(CoreError::Corrupt(
                            "issued call changed service class".into(),
                        ));
                    }
                }
                continue;
            };
            if self.run.activate(self.generation, |a| {
                Ok(a.is_dispatching(intent.call.position))
            })? {
                continue;
            }
            if let Some(deferred) = &call.deferred
                && deferred.retry_not_before > self.driver.services.now_ms()
            {
                continue;
            }
            if call.unknown.is_some() {
                continue;
            }
            if !intent.call.class.safe_to_resend()
                && !call.deferred.as_ref().is_some_and(|d| d.proven_unsent)
                && !call
                    .resolutions
                    .last()
                    .is_some_and(|r| r.choice == "not_applied" && r.attempts == intent.attempts)
            {
                self.run.activate(self.generation, |a| {
                    a.records()
                        .append(
                            self.generation,
                            Body::Unknown {
                                position: call.identity.position,
                                reason: "basal_restarted".into(),
                            },
                        )
                        .map(|_| ())
                })?;
                continue;
            }
            let mut retry = intent.clone();
            retry.attempts = retry
                .attempts
                .checked_add(1)
                .ok_or_else(|| CoreError::Corrupt("attempts exhausted".into()))?;
            retry.audit_at = self.driver.services.now_ms();
            retry.dispatch_count_window = Some(crate::run_records::DispatchCount {
                window_start: retry
                    .audit_at
                    .div_euclid(self.driver.budgets.dispatch_window_ms)
                    * self.driver.budgets.dispatch_window_ms,
                count: 1,
            });
            let proven_unsent = call.deferred.as_ref().is_some_and(|d| d.proven_unsent)
                || call
                    .resolutions
                    .last()
                    .is_some_and(|r| r.choice == "not_applied");
            if let Some(dispatch) =
                self.authorize_or_refuse(retry, refuse_budgets && proven_unsent)?
            {
                dispatches.push(dispatch);
            }
        }
        Ok(dispatches)
    }

    pub fn release(&self, awaited: &[u64]) -> Result<Option<Outcome>> {
        let outcome = self.run.activate(self.generation, |a| {
            let outcome = a.records().release_next(self.generation, awaited)?;
            if outcome.is_some() {
                self.driver.at_run(
                    &self.live,
                    Boundary::DriverReleased,
                    outcome.as_ref().map(|o| o.position),
                    a.records().state(),
                )?;
            }
            Ok(outcome)
        })?;
        if outcome.is_some() {
            self.driver
                .at(&self.live.run_id, Boundary::DriverReleased)?;
        }
        Ok(outcome)
    }

    pub fn park(&self, awaited: &[u64]) -> Result<bool> {
        self.run.activate(self.generation, |a| {
            let records = a.records();
            let waiting: BTreeSet<_> = awaited.iter().copied().collect();
            if waiting.is_empty()
                || waiting.len() != awaited.len()
                || waiting
                    .iter()
                    .any(|p| !records.state().calls.contains_key(p))
            {
                return Err(CoreError::Worker("invalid suspended awaited set".into()));
            }
            if waiting.iter().any(|p| {
                records.state().calls.get(p).is_some_and(|c| {
                    c.arrival.is_some()
                        || c.unknown.is_some()
                        || c.deferred
                            .as_ref()
                            .is_some_and(|d| d.retry_not_before <= self.driver.services.now_ms())
                })
            }) {
                return Ok(false);
            }
            if waiting.iter().any(|p| {
                records
                    .state()
                    .calls
                    .get(p)
                    .is_some_and(|c| c.accepted.is_none() && c.deferred.is_none())
            }) {
                return Err(CoreError::Worker(
                    "suspension requires accepted or deferred calls".into(),
                ));
            }
            records.append(
                self.generation,
                Body::Parked {
                    awaited: awaited.to_vec(),
                },
            )?;
            Ok(true)
        })
    }

    pub fn requeue(&self, broken: bool) -> Result<()> {
        self.run.activate(self.generation, |a| {
            let broken = a
                .records()
                .state()
                .broken
                .checked_add(u64::from(broken))
                .ok_or_else(|| CoreError::Corrupt("broken count exhausted".into()))?;
            a.records()
                .append(self.generation, Body::Requeued { broken })
                .map(|_| ())
        })
    }

    pub fn finish(&self, terminal: Terminal) -> Result<()> {
        if terminal.live != self.live {
            return Err(CoreError::Invalid("terminal admission changed".into()));
        }
        self.driver.registry.close_owned(
            &self.run,
            self.generation,
            &terminal,
            self.driver.services.as_ref(),
        )?;
        self.run.closed_hint(terminal.commit_seq());
        Ok(())
    }
}

impl Dispatch {
    /// Embedders may move this owned dispatch to a thread. It is already synced
    /// and counted, so close can stop the worker without racing the host reply.
    pub fn send(self) -> Result<()> {
        let check = self
            .driver
            .store
            .write(|tx| self.driver.services.dispatch_check(tx, &self.live));
        let reply = if let Err(error) = check {
            match error {
                CoreError::Invalid(reason) => {
                    let uncertain = self.intent.attempts > 1
                        && self.intent.call.class != StoredClass::Query
                        && self.entrant.inspect(|_, records| {
                            Ok(records
                                .state()
                                .calls
                                .get(&self.intent.call.position)
                                .is_none_or(|c| {
                                    !c.prior_intents
                                        .last()
                                        .and_then(|i| i.dispatch_count_window.as_ref())
                                        .is_some_and(|d| d.count == 0)
                                        && !c.resolutions.last().is_some_and(|r| {
                                            r.choice == "not_applied"
                                                && r.attempts.saturating_add(1)
                                                    == self.intent.attempts
                                        })
                                }))
                        })?;
                    if uncertain {
                        Reply::Unknown("basal_restarted".into())
                    } else {
                        Reply::Revoked(reason)
                    }
                }
                other => return Err(other),
            }
        } else if self.driver.services.now_ms() >= self.deadline_at {
            Reply::Deferred(crate::run_records::Deferred {
                position: self.intent.call.position,
                retry_not_before: self.deadline_at,
                cause: "run deadline elapsed before dispatch".into(),
                audit_at: self.intent.audit_at,
                proven_unsent: true,
            })
        } else {
            match self.driver.services.dispatch(&self.live, &self.intent) {
                Ok(reply) => reply,
                Err(CoreError::Cut) => return Err(CoreError::Cut),
                Err(error) => Reply::Unknown(error.to_string()),
            }
        };
        self.driver
            .at(&self.live.run_id, Boundary::DriverDispatched)?;
        self.entrant.inspect(|_, records| {
            self.driver.at_run(
                &self.live,
                Boundary::DriverDispatched,
                Some(self.intent.call.position),
                records.state(),
            )
        })?;
        let position = self.intent.call.position;
        let mut handle = None;
        let mut committed = false;
        let mut arrived = false;
        let mut refused = false;
        self.entrant.account(|records| {
            match reply {
                Reply::Revoked(reason) => {
                    record_revoked(
                        records,
                        self.generation,
                        self.intent.call.clone(),
                        &reason,
                        self.intent.audit_at,
                    )?;
                    arrived = true;
                }
                Reply::Refused(refusal) => {
                    let state = records.state();
                    let value = refusal.outcome().value;
                    let arrival_seq = state.next_arrival_seq;
                    records.append(
                        self.generation,
                        Body::Deferred(crate::run_records::Deferred {
                            position,
                            retry_not_before: i64::MAX,
                            cause: serde_json::to_string(&refusal)
                                .map_err(|e| CoreError::Invalid(e.to_string()))?,
                            audit_at: self.intent.audit_at,
                            proven_unsent: true,
                        }),
                    )?;
                    if refusal.readiness() {
                        records.append(
                            self.generation,
                            Body::ScopeCut {
                                deadline_exempt: true,
                            },
                        )?;
                    }
                    records.append(
                        self.generation,
                        Body::Arrived(Arrival {
                            position,
                            arrival_seq,
                            outcome: "rejected".into(),
                            settlement: Settlement::Rejected,
                            value_hash: crate::ids::payload_hash(Settlement::Rejected, &value),
                            value,
                            handle: None,
                            usage: Some(crate::run_records::Usage {
                                input_tokens: 0,
                                output_tokens: 0,
                            }),
                            cost_micros: Some(0),
                            kv_effect: None,
                            durability: ArrivalDurability {
                                refusal_committed: true,
                                immediate_completion: true,
                                ..Default::default()
                            },
                        }),
                    )?;
                    committed = true;
                    arrived = true;
                    refused = true;
                }
                Reply::Accepted(h) => {
                    records.append(
                        self.generation,
                        Body::Accepted {
                            position,
                            handle: h.clone(),
                        },
                    )?;
                    handle = Some(h);
                    committed = true;
                }
                Reply::Completed(mut arrival) => {
                    arrival.position = position;
                    arrival.arrival_seq = records.state().next_arrival_seq;
                    arrival.durability.immediate_completion = true;
                    records.append(self.generation, Body::Arrived(arrival))?;
                    committed = true;
                    arrived = true;
                }
                Reply::Deferred(mut deferred) => {
                    deferred.position = position;
                    records.append(self.generation, Body::Deferred(deferred))?;
                }
                Reply::Unknown(reason) => {
                    records.append(self.generation, Body::Unknown { position, reason })?;
                }
            }
            if records.state().parked {
                records.append(self.generation, Body::Woken)?;
            }
            if arrived {
                self.driver.at_run(
                    &self.live,
                    Boundary::HostArrived,
                    Some(position),
                    records.state(),
                )?;
            }
            Ok(((), accounting::rows(records.state(), &self.driver.budgets)?))
        })?;
        if committed {
            if arrived {
                self.driver.at(&self.live.run_id, Boundary::HostArrived)?;
            }
            if refused {
                self.driver.services.refusal_committed(&self.live, position);
            } else {
                self.driver
                    .services
                    .dispatch_committed(&self.live, position, handle.as_deref());
            }
        }
        Ok(())
    }
}
