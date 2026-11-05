//! End-to-end composition selected only by `Runtime::new_staged`. The ordinary
//! constructor keeps the SQLite execution model until the cutover.

#[cfg(test)]
#[path = "staged_runtime_tests.rs"]
pub(crate) mod tests;

use crate::{
    CoreError, Result, Runtime,
    admission::{Admission, TriggerSpec},
    channel::WorkerChannel,
    driver::CallPreparation,
    manifest::Manifest,
    model::{Run, RunState, StoredClass},
    run_admission::RunAdmission,
    run_driver::{self, Driver, Gate, Prepared, Reply, Terminal, TerminalEffects},
    run_records::{
        Arrival, ArrivalDurability, CallIdentity, DispatchCount, Intent, Records, State,
    },
    run_store::{CloseDriver, TerminalWrite},
    run_store_sql::{self as sql, LiveRun},
    runs::Lease,
    runtime::{ActivationEnd, Config, Shared},
};
use basal_host::{CallClass, CallRequest, Dispatched, HostOutcome, InstallStatus, TransportError};
use basal_proto::{
    ActivationRequest, CallKind, HostCall, JsonText, Primitive, Profile, Settlement,
};
use rusqlite::{OptionalExtension, params};
use std::sync::{Arc, Mutex, Weak};

struct Services {
    shared: Weak<Shared>,
    config: Arc<Config>,
    acknowledgements: Mutex<std::collections::HashMap<(String, u64), CallRequest>>,
    audit_actor: Option<String>,
}

struct EventWakes<'a>(&'a Runtime);
impl crate::run_admission::EventWakes for EventWakes<'_> {
    fn wake(
        &self,
        tx: &rusqlite::Transaction<'_>,
        notice: &crate::events::Notice,
        now: i64,
    ) -> Result<usize> {
        crate::events::broca_wakes(tx, notice, now)
    }
    fn committed(&self) {
        self.0.shared.host.model_events_committed();
        self.0.shared.signal.bump();
    }
}
impl Services {
    fn retry_at(
        &self,
        rt: &Runtime,
        live: &LiveRun,
        intent: &Intent,
        refusal: &basal_host::flow_refusal::FlowRefusal,
    ) -> i64 {
        let shift = u32::try_from(intent.attempts.saturating_sub(1))
            .unwrap_or(31)
            .min(31);
        let delay = rt
            .config
            .deferral_retry
            .saturating_mul(1u32 << shift)
            .min(rt.config.deferral_retry_max)
            .max(std::time::Duration::from_millis(
                refusal.retry_after_ms.unwrap_or(0),
            ));
        let deadline = rt
            .staged_snapshot(&live.run_id)
            .and_then(|s| s.claim.map(|c| c.deadline_at))
            .unwrap_or(i64::MAX);
        self.config
            .clock
            .now_ms()
            .saturating_add(i64::try_from(delay.as_millis()).unwrap_or(i64::MAX))
            .min(deadline)
    }
    fn runtime(&self) -> Result<Runtime> {
        Ok(Runtime {
            shared: self
                .shared
                .upgrade()
                .ok_or_else(|| CoreError::Unavailable("runtime stopped".into()))?,
            config: self.config.clone(),
        })
    }
}

impl CloseDriver for Services {
    fn stop_worker(&self, id: &str) -> Result<()> {
        let rt = self.runtime()?;
        let mut workers = rt.shared.staged_workers.lock().unwrap();
        while workers
            .iter()
            .any(|((run, _), thread)| run == id && *thread != std::thread::current().id())
        {
            workers = rt.shared.staged_stopped.wait(workers).unwrap();
        }
        Ok(())
    }
    fn closed(&self, id: &str) {
        if let Ok(rt) = self.runtime() {
            rt.shared.staged_owners.lock().unwrap().remove(id);
            rt.shared.signal.bump();
            if let Ok(run) = rt.run(id) {
                let _ = rt.release_unused_flow_routes_for(&run.flow_id);
                rt.wake_flow(&run.flow_id);
            }
        }
    }
}

struct Effects {
    live: LiveRun,
    now: i64,
    end_instance: bool,
    refusals: Vec<String>,
    audit_actor: Option<String>,
}
impl TerminalWrite for Effects {
    fn apply(&self, tx: &rusqlite::Transaction<'_>) -> Result<()> {
        if let Some(actor) = &self.audit_actor {
            crate::reconcile::audit(tx, actor, "cancel", Some(&self.live.run_id), None, "")?;
        }
        crate::owner_notice::finish_run_staged(tx, &self.live.run_id, self.now)?;
        for body in &self.refusals {
            tx.execute(
                "INSERT INTO outbox(at,kind,flow_id,body) VALUES(?1,'refusal_committed',?2,?3)",
                params![self.now, self.live.flow_id, body],
            )?;
        }
        if self.end_instance {
            let current: Option<i64> = tx
                .query_row(
                    "SELECT incarnation FROM template_instances WHERE flow_id=?1",
                    [&self.live.flow_id],
                    |r| r.get(0),
                )
                .optional()?;
            if current.is_some() && current == self.live.template_incarnation {
                crate::templates::end(tx, &self.live.flow_id, self.now)?;
            }
        }
        Ok(())
    }
}
impl TerminalEffects for Services {
    fn capture(
        &self,
        live: &LiveRun,
        state: &State,
        _: RunState,
    ) -> Result<Box<dyn TerminalWrite>> {
        let mut refusals = Vec::new();
        for call in state.calls.values().filter(|c| c.arrival.is_some()) {
            if let (Some(intent), Some(deferred)) = (&call.intent, &call.deferred)
                && let Ok(refusal) =
                    serde_json::from_str::<basal_host::flow_refusal::FlowRefusal>(&deferred.cause)
            {
                let saved = crate::journal::SavedRefusal::new(&request(live, intent)?, &refusal)?;
                refusals.push(
                    serde_json::to_string(&saved).map_err(|e| CoreError::Invalid(e.to_string()))?,
                );
            }
        }
        Ok(Box::new(Effects {
            live: live.clone(),
            now: self.config.clock.now_ms(),
            end_instance: live.end_instance
                || state.calls.values().any(|c| {
                    c.identity.kind == CallKind::Primitive(Primitive::InstanceEnd)
                        && c.arrival
                            .as_ref()
                            .is_some_and(|a| a.settlement == Settlement::Fulfilled)
                }),
            refusals,
            audit_actor: self.audit_actor.clone(),
        }))
    }
}

fn request(live: &LiveRun, intent: &Intent) -> Result<CallRequest> {
    Ok(CallRequest {
        flow_id: live.flow_id.clone(),
        flow_version: Some(live.flow_version),
        run_id: live.run_id.clone(),
        position: intent.call.position,
        kind: intent.call.kind.clone(),
        args: intent.frozen_request.clone(),
        idempotency_key: intent.idempotency_key.clone().unwrap_or_else(|| {
            crate::ids::idempotency_key(&live.flow_id, &live.run_id, intent.call.position)
        }),
        attempt: u32::try_from(intent.attempts)
            .map_err(|_| CoreError::Invalid("attempt overflow".into()))?,
    })
}

fn arrival(
    position: u64,
    outcome: HostOutcome,
    reservation: Option<&crate::run_records::Reservation>,
) -> Arrival {
    let value: Option<serde_json::Value> = serde_json::from_str(outcome.value.as_str()).ok();
    let cost_micros = reservation
        .filter(|r| r.cost_micros > 0 || r.input_tokens.saturating_add(r.output_tokens) == 0)
        .map(|r| {
            (crate::classify_cost::charge(r.cost_micros as f64 / 1_000_000.0, value.as_ref(), None)
                * 1_000_000.0)
                .ceil() as u64
        });
    let usage = outcome.usage.map(|u| {
        let input = u
            .input_tokens
            .unwrap_or(0)
            .saturating_add(u.cache_write_tokens.unwrap_or(0));
        let output = u.output_tokens.unwrap_or(0);
        let missing =
            u.input_tokens.is_none() || u.cache_write_tokens.is_none() || u.output_tokens.is_none();
        let reserved = reservation.map_or(0, |r| r.input_tokens.saturating_add(r.output_tokens));
        crate::run_records::Usage {
            input_tokens: if missing {
                input.max(reserved.saturating_sub(output))
            } else {
                input
            },
            output_tokens: output,
        }
    });
    Arrival {
        position,
        arrival_seq: 0,
        outcome: if outcome.settlement == Settlement::Fulfilled {
            "fulfilled"
        } else {
            "rejected"
        }
        .into(),
        value_hash: crate::ids::payload_hash(outcome.settlement, &outcome.value),
        settlement: outcome.settlement,
        value: outcome.value,
        handle: None,
        usage,
        cost_micros,
        kv_effect: None,
        durability: ArrivalDurability::default(),
    }
}

impl run_driver::Services for Services {
    fn install(&self, live: &LiveRun) -> Result<Gate> {
        let rt = self.runtime()?;
        let revoked = rt.store().read(|c| {
            Ok(crate::packages::removed(c, &live.flow_id)?
                || crate::install::revocation(c, &live.flow_id, live.flow_version)?.is_some())
        })?;
        if revoked {
            return Ok(Gate::Revoked);
        }
        if rt.config.install_gate == crate::gate::InstallGate::Off {
            return Ok(Gate::Open);
        }
        let identity = rt
            .store()
            .read(|c| sql::identity(c, &live.run_id))?
            .ok_or_else(|| CoreError::NoSuchRun(live.run_id.clone()))?;
        match rt
            .shared
            .host
            .install_status(&live.flow_id, live.flow_version)
        {
            Ok(InstallStatus::Active { code_hash, scope })
                if code_hash == crate::ids::hex(&identity.code_hash) =>
            {
                rt.configure_flow_version(&live.flow_id, live.flow_version, scope);
                rt.clear_backoff(&live.run_id);
                Ok(Gate::Open)
            }
            Err(_) => {
                rt.back_off(&live.run_id)?;
                Ok(Gate::Deferred)
            }
            Ok(_) => {
                rt.store().write(|tx| {
                    crate::install::revoke(
                        tx,
                        &live.flow_id,
                        live.flow_version,
                        "core no longer approves the pinned install",
                        rt.config.clock.now_ms(),
                    )
                })?;
                Ok(Gate::Revoked)
            }
        }
    }
    fn prepare(&self, live: &LiveRun, call: &HostCall, state: &State) -> Result<Prepared> {
        let rt = self.runtime()?;
        if let Some(reason) = rt
            .store()
            .read(|c| crate::install::revocation(c, &live.flow_id, live.flow_version))?
        {
            let args = serde_json::from_str::<serde_json::Value>(call.args.as_str()).ok();
            return Ok(Prepared::Revoked {
                class: state.calls.get(&call.position).map_or_else(
                    || rt.class_of(&call.kind, args.as_ref()),
                    |c| c.identity.class,
                ),
                reason,
            });
        }
        let bytes: u64 = state
            .calls
            .values()
            .map(|c| {
                (c.identity.args.len()
                    + c.intent.as_ref().map_or(0, |i| i.frozen_request.len())
                    + c.arrival.as_ref().map_or(0, |a| a.value.len())
                    + c.sync.as_ref().map_or(0, |s| s.value.len())) as u64
            })
            .sum();
        if !state.calls.contains_key(&call.position)
            && (state.calls.len() as u64 >= rt.config.limits.max_calls
                || bytes.saturating_add(call.args.len() as u64)
                    > rt.config.limits.max_journal_bytes)
        {
            return Err(CoreError::Worker(
                "run_limit: the run reached its call or replay-size limit".into(),
            ));
        }
        let run = rt.store().read(|c| {
            crate::runs::load_staged(c, &live.run_id, Some(state), true, &rt.config.owner)
        })?;
        let mut manifest =
            Manifest::parse_stored(&run.manifest).map_err(|e| CoreError::Corrupt(e.to_string()))?;
        if let Some(agent) = live.self_input["agent_id"].as_str() {
            crate::authorize::resolve_self(&mut manifest, agent);
        }
        if manifest.params.is_some() {
            let guards: Vec<basal_host::builtins::TemplateRoot> =
                serde_json::from_value(live.grant_guards.clone())
                    .map_err(|e| CoreError::Corrupt(e.to_string()))?;
            crate::templates::effective_instance(&mut manifest, &live.params_input, &guards);
        }
        let policies = rt
            .store()
            .read(|c| crate::classify_cost::load(c, &live.flow_id, live.flow_version))?;
        let lease = Lease {
            run_id: live.run_id.clone(),
            generation: state.generation,
            owner: rt.config.owner.clone(),
        };
        let approved = Some(manifest);
        let preparation = CallPreparation {
            rt: &rt,
            lease: &lease,
            run: &run,
            manifest: &approved,
            input: &live.self_input,
            trigger: &live.trigger,
            tool_policies: &policies,
            state: Some(state),
        }
        .prepare(call);
        let mut prepared = match preparation {
            Ok(prepared) => prepared,
            Err(refusal) => {
                return Ok(Prepared::Refused {
                    class: rt.class_of(&call.kind, None),
                    value: refusal.outcome().value,
                    notify: false,
                });
            }
        };
        if bytes.saturating_add(
            (if state.calls.contains_key(&call.position) {
                0
            } else {
                call.args.len()
            } + prepared.request.as_ref().map_or(0, JsonText::len)) as u64,
        ) > rt.config.limits.max_journal_bytes
        {
            return Err(CoreError::Worker(
                "run_limit: the run reached its replay-size limit".into(),
            ));
        }
        let class = if prepared.classify_cost {
            StoredClass::Mutation
        } else {
            rt.class_of(&call.kind, prepared.args.as_ref())
        };
        if !matches!(class, StoredClass::Local | StoredClass::Sync)
            && rt
                .store()
                .read(|c| crate::install::is_disabled(c, &live.flow_id))?
        {
            return Ok(Prepared::Refused {
                class,
                value: crate::driver::disabled_refusal().outcome().value,
                notify: false,
            });
        }
        let mut cost_micros = 0;
        if prepared.classify_cost {
            let hosts = rt.shared.staged_hosts.as_ref().unwrap();
            let budgets = hosts.budgets_for(&live.run_id)?;
            let remaining = hosts.registry().accounting_read(&live.flow_id, rt.store(), |conn, runs| {
                let committed = crate::classify_cost::spent(conn, "flow", &live.run_id)?;
                let own: u64 = runs.get(&live.run_id).into_iter().flatten().filter(|r| r.window.0.starts_with("cost:")).map(|r| r.amount).sum();
                let mut remaining = budgets.cost_micros as f64 / 1_000_000.0 - committed - own as f64 / 1_000_000.0;
                if let Some((ms, cap)) = budgets.cost_window {
                    let floor = self.now_ms().saturating_sub(ms);
                    let committed: f64 = conn.query_row("SELECT COALESCE(SUM(COALESCE(charged,reserved)),0) FROM classify_costs WHERE kind='flow' AND flow_id=?1 AND created_at>?2", params![live.flow_id,floor], |r| r.get(0))?;
                    let pending: u64 = runs.values().flatten().filter(|r| r.window.0.starts_with("cost:") && r.audit_at > floor).map(|r|r.amount).sum();
                    remaining = remaining.min(cap as f64 / 1_000_000.0 - committed - pending as f64 / 1_000_000.0);
                }
                Ok(remaining.max(0.0))
            })?;
            if remaining <= 0.0 {
                return Ok(Prepared::Refused {
                    class,
                    value: crate::kv::rejection(
                        "budget_exhausted",
                        "classifier dollar cap exhausted",
                    )
                    .value,
                    notify: false,
                });
            }
            let mut input = prepared.args.clone().unwrap_or(serde_json::Value::Null);
            if !input.is_object() {
                return Ok(Prepared::Refused {
                    class,
                    value: crate::kv::rejection(
                        "invalid_params",
                        "classifier arguments must be an object",
                    )
                    .value,
                    notify: false,
                });
            }
            crate::classify_cost::clamp(&mut input, remaining);
            let Some(reserved) = input["max_cost_usd"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0 && *n * 1_000_000.0 <= u64::MAX as f64)
            else {
                return Ok(Prepared::Refused {
                    class,
                    value: crate::kv::rejection(
                        "invalid_params",
                        "max_cost_usd must be non-negative",
                    )
                    .value,
                    notify: false,
                });
            };
            cost_micros = (reserved * 1_000_000.0).ceil() as u64;
            prepared.request = Some(crate::run_readers::json(&input)?);
        }
        if let Some(deferred) = prepared.deferred {
            return Ok(Prepared::Deferred {
                class,
                cause: serde_json::to_string(&deferred)
                    .map_err(|e| CoreError::Invalid(e.to_string()))?,
                retry_not_before: self.now_ms().saturating_add(
                    i64::try_from(rt.config.deferral_retry.as_millis()).unwrap_or(i64::MAX),
                ),
            });
        }
        match class {
            StoredClass::Sync => {
                let now = matches!(call.kind, CallKind::Primitive(Primitive::Now));
                let value = if now {
                    rt.config.clock.now_ms().to_string()
                } else {
                    rt.shared.host.random().to_string()
                };
                Ok(Prepared::Sync {
                    value: JsonText::new(value).map_err(|e| CoreError::Invalid(e.to_string()))?,
                    clock_ms: now.then(|| rt.config.clock.now_ms()),
                })
            }
            StoredClass::Local => {
                if call.kind == CallKind::Primitive(Primitive::InstanceEnd) {
                    if prepared
                        .args
                        .as_ref()
                        .is_none_or(|v| !v.as_object().is_some_and(|o| o.is_empty()))
                    {
                        return Ok(Prepared::Refused {
                            class,
                            value: crate::kv::rejection(
                                "invalid_arguments",
                                "instance.end takes no arguments",
                            )
                            .value,
                            notify: false,
                        });
                    }
                    if live.template_incarnation.is_none() {
                        return Ok(Prepared::Refused {
                            class,
                            value: crate::kv::rejection(
                                "not_a_template_instance",
                                "only template instances may end themselves",
                            )
                            .value,
                            notify: false,
                        });
                    }
                    Ok(Prepared::Local {
                        value: JsonText::new("true").expect("static JSON"),
                        effect: None,
                    })
                } else {
                    Ok(Prepared::Kv(rt.config.kv))
                }
            }
            _ => {
                let now = self.now_ms();
                let mut budgets = rt
                    .shared
                    .staged_hosts
                    .as_ref()
                    .unwrap()
                    .budgets_for(&live.run_id)?;
                budgets.dispatch_window_ms = i64::try_from(rt.config.rate.window.as_millis())
                    .map_err(|_| CoreError::Invalid("invalid rate window".into()))?;
                let reservation =
                    (prepared.tokens.is_some() || prepared.classify_cost).then(|| {
                        crate::run_records::Reservation {
                            input_tokens: prepared.tokens.unwrap_or(0),
                            output_tokens: 0,
                            cost_micros,
                            window_start: now.div_euclid(budgets.token_window_ms)
                                * budgets.token_window_ms,
                        }
                    });
                Ok(Prepared::Remote(Box::new(Intent {
                    call: CallIdentity::new(call.clone(), class),
                    idempotency_key: Some(crate::ids::idempotency_key(
                        &live.flow_id,
                        &live.run_id,
                        call.position,
                    )),
                    frozen_request: prepared.request.unwrap_or_else(|| call.args.clone()),
                    attempts: 1,
                    dispatch_count_window: Some(DispatchCount {
                        window_start: now.div_euclid(budgets.dispatch_window_ms)
                            * budgets.dispatch_window_ms,
                        count: 1,
                    }),
                    audit_at: now,
                    reservation,
                })))
            }
        }
    }
    fn dispatch(&self, live: &LiveRun, intent: &Intent) -> Result<Reply> {
        let rt = self.runtime()?;
        let mut request = request(live, intent)?;
        let class = match intent.call.class {
            StoredClass::Query => CallClass::Query,
            class => CallClass::Mutation {
                honours_idempotency_keys: class == StoredClass::KeyedMutation,
            },
        };
        let previous_unsent = rt
            .staged_snapshot(&live.run_id)
            .and_then(|s| s.calls.get(&intent.call.position).cloned())
            .is_some_and(|c| {
                c.prior_intents
                    .last()
                    .and_then(|i| i.dispatch_count_window.as_ref())
                    .is_some_and(|d| d.count == 0)
                    || c.resolutions.last().is_some_and(|r| {
                        r.choice == "not_applied" && r.attempts.saturating_add(1) == intent.attempts
                    })
            });
        let mut maybe_sent =
            (intent.attempts > 1 && intent.call.class != StoredClass::Query && !previous_unsent)
                .then_some(basal_host::UnknownReason::BasalRestarted);
        let mut retries = 0;
        let answer = loop {
            if retries > 0
                && let Err(error) = rt.store().write(|tx| self.dispatch_check(tx, live))
            {
                if let Some(reason) = maybe_sent {
                    return Ok(Reply::Unknown(reason.as_str().into()));
                }
                return match error {
                    CoreError::Invalid(reason) => Ok(Reply::Revoked(reason)),
                    other => Err(other),
                };
            }
            match rt.shared.host.dispatch_classified(&request, class) {
                Ok(reply) => break Ok(reply),
                Err(TransportError::Refused(refusal)) => break Err(refusal),
                Err(TransportError::Unavailable { sent, detail }) => {
                    if let basal_host::Sent::Maybe(reason) = sent {
                        maybe_sent = Some(reason);
                    }
                    if (intent.call.class.safe_to_resend() || sent == basal_host::Sent::Never)
                        && retries < rt.config.unavailable_retries
                    {
                        retries += 1;
                        request.attempt = request.attempt.saturating_add(1);
                        std::thread::sleep(rt.config.retry_backoff);
                        continue;
                    }
                    if let Some(reason) =
                        maybe_sent.filter(|_| intent.call.class != StoredClass::Query)
                    {
                        return Ok(Reply::Unknown(
                            if intent.call.class.safe_to_resend() {
                                basal_host::UnknownReason::RetriesExhausted
                            } else {
                                reason
                            }
                            .as_str()
                            .into(),
                        ));
                    }
                    let outcome = crate::runtime::unavailable_rejection(&detail);
                    return Ok(Reply::Completed(arrival(
                        intent.call.position,
                        rt.cap_result(&outcome).unwrap_or(outcome),
                        intent.reservation.as_ref(),
                    )));
                }
                Err(TransportError::ScopeCut { detail }) => {
                    if intent.call.class == StoredClass::Query
                        && retries < rt.config.unavailable_retries
                    {
                        retries += 1;
                        request.attempt = request.attempt.saturating_add(1);
                        continue;
                    }
                    if intent.call.class != StoredClass::Query {
                        return Ok(Reply::Unknown("connection_lost".into()));
                    }
                    let outcome = crate::runtime::unavailable_rejection(&detail);
                    return Ok(Reply::Completed(arrival(
                        intent.call.position,
                        rt.cap_result(&outcome).unwrap_or(outcome),
                        intent.reservation.as_ref(),
                    )));
                }
            }
        };
        if answer.is_ok() {
            self.acknowledgements
                .lock()
                .unwrap()
                .insert((live.run_id.clone(), intent.call.position), request.clone());
        }
        if let Err(refusal) = &answer
            && maybe_sent.is_some()
            && intent.call.class != StoredClass::Query
        {
            if refusal.grant_absent() {
                rt.store().write(|tx| {
                    crate::grant_loss::record_staged(
                        tx,
                        &live.flow_id,
                        &live.run_id,
                        refusal,
                        self.now_ms(),
                    )
                })?;
            } else if refusal.reason == basal_host::flow_refusal::RefusalReason::AgentRetired {
                let target = rt.retirement_target_id(&request);
                rt.store().write(|tx| {
                    crate::grant_loss::record_retirement_staged(
                        tx,
                        &request,
                        refusal,
                        self.now_ms(),
                        target.as_deref(),
                    )?;
                    crate::install::disable(
                        tx,
                        &live.flow_id,
                        &crate::install::Actor::Core,
                        "agent_retired",
                        self.now_ms(),
                    )
                    .map_err(|e| CoreError::Invalid(e.to_string()))
                })?;
            }
            return Ok(Reply::Unknown(
                maybe_sent.expect("checked prior send").as_str().into(),
            ));
        }
        match answer {
            Ok(Dispatched::Accepted { handle }) => Ok(Reply::Accepted(handle)),
            Ok(Dispatched::Completed(outcome)) => Ok(Reply::Completed(arrival(
                intent.call.position,
                rt.cap_result(&outcome).unwrap_or(outcome),
                intent.reservation.as_ref(),
            ))),
            Err(refusal)
                if !refusal.terminal()
                    && !refusal.readiness()
                    && !refusal.grant_absent()
                    && refusal.reason != basal_host::flow_refusal::RefusalReason::AgentRetired =>
            {
                Ok(Reply::Deferred(crate::run_records::Deferred {
                    position: intent.call.position,
                    retry_not_before: self.retry_at(&rt, live, intent, &refusal),
                    cause: serde_json::to_string(&refusal)
                        .map_err(|e| CoreError::Invalid(e.to_string()))?,
                    audit_at: intent.audit_at,
                    proven_unsent: true,
                }))
            }
            Err(refusal) => {
                if refusal.readiness() {
                    rt.store()
                        .write(|tx| crate::flow_scope::health(tx, &live.flow_id, &refusal))?;
                }
                if refusal.grant_absent() {
                    rt.store().write(|tx| {
                        crate::grant_loss::record_staged(
                            tx,
                            &live.flow_id,
                            &live.run_id,
                            &refusal,
                            self.now_ms(),
                        )
                    })?;
                    return Ok(Reply::Deferred(crate::run_records::Deferred {
                        position: intent.call.position,
                        retry_not_before: i64::MAX,
                        cause: serde_json::to_string(&refusal)
                            .map_err(|e| CoreError::Invalid(e.to_string()))?,
                        audit_at: intent.audit_at,
                        proven_unsent: true,
                    }));
                }
                if refusal.reason == basal_host::flow_refusal::RefusalReason::AgentRetired {
                    let target = rt.retirement_target_id(&request);
                    rt.store().write(|tx| {
                        crate::grant_loss::record_retirement_staged(
                            tx,
                            &request,
                            &refusal,
                            self.now_ms(),
                            target.as_deref(),
                        )?;
                        crate::install::disable(
                            tx,
                            &live.flow_id,
                            &crate::install::Actor::Core,
                            "agent_retired",
                            self.now_ms(),
                        )
                        .map_err(|e| CoreError::Invalid(e.to_string()))
                    })?;
                }
                self.acknowledgements
                    .lock()
                    .unwrap()
                    .insert((live.run_id.clone(), intent.call.position), request);
                Ok(Reply::Refused(Box::new(refusal)))
            }
        }
    }
    fn dispatch_committed(&self, live: &LiveRun, position: u64, _: Option<&str>) {
        if let Ok(rt) = self.runtime() {
            let request = self
                .acknowledgements
                .lock()
                .unwrap()
                .remove(&(live.run_id.clone(), position));
            if let Some(request) = request {
                rt.shared.host.dispatch_committed(&request);
            }
            rt.shared.signal.bump();
        }
    }
    fn refusal_committed(&self, live: &LiveRun, position: u64) {
        if let Ok(rt) = self.runtime()
            && let Some(state) = rt.staged_snapshot(&live.run_id)
            && let Some(call) = state.calls.get(&position)
            && let Some(intent) = &call.intent
            && let Some(deferred) = &call.deferred
            && let Ok(refusal) = serde_json::from_str(&deferred.cause)
        {
            let saved = self
                .acknowledgements
                .lock()
                .unwrap()
                .remove(&(live.run_id.clone(), position));
            if let Some(request) = saved.or_else(|| request(live, intent).ok()) {
                rt.shared.host.refusal_committed(&request, &refusal);
            }
        }
    }
    fn dispatch_check(&self, tx: &rusqlite::Transaction<'_>, live: &LiveRun) -> Result<()> {
        let rt = self.runtime()?;
        if rt.is_stopping() {
            return Err(CoreError::Unavailable(
                "runtime stopped for recovery".into(),
            ));
        }
        if let Some(reason) = crate::install::revocation(tx, &live.flow_id, live.flow_version)? {
            return Err(CoreError::Invalid(reason));
        }
        Ok(())
    }
    fn now_ms(&self) -> i64 {
        self.config.clock.now_ms()
    }
    fn saturated(
        &self,
        tx: &rusqlite::Transaction<'_>,
        live: &LiveRun,
        reason: &str,
    ) -> Result<()> {
        if reason.starts_with("dispatch") {
            crate::rate::saturate_dispatch(
                tx,
                &live.flow_id,
                self.config.clock.now_ms(),
                &self.config.rate,
            )?;
        }
        Ok(())
    }
}

impl Runtime {
    pub(crate) fn flush_staged_live_refusals(&self) -> Result<()> {
        let snapshots = self.log_states();
        for (id, state) in snapshots {
            let Some(live) = self.store().read(|c| sql::live(c, &id))? else {
                continue;
            };
            for call in state.calls.values().filter(|c| c.arrival.is_some()) {
                if let (Some(intent), Some(deferred)) = (&call.intent, &call.deferred)
                    && let Ok(refusal) = serde_json::from_str::<basal_host::flow_refusal::FlowRefusal>(
                        &deferred.cause,
                    )
                {
                    self.shared
                        .host
                        .refusal_committed(&request(&live, intent)?, &refusal);
                }
            }
        }
        Ok(())
    }
    fn publish_pending_runs(&self) -> Result<()> {
        let hosts = self.shared.staged_hosts.as_ref().expect("staged runtime");
        let ids: Vec<String> = self.store().read(|c| {
            Ok(
                c.prepare("SELECT run_id FROM live_runs WHERE history_lost=0 ORDER BY admit_seq")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?,
            )
        })?;
        for id in ids {
            if hosts.registry().get(&id).is_none()
                && let Some(live) = self.store().read(|c| sql::live(c, &id))?
            {
                self.staged_driver(&id)?
                    .register(&live, Records::new(id, live.admitted_at, hosts.log.clone()))?;
            }
        }
        self.shared.signal.bump();
        Ok(())
    }

    pub(crate) fn staged_tick_at(
        &self,
        now: jiff::Timestamp,
        config: &crate::schedule::SchedulerConfig,
        limits: &crate::schedule::AdmitLimits,
    ) -> Result<crate::schedule::TickReport> {
        if self.is_stopping() {
            return Err(CoreError::Unavailable(
                "runtime stopped for recovery".into(),
            ));
        }
        let ctx = crate::admission::AdmitContext {
            now_ms: now.as_millisecond(),
            rate: limits.rate,
            default_deadline_ms: limits.default_deadline_ms,
        };
        let report = self.staged_admission().tick_at(now, config, &ctx)?;
        self.publish_pending_runs()?;
        Ok(report)
    }

    pub(crate) fn staged_admit_events(
        &self,
        notices: &[crate::events::Notice],
    ) -> Result<crate::events::Report> {
        if self.is_stopping() {
            return Err(CoreError::Unavailable(
                "runtime stopped for recovery".into(),
            ));
        }
        self.staged_admission().admit_events(
            notices,
            &self.admit_context()?,
            &EventWakes(self),
            || self.publish_pending_runs(),
        )
    }

    pub(crate) fn staged_drain_event_backlog(&self) -> Result<usize> {
        if self.is_stopping() {
            return Err(CoreError::Unavailable(
                "runtime stopped for recovery".into(),
            ));
        }
        let count = self
            .staged_admission()
            .drain_event_backlog(&self.admit_context()?)?;
        self.publish_pending_runs()?;
        Ok(count)
    }
    /// A disposable staged runtime owns its log beside its SQLite file. This is
    /// deliberately test-only: selecting it for real dry runs belongs to cutover.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_staged_scratch(
        store: Arc<crate::Store>,
        host: Arc<dyn basal_host::Host>,
        catalog: Arc<dyn basal_host::Catalog>,
        hooks: Arc<dyn crate::Hooks>,
        source: Option<Arc<dyn crate::channel::WorkerSource>>,
        config: Config,
        retain_detail: bool,
    ) -> Result<Self> {
        use crate::run_log::{Config as LogConfig, Identity, Lsn, RunLog, SqliteRegistry};
        let path: String = store.read(|c| {
            Ok(c.query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |r| r.get(0),
            )?)
        })?;
        let directory = std::path::Path::new(&path)
            .parent()
            .ok_or_else(|| CoreError::Invalid("scratch store needs a directory".into()))?
            .join("run-log");
        let epoch = sql::EpochLsn {
            log_epoch: 1,
            lsn: Lsn::default(),
        };
        store.write(|tx| {
            sql::create_test_schema(
                tx,
                &sql::Meta {
                    store_id: store.store_id().into(),
                    log_epoch: 1,
                    log_hwm: epoch,
                    commit_seq: 0,
                    gc_floor: sql::GcFloor {
                        log_epoch: 1,
                        first_retained: 0,
                        predecessor: None,
                        synced_lsn: Lsn::default(),
                        live_starts: Default::default(),
                    },
                    migration_phase: "test".into(),
                },
            )
        })?;
        let log = Arc::new(
            RunLog::<crate::run_records::Record>::create(
                LogConfig {
                    directory,
                    identity: Identity {
                        store_id: store.store_id().as_bytes().try_into().map_err(|_| {
                            CoreError::Corrupt("store identity must contain 16 bytes".into())
                        })?,
                        log_epoch: 1,
                    },
                    lease_epoch: store.lease_epoch(),
                    target_size: LogConfig::TARGET_SIZE,
                    hooks: hooks.clone(),
                },
                Arc::new(SqliteRegistry::new(store.clone(), 1)),
            )
            .map_err(|e| CoreError::Store(e.to_string()))?,
        );
        let budgets = run_driver::Budgets {
            dispatch_window_ms: i64::try_from(config.rate.window.as_millis())
                .map_err(|_| CoreError::Invalid("invalid rate window".into()))?,
            token_window_ms: 86_400_000,
            dispatches: u64::from(config.rate.max_dispatches),
            tokens: u64::MAX,
            cost_micros: u64::MAX,
            cost_window: None,
        };
        let hosts = Arc::new(crate::run_host::RecoveredHosts::recover(
            store.clone(),
            log.clone(),
            Arc::new(log.watermark()),
            &[],
            budgets,
            hooks.clone(),
            config.clock.clone(),
        )?);
        if retain_detail {
            hosts.registry().retain_detail_for_test(store)?;
        }
        Self::new_staged(hosts, host, catalog, hooks, source, config)
    }

    fn staged_admission(&self) -> RunAdmission {
        RunAdmission::new_staged(
            self.shared.store.clone(),
            self.shared
                .staged_hosts
                .as_ref()
                .expect("staged runtime")
                .watermark
                .clone(),
            self.shared.hooks.clone(),
        )
    }
    pub(crate) fn configure_staged_decisions(&self) -> Result<()> {
        let services = Arc::new(Services {
            shared: Arc::downgrade(&self.shared),
            config: self.config.clone(),
            acknowledgements: Default::default(),
            audit_actor: None,
        });
        self.shared
            .staged_hosts
            .as_ref()
            .expect("staged runtime")
            .configure_decisions(services.clone(), services)
    }
    pub(crate) fn staged_driver(&self, id: &str) -> Result<Arc<Driver>> {
        let hosts = self.shared.staged_hosts.as_ref().expect("staged runtime");
        let mut budgets = hosts.budgets_for(id)?;
        budgets.dispatches = u64::from(self.config.rate.max_dispatches);
        budgets.dispatch_window_ms = i64::try_from(self.config.rate.window.as_millis())
            .map_err(|_| CoreError::Invalid("invalid rate window".into()))?;
        Ok(Driver::new_staged(
            self.shared.store.clone(),
            hosts.registry().clone(),
            Arc::new(Services {
                shared: Arc::downgrade(&self.shared),
                config: self.config.clone(),
                acknowledgements: Default::default(),
                audit_actor: None,
            }),
            self.shared.hooks.clone(),
            budgets,
        ))
    }
    fn publish_admission(&self, admission: Admission) -> Result<Admission> {
        if let Admission::Admitted { run_id } = &admission {
            let hosts = self.shared.staged_hosts.as_ref().expect("staged runtime");
            let live = self
                .store()
                .read(|c| sql::live(c, run_id))?
                .ok_or_else(|| CoreError::NoSuchRun(run_id.clone()))?;
            self.staged_driver(run_id)?.register(
                &live,
                Records::new(run_id.clone(), live.admitted_at, hosts.log.clone()),
            )?;
        }
        self.shared.signal.bump();
        Ok(admission)
    }
    pub(crate) fn staged_admit(&self, spec: &TriggerSpec) -> Result<Admission> {
        let approved = self
            .store()
            .read(|c| crate::install::approved(c, &spec.flow_id))?;
        if approved
            .as_ref()
            .is_none_or(|a| a.script != spec.script || a.manifest != spec.manifest)
        {
            return Ok(Admission::NotApproved);
        }
        self.staged_admit_trigger(&spec.flow_id, &spec.trigger_id, spec.trigger.clone())
    }
    pub(crate) fn staged_admit_trigger(
        &self,
        flow: &str,
        trigger_id: &str,
        trigger: JsonText,
    ) -> Result<Admission> {
        if self.is_stopping() {
            return Err(CoreError::Unavailable(
                "runtime stopped for recovery".into(),
            ));
        }
        self.publish_admission(self.staged_admission().admit_trigger(
            flow,
            trigger_id,
            trigger,
            &self.admit_context()?,
        )?)
    }
    pub(crate) fn staged_retrigger(&self, id: &str) -> Result<Admission> {
        self.publish_admission(
            self.staged_admission()
                .retrigger(id, &self.admit_context()?)?,
        )
    }
    pub(crate) fn staged_snapshot(&self, id: &str) -> Option<State> {
        self.shared
            .staged_hosts
            .as_ref()?
            .registry()
            .get(id)
            .map(|r| r.reader_snapshot().3)
    }
    pub(crate) fn staged_run(&self, id: &str) -> Result<Run> {
        let state = self.staged_snapshot(id);
        let owner = self.shared.staged_owners.lock().unwrap().get(id).cloned();
        let active = owner.as_ref().is_some_and(|(generation, _)| {
            state.as_ref().is_some_and(|s| *generation == s.generation)
        });
        self.store().read(|c| {
            crate::runs::load_staged(
                c,
                id,
                state.as_ref(),
                active,
                owner.as_ref().map_or("", |(_, owner)| owner),
            )
        })
    }
    pub(crate) fn staged_calls(&self, id: &str) -> Result<Vec<crate::model::CallRow>> {
        let state = self.staged_snapshot(id);
        self.store()
            .read(|c| crate::run_readers::calls(c, id, state.as_ref()))
    }
    pub(crate) fn staged_claim(
        &self,
        id: &str,
    ) -> Result<std::result::Result<Lease, ActivationEnd>> {
        if self.is_stopping() {
            return Ok(Err(ActivationEnd::OwnerLost));
        }
        if let Some(retry_in) = self.gate_wait(id) {
            return Ok(Err(ActivationEnd::Deferred {
                detail: "waiting for core install status".into(),
                retry_in,
            }));
        }
        let run = self.staged_run(id)?;
        if run.state != RunState::Pending {
            return Ok(Err(ActivationEnd::NotRunnable { state: run.state }));
        }
        if run
            .deadline_at
            .is_some_and(|at| at <= self.config.clock.now_ms())
        {
            self.staged_finish(
                id,
                RunState::Failed,
                Some("deadline"),
                Some("the run passed its wall-clock deadline"),
            )?;
            return Ok(Err(ActivationEnd::NotRunnable {
                state: RunState::Failed,
            }));
        }
        let (live, holder) = self.store().read(|c| {
            let Some(live)=sql::live(c,id)? else {return Ok((None,None))};
            let holder = c.query_row("SELECT run_id FROM live_runs WHERE flow_id=?1 AND admit_seq<?2 ORDER BY admit_seq LIMIT 1", params![live.flow_id,live.admit_seq], |r| r.get::<_,String>(0)).optional()?;
            Ok((Some(live), holder))
        })?;
        let Some(live) = live else {
            return Ok(Err(ActivationEnd::NotRunnable {
                state: self.run(id)?.state,
            }));
        };
        if let Some(holder) = holder {
            return Ok(Err(ActivationEnd::Waiting { holder }));
        }
        {
            let mut owners = self.shared.staged_owners.lock().unwrap();
            if owners.contains_key(id) {
                return Ok(Err(ActivationEnd::NotRunnable {
                    state: RunState::Running,
                }));
            }
            owners.insert(id.into(), (u64::MAX, self.config.owner.clone()));
        }
        let result = self.staged_driver(id)?.claim(live.clone(), run.generation);
        match result {
            Ok(run_driver::Claim::Active(activation)) => {
                self.shared.staged_owners.lock().unwrap().insert(
                    id.into(),
                    (activation.generation, self.config.owner.clone()),
                );
                self.shared.signal.bump();
                Ok(Ok(Lease {
                    run_id: id.into(),
                    owner: self.config.owner.clone(),
                    generation: activation.generation,
                }))
            }
            other => {
                self.shared.staged_owners.lock().unwrap().remove(id);
                match other {
                    Ok(run_driver::Claim::Deferred) => Ok(Err(ActivationEnd::Deferred {
                        detail: "waiting for core install status".into(),
                        retry_in: self.gate_wait(id).unwrap_or(self.config.install_gate_retry),
                    })),
                    Ok(run_driver::Claim::Revoked) => {
                        self.staged_driver(id)?.cancel(Terminal::new(
                            live.clone(),
                            RunState::Cancelled,
                            self.config.clock.now_ms(),
                            self.shared.staged_hosts.as_ref().unwrap().budgets_for(id)?,
                            Arc::new(Services {
                                shared: Arc::downgrade(&self.shared),
                                config: self.config.clone(),
                                acknowledgements: Default::default(),
                                audit_actor: None,
                            }),
                        ))?;
                        Ok(Err(ActivationEnd::Revoked {
                            version: live.flow_version,
                            cause: crate::gate::RevokeCause::AlreadyRevoked,
                            detail: "install no longer approved".into(),
                        }))
                    }
                    Err(CoreError::OwnerLost { .. }) => Ok(Err(ActivationEnd::OwnerLost)),
                    Err(CoreError::NoSuchRun(_)) => Ok(Err(ActivationEnd::NotRunnable {
                        state: self.run(id)?.state,
                    })),
                    Err(e) => Err(e),
                    Ok(run_driver::Claim::Active(_)) => unreachable!(),
                }
            }
        }
    }
    pub(crate) fn staged_drive(
        &self,
        lease: Lease,
        worker: &mut dyn WorkerChannel,
    ) -> ActivationEnd {
        let result = (|| {
            let live = self
                .store()
                .read(|c| sql::live(c, &lease.run_id))?
                .ok_or_else(|| lease.lost())?;
            self.shared.staged_workers.lock().unwrap().insert(
                (lease.run_id.clone(), lease.generation),
                std::thread::current().id(),
            );
            let activation = self
                .staged_driver(&lease.run_id)?
                .owned(live.clone(), lease.generation)?;
            let run = self.staged_run(&lease.run_id)?;
            let manifest = Manifest::parse_stored(&run.manifest)
                .map_err(|e| CoreError::Invalid(e.to_string()))?;
            let trigger = match self.event_preamble(&run, &manifest)? {
                crate::events::Preamble::Ready(trigger) => trigger,
                crate::events::Preamble::Retry => {
                    activation.requeue(false)?;
                    return Ok(ActivationEnd::Requeued);
                }
                crate::events::Preamble::Failed { kind, detail } => {
                    let mut terminal = Terminal::new(
                        live,
                        RunState::Failed,
                        self.config.clock.now_ms(),
                        self.shared
                            .staged_hosts
                            .as_ref()
                            .unwrap()
                            .budgets_for(&lease.run_id)?,
                        Arc::new(Services {
                            shared: Arc::downgrade(&self.shared),
                            config: self.config.clone(),
                            acknowledgements: Default::default(),
                            audit_actor: None,
                        }),
                    );
                    terminal.error_kind = Some(kind.clone());
                    terminal.error_detail = Some(detail.clone());
                    activation.finish(terminal)?;
                    return Ok(ActivationEnd::Failed { kind, detail });
                }
            };
            let request = ActivationRequest {
                activation_id: lease.generation,
                profile: Profile::Flow,
                tools: vec![],
                prelude_hash: worker.welcome().prelude_hash,
                script: run.script,
                trigger,
                self_input: run.self_input,
                params_input: run.params_input,
                budgets: self.config.budgets,
                prefix: vec![],
            };
            activation.drive(
                worker,
                request,
                Arc::new(Services {
                    shared: Arc::downgrade(&self.shared),
                    config: self.config.clone(),
                    acknowledgements: Default::default(),
                    audit_actor: None,
                }),
                self.config.activation_deadline,
            )
        })();
        let result = match result {
            Err(error)
                if !matches!(
                    error,
                    CoreError::Store(_)
                        | CoreError::Cut
                        | CoreError::OwnerLost { .. }
                        | CoreError::Unavailable(_)
                ) =>
            {
                worker.kill();
                let detail = error.to_string();
                self.staged_finish(&lease.run_id, RunState::Failed, Some("core"), Some(&detail))
                    .map(|_| ActivationEnd::Failed {
                        kind: "core".into(),
                        detail,
                    })
            }
            other => other,
        };
        let result = if matches!(result, Ok(ActivationEnd::Requeued))
            && self
                .staged_snapshot(&lease.run_id)
                .is_some_and(|s| s.broken >= u64::from(self.config.max_broken_activations))
        {
            self.staged_finish(
                &lease.run_id,
                RunState::Failed,
                Some("worker"),
                Some("the worker channel ended during activation"),
            )
            .map(|_| ActivationEnd::Failed {
                kind: "worker".into(),
                detail: "the worker channel ended during activation".into(),
            })
        } else {
            result
        };
        self.shared
            .staged_workers
            .lock()
            .unwrap()
            .remove(&(lease.run_id.clone(), lease.generation));
        self.shared.staged_stopped.notify_all();
        let mut owners = self.shared.staged_owners.lock().unwrap();
        if owners
            .get(&lease.run_id)
            .is_some_and(|(g, _)| *g == lease.generation)
        {
            owners.remove(&lease.run_id);
        }
        drop(owners);
        self.shared.signal.bump();
        let result = self.flush_refusals().and(result);
        match result {
            Ok(end) => end,
            Err(CoreError::OwnerLost { .. }) => {
                worker.kill();
                ActivationEnd::OwnerLost
            }
            Err(CoreError::Cut) => {
                worker.kill();
                ActivationEnd::Crashed
            }
            Err(e) => {
                worker.kill();
                ActivationEnd::Failed {
                    kind: "store".into(),
                    detail: e.to_string(),
                }
            }
        }
    }

    pub(crate) fn staged_recover(&self) -> Result<Vec<String>> {
        let hosts = self.shared.staged_hosts.as_ref().expect("staged runtime");
        let mut ids = Vec::new();
        for run in hosts.registry().snapshot() {
            if run.routing() != crate::run_store::Routing::Open {
                continue;
            }
            let (id, _, _, state) = run.reader_snapshot();
            if !state.parked && matches!(state.phase.as_str(), "claimed" | "script") {
                run.enter()
                    .map_err(|_| CoreError::Unavailable("run is closing".into()))?
                    .finish(|records| {
                        records
                            .append(
                                state.generation,
                                crate::run_records::Body::Requeued {
                                    broken: state.broken,
                                },
                            )
                            .map(|_| ())
                    })?;
                ids.push(id);
            }
        }
        self.shared.signal.bump();
        self.flush_staged_live_refusals()?;
        self.flush_refusals()?;
        Ok(ids)
    }

    pub(crate) fn staged_take_over(&self, id: &str, generation: u64) -> Result<Lease> {
        let run = self.run(id)?;
        if run.state != RunState::Running {
            return Err(CoreError::WrongState {
                run_id: id.into(),
                state: run.state,
                operation: "take_over",
            });
        }
        if run.generation != generation {
            return Err(CoreError::OwnerLost {
                run_id: id.into(),
                generation,
            });
        }
        let live = self
            .store()
            .read(|c| sql::live(c, id))?
            .ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
        match self.staged_driver(id)?.claim(live, generation)? {
            run_driver::Claim::Active(a) => {
                self.shared
                    .staged_owners
                    .lock()
                    .unwrap()
                    .insert(id.into(), (a.generation, self.config.owner.clone()));
                Ok(Lease {
                    run_id: id.into(),
                    owner: self.config.owner.clone(),
                    generation: a.generation,
                })
            }
            _ => Err(CoreError::Unavailable(
                "install gate did not authorize takeover".into(),
            )),
        }
    }

    pub(crate) fn staged_finish(
        &self,
        id: &str,
        state: RunState,
        kind: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        self.staged_finish_with_audit(id, state, kind, detail, None)
    }

    pub(crate) fn staged_cancel(&self, id: &str, actor: &str) -> Result<()> {
        let run = self.run(id)?;
        if run.state.is_terminal() {
            return Err(CoreError::WrongState {
                run_id: id.into(),
                state: run.state,
                operation: "cancel",
            });
        }
        self.staged_finish_with_audit(
            id,
            RunState::Cancelled,
            Some("cancelled"),
            Some(&format!("cancelled by {actor}")),
            Some(actor),
        )
    }

    fn staged_finish_with_audit(
        &self,
        id: &str,
        state: RunState,
        kind: Option<&str>,
        detail: Option<&str>,
        audit_actor: Option<&str>,
    ) -> Result<()> {
        let hosts = self.shared.staged_hosts.as_ref().expect("staged runtime");
        let live = self
            .store()
            .read(|c| sql::live(c, id))?
            .ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
        let actor = hosts
            .registry()
            .get(id)
            .ok_or_else(|| CoreError::NoSuchRun(id.into()))?;
        let services = Arc::new(Services {
            shared: Arc::downgrade(&self.shared),
            config: self.config.clone(),
            acknowledgements: Default::default(),
            audit_actor: audit_actor.map(str::to_owned),
        });
        let mut terminal = Terminal::new(
            live,
            state,
            self.config.clock.now_ms(),
            hosts.budgets_for(id)?,
            services.clone(),
        );
        terminal.error_kind = kind.map(str::to_owned);
        terminal.error_detail = detail.map(str::to_owned);
        hosts
            .registry()
            .close(&actor, &terminal, services.as_ref())?;
        actor.closed_hint(terminal.commit_seq());
        self.shared.signal.bump();
        self.flush_refusals()
    }

    pub(crate) fn staged_startable(&self) -> Result<Vec<(String, String)>> {
        let hosts = self.shared.staged_hosts.as_ref().expect("staged runtime");
        let ids: Vec<(String,String,bool)> = self.store().read(|c| {
            Ok(c.prepare("SELECT r.run_id,r.flow_id,f.state='enabled' FROM live_runs r JOIN flows f USING(flow_id) WHERE r.history_lost=0 AND NOT EXISTS(SELECT 1 FROM live_runs o WHERE o.flow_id=r.flow_id AND o.admit_seq<r.admit_seq) ORDER BY r.admit_seq,r.run_id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
        })?;
        let now = self.config.clock.now_ms();
        let mut ready = Vec::new();
        for (id, flow, enabled) in ids {
            let Some(actor) = hosts
                .registry()
                .get(&id)
                .filter(|r| r.routing() == crate::run_store::Routing::Open)
            else {
                continue;
            };
            if enabled && let Ok(entrant) = actor.enter() {
                entrant.finish(|records| {
                    let deferrals: Vec<_> = records
                        .state()
                        .calls
                        .values()
                        .filter(|c| c.arrival.is_none())
                        .filter_map(|c| c.deferred.as_ref())
                        .filter(|d| {
                            d.retry_not_before == i64::MAX
                                && serde_json::from_str::<basal_host::flow_refusal::FlowRefusal>(
                                    &d.cause,
                                )
                                .is_ok_and(|r| r.grant_absent())
                        })
                        .cloned()
                        .collect();
                    for mut deferred in deferrals {
                        deferred.retry_not_before = now;
                        records.append(
                            records.state().generation,
                            crate::run_records::Body::Deferred(deferred),
                        )?;
                    }
                    Ok(())
                })?;
            }
            self.staged_driver(&id)?.wake(&id)?;
            let Some(state) = hosts.registry().get(&id).map(|r| r.reader_snapshot().3) else {
                continue;
            };
            let run = self.run(&id)?;
            if run.state == RunState::Pending && self.gate_wait(&id).is_none() {
                let waiting = state.event_bodies.last().is_some_and(|body| matches!(body, crate::run_records::Body::EventBody { body: None, retry_at: Some(at), .. } if *at > now));
                if !waiting && actor.routing() == crate::run_store::Routing::Open {
                    ready.push((id, flow));
                }
            }
        }
        Ok(ready)
    }
}
