//! The `flow.*` ops and who may call each.
//!
//! | Op | Operator | Owning agent | Another agent | Core |
//! |---|---|---|---|---|
//! | `flow.install` | yes, for any author, with the loop override | for itself only, no override | no (a flow another agent wrote is not theirs to replace) | no |
//! | `flow.dry_run` capture | yes | yes | no | no |
//! | `flow.dry_run` live | yes | no | no | no |
//! | `flow.health` | yes | no | no | yes |
//! | `flow.reconcile` | yes | no | no | no |
//! | `flow.drain` | yes | no | no | no |
//! | `flow.disable` | yes | yes | no | no |
//! | `flow.enable` | yes | only to undo a disable it made itself | no | no |
//!
//! The owning agent is the author of the flow's approved version (before any
//! version is approved, the author of its installed versions). An owner
//! cannot enable a flow the operator disabled, or one the runtime disabled
//! for sustained saturation: that would let a flow's author undo the
//! operator's stop or the flow's loop protection. The caller
//! comes from the route the daemon stamped ([`crate::caller`]), never from a
//! request field.

use std::time::Duration;

use basal_core::cards::CardState;
use basal_core::manifest::{Manifest, duration_ms};
use basal_core::{Actor, CoreError, InstallError, InstallRequest, Resolution};
use basal_host::{ConsentError, HostOutcome, InstallCard};
use basal_proto::{JsonText, Settlement};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::caller::Caller;
use crate::card::{self, CardInput, code_hash_hex};
use crate::dryrun::{DryRunError, DryRunRequest, DryTrigger, Mode};
use crate::fatal::{install_is_storage, is_storage};
use crate::module::Module;

/// A refusal or failure, as the op's error frame carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    pub code: String,
    pub message: String,
}

impl OpError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    fn not_permitted(op: &str, caller: &Caller) -> Self {
        Self::new(
            "not_permitted",
            format!("{} may not call {op}", caller.label()),
        )
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub type OpResult = Result<Value, OpError>;

fn invalid_params(e: impl std::fmt::Display) -> OpError {
    OpError::new("invalid_params", e.to_string())
}

impl Module {
    /// A core error as an op error. A storage failure also raises the fatal
    /// latch, which ends the process.
    fn core_error(&self, e: CoreError) -> OpError {
        if is_storage(&e) {
            self.fatal.raise(e.to_string());
            return OpError::new("store_failure", e.to_string());
        }
        match e {
            CoreError::NoSuchRun(r) => OpError::new("not_found", format!("no run {r}")),
            CoreError::WrongState { .. } => OpError::new("wrong_state", e.to_string()),
            CoreError::Invalid(m) => OpError::new("invalid_params", m),
            other => OpError::new("internal", other.to_string()),
        }
    }

    fn install_error(&self, e: InstallError) -> OpError {
        if install_is_storage(&e) {
            let text = e.to_string();
            self.fatal.raise(text.clone());
            return OpError::new("store_failure", text);
        }
        match e {
            InstallError::NotOwner { .. } | InstallError::NotYourDisable { .. } => {
                OpError::new("not_permitted", e.to_string())
            }
            InstallError::NoSuchFlow(f) => OpError::new("not_found", format!("no flow {f}")),
            InstallError::Store(inner) => self.core_error(inner),
            other => OpError::new("install_refused", format!("{other:?}")),
        }
    }

    /// Runs one op for `caller`.
    pub fn handle(&self, caller: &Caller, method: &str, params: Value) -> OpResult {
        let result = match method {
            "flow.install" => self.op_install(caller, params),
            "flow.dry_run" => self.op_dry_run(caller, params),
            "flow.health" => self.op_health(caller, params),
            "flow.reconcile" => self.op_reconcile(caller, params),
            "flow.drain" => self.op_drain(caller, params),
            "flow.disable" => self.op_disable(caller, params),
            "flow.enable" => self.op_enable(caller, params),
            other => Err(OpError::new(
                "unknown_method",
                format!("basal serves no op {other:?}; see its management manifest"),
            )),
        };
        if let Err(e) = &result {
            tracing::info!(target: "ops", caller = %caller.label(), method, "refused: {e}");
        }
        result
    }

    /// The flow's owner, or before any approval the authors of its
    /// installed versions. Empty for a flow basal does not know.
    fn owners(&self, flow_id: &str) -> Result<Vec<String>, OpError> {
        let record = self.rt.flow(flow_id).map_err(|e| self.core_error(e))?;
        if let Some(owner) = record.and_then(|r| r.owner) {
            return Ok(vec![owner]);
        }
        self.rt
            .store()
            .read(|c| {
                let mut stmt = c.prepare(
                    "SELECT DISTINCT author FROM installs WHERE flow_id = ?1 ORDER BY author",
                )?;
                let authors = stmt
                    .query_map([flow_id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(authors)
            })
            .map_err(|e| self.core_error(e))
    }

    fn owns(&self, agent: &str, flow_id: &str) -> Result<bool, OpError> {
        let owners = self.owners(flow_id)?;
        Ok(!owners.is_empty() && owners.iter().all(|o| o == agent))
    }

    // ---- flow.install --------------------------------------------------

    fn op_install(&self, caller: &Caller, params: Value) -> OpResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            script: String,
            /// The manifest's exact text: the code hash covers its bytes.
            manifest: String,
            #[serde(default)]
            author: Option<String>,
            #[serde(default)]
            loop_override: bool,
        }
        let p: Params = serde_json::from_value(params).map_err(invalid_params)?;
        let manifest = Manifest::parse(&p.manifest).map_err(|e| {
            OpError::new("install_refused", format!("the manifest is invalid: {e}"))
        })?;
        // Authorization: an agent installs for itself, never with the
        // operator's loop override, and only a flow no other agent wrote.
        let author = match caller {
            Caller::Operator => p.author.unwrap_or_else(|| "operator".to_owned()),
            Caller::Agent(agent) => {
                if p.author.as_ref().is_some_and(|a| a != agent) {
                    return Err(OpError::new(
                        "not_permitted",
                        "an agent installs flows for itself only",
                    ));
                }
                if p.loop_override {
                    return Err(OpError::new(
                        "not_permitted",
                        "only the operator may override the loop install rule",
                    ));
                }
                let owners = self.owners(&manifest.id)?;
                if owners.iter().any(|o| o != agent) {
                    return Err(OpError::new(
                        "not_permitted",
                        format!("flow {} belongs to someone else", manifest.id),
                    ));
                }
                agent.clone()
            }
            Caller::Core | Caller::Other(_) => {
                return Err(OpError::not_permitted("flow.install", caller));
            }
        };

        let installed = self
            .rt
            .install(&InstallRequest {
                script: p.script.clone(),
                manifest: p.manifest.clone(),
                author: author.clone(),
                loop_override: p.loop_override,
            })
            .map_err(|e| self.install_error(e))?;

        let existing = self
            .rt
            .cards(&installed.flow_id)
            .map_err(|e| self.core_error(e))?
            .into_iter()
            .find(|c| c.version == installed.version);
        let record = match existing {
            Some(card) => card,
            None => {
                let now = self.rt.config().clock.now_ms();
                let dry_run = match self.dry.run(&DryRunRequest {
                    script: p.script.clone(),
                    manifest: p.manifest.clone(),
                    author: author.clone(),
                    loop_override: p.loop_override,
                    mode: Mode::Capture,
                    trigger: DryTrigger::Schedule { window: None },
                    now_ms: now,
                }) {
                    Ok(summary) => summary,
                    // The card says the dry run failed rather than holding
                    // the install back: the operator decides with that known.
                    Err(e) => json!({ "error": e.to_string() }),
                };
                let token_window = self
                    .rt
                    .token_usage(&installed.flow_id, now)
                    .map_err(|e| self.core_error(e))?
                    .map(|w| {
                        json!({
                            "window_ms": w.window_ms,
                            "window_start": w.window_start,
                            "reserved": w.reserved,
                            "input_tokens": w.input_tokens,
                            "cache_write_tokens": w.cache_write_tokens,
                            "output_tokens": w.output_tokens,
                            "cached_input_tokens": w.cached_input_tokens,
                            "unreported_tokens": w.unreported_tokens,
                        })
                    });
                let fields = card::fields(&CardInput {
                    manifest: &manifest,
                    script: &p.script,
                    manifest_text: &p.manifest,
                    author: &author,
                    installed: &installed,
                    catalog: self.catalog.as_ref(),
                    token_window,
                    dry_run,
                });
                self.rt
                    .record_card(
                        &installed.flow_id,
                        installed.version,
                        &installed.code_hash,
                        &author,
                        &fields.to_string(),
                    )
                    .map_err(|e| self.core_error(e))?
            }
        };
        let fields: Value = serde_json::from_str(&record.card).unwrap_or(Value::Null);
        if record.state == CardState::Pending {
            // Raised every time the version is installed while undecided: a
            // raise that was lost (the consent plane was down) is retried by
            // installing again, and the card id makes it one card.
            let raised = self.consent.raise(&InstallCard {
                card_id: record.card_id.clone(),
                flow_id: record.flow_id.clone(),
                version: record.version,
                fields: fields.clone(),
            });
            if let Err(e) = raised {
                let code = match e {
                    ConsentError::Unavailable(_) => "consent_unavailable",
                    ConsentError::Refused(_) => "consent_refused",
                };
                return Err(OpError::new(
                    code,
                    format!(
                        "version {} of {} is installed and its card {} recorded, but the card could not be raised: {e}; install again to retry",
                        record.version, record.flow_id, record.card_id
                    ),
                ));
            }
        }
        Ok(json!({
            "flow_id": record.flow_id,
            "version": record.version,
            "code_hash": code_hash_hex(&record.code_hash),
            "card_id": record.card_id,
            "state": record.state.as_str(),
            "new": installed.new,
            "warnings": fields.get("warnings").cloned().unwrap_or(Value::Null),
            "dry_run": fields.get("dry_run_summary").cloned().unwrap_or(Value::Null),
        }))
    }

    // ---- flow.dry_run --------------------------------------------------

    fn op_dry_run(&self, caller: &Caller, params: Value) -> OpResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            flow_id: String,
            #[serde(default)]
            version: Option<u32>,
            #[serde(default)]
            mode: Option<String>,
            /// A synthetic trigger payload; without it a schedule flow
            /// replays its window.
            #[serde(default)]
            trigger: Option<Value>,
            /// The window to replay, as a duration (`"6h"`, `"2d"`).
            #[serde(default)]
            window: Option<String>,
        }
        let p: Params = serde_json::from_value(params).map_err(invalid_params)?;
        let mode = match p.mode.as_deref() {
            None | Some("capture") => Mode::Capture,
            Some("live") => Mode::Live,
            Some(other) => return Err(invalid_params(format!("unknown mode {other:?}"))),
        };
        // Authorization: capture for the operator and the flow's owner; live
        // for the operator only, because it runs ops with basal's authority.
        match (caller, mode) {
            (Caller::Operator, _) => {}
            (Caller::Agent(agent), Mode::Capture) if self.owns(agent, &p.flow_id)? => {}
            _ => {
                return Err(OpError::not_permitted(
                    &format!("flow.dry_run in {} mode", mode.as_str()),
                    caller,
                ));
            }
        }
        let version = self.pick_version(&p.flow_id, p.version)?;
        let trigger = match p.trigger {
            Some(payload) => DryTrigger::Synthetic(payload),
            None => DryTrigger::Schedule {
                window: p
                    .window
                    .as_deref()
                    .map(|w| {
                        duration_ms(w)
                            .and_then(|ms| u64::try_from(ms).ok())
                            .map(Duration::from_millis)
                            .ok_or_else(|| invalid_params(format!("bad window {w:?}")))
                    })
                    .transpose()?,
            },
        };
        let summary = self
            .dry
            .run(&DryRunRequest {
                script: version.script,
                manifest: version.manifest,
                author: version.author,
                loop_override: version.loop_override,
                mode,
                trigger,
                now_ms: self.rt.config().clock.now_ms(),
            })
            .map_err(|e| match e {
                DryRunError::Invalid(m) => invalid_params(m),
                DryRunError::Failed(m) => OpError::new("dry_run_failed", m),
            })?;
        Ok(json!({ "flow_id": p.flow_id, "version": version.version, "summary": summary }))
    }

    /// The version a dry run runs: the one named, else the approved one,
    /// else the newest installed.
    fn pick_version(
        &self,
        flow_id: &str,
        version: Option<u32>,
    ) -> Result<InstalledVersion, OpError> {
        let found = self
            .rt
            .store()
            .read(|c| {
                let sql = "SELECT version, script, manifest, author, loop_override FROM installs \
                           WHERE flow_id = ?1 AND (?2 IS NULL OR version = ?2) \
                           ORDER BY (state = 'approved') DESC, version DESC LIMIT 1";
                Ok(c.query_row(
                    sql,
                    rusqlite::params![flow_id, version.map(i64::from)],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    },
                )
                .optional()?)
            })
            .map_err(|e| self.core_error(e))?;
        let Some((v, script, manifest, author, loop_override)) = found else {
            return Err(OpError::new(
                "not_found",
                format!("no installed version of {flow_id}"),
            ));
        };
        Ok(InstalledVersion {
            version: u32::try_from(v).map_err(|_| OpError::new("internal", "bad version"))?,
            script,
            manifest,
            author,
            loop_override: loop_override != 0,
        })
    }

    // ---- flow.health ---------------------------------------------------

    /// `flow.health`. prefrontal-core polls it on its own route to decide
    /// whether a flow's claim on a source still holds, and treats any reply
    /// it cannot decode as unhealthy. So the fields of that contract are
    /// always present with fixed types: `as_of` (RFC 3339, UTC) and, per
    /// flow, `flow_id`, `state` (`enabled`, `disabled`, or `shadow` once
    /// shadow mode exists), `last_run` (`{outcome, at}` or null),
    /// `oldest_overdue_age_ms` (0 when nothing is overdue),
    /// `consecutive_failures`, and the booleans `needs_reconcile`,
    /// `overflowed` and `auto_disabled`. Everything else in the reply is for
    /// the operator, and core ignores it.
    fn op_health(&self, caller: &Caller, params: Value) -> OpResult {
        if !matches!(caller, Caller::Operator | Caller::Core) {
            return Err(OpError::not_permitted("flow.health", caller));
        }
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct Params {
            /// The flows to report; absent means every flow. A flow basal
            /// does not know is left out.
            #[serde(default)]
            flow_ids: Option<Vec<String>>,
        }
        let p: Params = if params.is_null() {
            Params::default()
        } else {
            serde_json::from_value(params).map_err(invalid_params)?
        };
        let now = self.rt.config().clock.now_ms();
        let as_of = rfc3339(now)?;
        let wanted = |flow_id: &str| {
            p.flow_ids
                .as_ref()
                .is_none_or(|ids| ids.iter().any(|id| id == flow_id))
        };
        let flows: Vec<_> = self
            .rt
            .flow_health()
            .map_err(|e| self.core_error(e))?
            .into_iter()
            .filter(|f| wanted(&f.flow_id))
            .collect();
        let ages = self.rt.run_ages().map_err(|e| self.core_error(e))?;
        let health = self.rt.health().map_err(|e| self.core_error(e))?;
        let mut tokens = Vec::new();
        for f in &flows {
            if let Some(w) = self
                .rt
                .token_usage(&f.flow_id, now)
                .map_err(|e| self.core_error(e))?
            {
                tokens.push(json!({
                    "flow_id": f.flow_id,
                    "window_start": w.window_start,
                    "window_ms": w.window_ms,
                    // Outstanding reservations, against what was reported.
                    "reserved": w.reserved,
                    "actual": w.input_tokens + w.cache_write_tokens + w.output_tokens + w.unreported_tokens,
                    "cached_input_tokens": w.cached_input_tokens,
                }));
            }
        }
        let mut entries = Vec::with_capacity(flows.len());
        for f in flows {
            let last_run = match f.last_run {
                Some(r) => json!({
                    "outcome": r.state,
                    "at": rfc3339(r.ended_at)?,
                    "run_id": r.run_id,
                    "error_kind": r.error_kind,
                }),
                None => Value::Null,
            };
            let state = if f.enabled { "enabled" } else { "disabled" };
            entries.push(json!({
                "flow_id": f.flow_id,
                "state": state,
                "last_run": last_run,
                "oldest_overdue_age_ms": f.oldest_overdue_ms.unwrap_or(0),
                "consecutive_failures": f.consecutive_failures,
                "needs_reconcile": !f.needs_reconcile.is_empty(),
                // The event plane does not exist yet, so no backlog can
                // overflow.
                "overflowed": false,
                "auto_disabled": f.auto_disabled,
                "needs_reconcile_runs": f.needs_reconcile,
                "owner": f.owner,
                "approved_version": f.approved_version,
                "disabled_by": f.disabled_by,
                // Who disabled the flow, by kind (operator, the owning
                // agent, or auto for the runtime's own saturation
                // disable), and why: core can tell a flow stopped by its
                // author from one stopped for it.
                "disabled": f.disabled_by.as_deref().map(|by| json!({
                    "by": disabled_kind(by),
                    "actor": by,
                    "reason": f.disabled_reason,
                })),
            }));
        }
        let pool = self.pool.stats();
        Ok(json!({
            "as_of": as_of,
            "flows": entries,
            "runs": {
                "by_state": health.runs,
                "oldest_pending_ms": ages.oldest_pending_ms,
                "oldest_suspended_ms": ages.oldest_suspended_ms,
                "oldest_needs_reconcile_ms": ages.oldest_needs_reconcile_ms,
                "unknown_calls": health.unknown_calls.len(),
                "open_obligations": health.open_obligations.len(),
                "quarantined": health.quarantined,
            },
            "draining": health.draining,
            "pool": {
                "live": pool.live,
                "spares": pool.spares,
                "spawning": pool.spawning,
                "busy": pool.busy,
                "bound": pool.bound.iter().map(|(f, n)| json!({"flow_id": f, "idle": n})).collect::<Vec<_>>(),
            },
            "metrics": self.metrics.to_json(),
            "tokens": tokens,
            "active_activations": self.engine.active_count(),
        }))
    }

    // ---- flow.reconcile ------------------------------------------------

    fn op_reconcile(&self, caller: &Caller, params: Value) -> OpResult {
        if *caller != Caller::Operator {
            return Err(OpError::not_permitted("flow.reconcile", caller));
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            run_id: String,
            position: u64,
            resolution: Value,
        }
        let p: Params = serde_json::from_value(params).map_err(invalid_params)?;
        let resolution = match &p.resolution {
            Value::String(s) if s == "not_applied" => Resolution::NotApplied,
            Value::String(s) if s == "cancel" => Resolution::Cancel,
            Value::Object(o) if o.len() == 1 && o.contains_key("observed_result") => {
                let observed = &o["observed_result"];
                let settlement = match observed.get("settlement").and_then(Value::as_str) {
                    Some("fulfilled") => Settlement::Fulfilled,
                    Some("rejected") => Settlement::Rejected,
                    _ => {
                        return Err(invalid_params(
                            "observed_result needs settlement fulfilled or rejected",
                        ));
                    }
                };
                let value = observed.get("value").cloned().unwrap_or(Value::Null);
                let text = JsonText::new(value.to_string()).map_err(invalid_params)?;
                Resolution::ObservedResult(HostOutcome {
                    usage: None,
                    settlement,
                    value: text,
                })
            }
            _ => {
                return Err(invalid_params(
                    "resolution is \"not_applied\", \"cancel\" or {\"observed_result\": {settlement, value}}",
                ));
            }
        };
        let state = self
            .rt
            .reconcile(&p.run_id, p.position, resolution, &caller.label())
            .map_err(|e| self.core_error(e))?;
        Ok(json!({ "run_id": p.run_id, "state": state.as_str() }))
    }

    // ---- flow.drain ----------------------------------------------------

    fn op_drain(&self, caller: &Caller, params: Value) -> OpResult {
        if *caller != Caller::Operator {
            return Err(OpError::not_permitted("flow.drain", caller));
        }
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct Params {
            #[serde(default)]
            resume: bool,
        }
        let p: Params = if params.is_null() {
            Params::default()
        } else {
            serde_json::from_value(params).map_err(invalid_params)?
        };
        if p.resume {
            self.rt.undrain().map_err(|e| self.core_error(e))?;
            return Ok(json!({ "draining": false }));
        }
        let unfinished = self.rt.drain().map_err(|e| self.core_error(e))?;
        Ok(json!({ "draining": true, "unfinished": unfinished }))
    }

    // ---- flow.disable and flow.enable ----------------------------------

    fn op_disable(&self, caller: &Caller, params: Value) -> OpResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            flow_id: String,
            #[serde(default)]
            reason: Option<String>,
        }
        let p: Params = serde_json::from_value(params).map_err(invalid_params)?;
        let actor = match caller {
            Caller::Operator => Actor::Operator("operator".into()),
            // The core checks ownership again in the disabling transaction.
            Caller::Agent(agent) if self.owns(agent, &p.flow_id)? => Actor::Agent(agent.clone()),
            _ => return Err(OpError::not_permitted("flow.disable", caller)),
        };
        let reason = p.reason.unwrap_or_else(|| "disabled by request".into());
        let changed = self
            .rt
            .disable_flow(&p.flow_id, &actor, &reason)
            .map_err(|e| self.install_error(e))?;
        Ok(json!({ "flow_id": p.flow_id, "state": "disabled", "changed": changed }))
    }

    fn op_enable(&self, caller: &Caller, params: Value) -> OpResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            flow_id: String,
        }
        let p: Params = serde_json::from_value(params).map_err(invalid_params)?;
        let actor = match caller {
            Caller::Operator => Actor::Operator("operator".into()),
            // Whether the owner may undo the flow's current disable (only
            // one it made itself) is decided by the core, in the same
            // transaction that enables the flow.
            Caller::Agent(agent) if self.owns(agent, &p.flow_id)? => Actor::Agent(agent.to_owned()),
            _ => return Err(OpError::not_permitted("flow.enable", caller)),
        };
        let changed = self
            .rt
            .enable_flow_as(&p.flow_id, &actor)
            .map_err(|e| self.install_error(e))?;
        Ok(json!({ "flow_id": p.flow_id, "state": "enabled", "changed": changed }))
    }
}

/// Milliseconds since the epoch as RFC 3339 in UTC (`2026-05-01T00:00:00Z`).
fn rfc3339(ms: i64) -> Result<String, OpError> {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_string())
        .map_err(|e| OpError::new("internal", format!("time {ms} out of range: {e}")))
}

/// The kind of actor a recorded `disabled_by` names: `operator`, `auto`
/// for the runtime's own disable, or the agent label as recorded.
fn disabled_kind(by: &str) -> &str {
    if by.starts_with(basal_core::install::OPERATOR_ACTOR_PREFIX) {
        "operator"
    } else if by == basal_core::install::RUNTIME_ACTOR {
        "auto"
    } else {
        by
    }
}

struct InstalledVersion {
    version: u32,
    script: String,
    manifest: String,
    author: String,
    loop_override: bool,
}
