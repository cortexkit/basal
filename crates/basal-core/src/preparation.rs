//! Policy and frozen-request preparation shared by log-backed activation services.
use crate::{
    authorize::{self, Refusal, codes},
    ids::idempotency_key,
    manifest::Manifest,
    model::Run,
    runs::Lease,
    runtime::Runtime,
    tokens,
};
use basal_proto::{CallKind, HostCall, JsonText, Primitive};
use serde_json::Value;
use std::collections::BTreeSet;

/// A call ready to journal, including a board call waiting for its registry read.
#[derive(Debug, Default)]
pub(crate) struct Prepared {
    /// Prepared dispatch bytes, rather than the script's arguments. Absent
    /// while a board or targeted call's agent registry read is deferred.
    pub(crate) request: Option<JsonText>,
    /// Tokens to reserve for a model call.
    pub(crate) tokens: Option<u64>,
    pub(crate) model_cost_micros: u64,
    pub(crate) classify_cost: bool,
    pub(crate) args: Option<Value>,
    /// A failed agent registry read is retried without dispatching the call.
    pub(crate) deferred: Option<basal_host::flow_refusal::FlowRefusal>,
}

/// Prepare approved requests from the owning actor snapshot. Targeted module
/// calls count stable agent ids in frozen requests to enforce max_targets.
pub(crate) struct CallPreparation<'a> {
    pub(crate) rt: &'a Runtime,
    pub(crate) lease: &'a Lease,
    pub(crate) run: &'a Run,
    pub(crate) manifest: &'a Option<Manifest>,
    pub(crate) input: &'a Value,
    pub(crate) trigger: &'a Value,
    pub(crate) tool_policies: &'a crate::classify_cost::ToolPolicies,
    pub(crate) state: &'a crate::run_records::State,
}

pub(crate) fn disabled_refusal() -> Refusal {
    Refusal::new(
        codes::FLOW_DISABLED,
        "the flow is disabled; it makes no new calls",
    )
}

impl CallPreparation<'_> {
    /// Checks a new call's argument size and its grant in the approved
    /// manifest, then selects a model and builds its clamped request. The
    /// selector runs before the intent transaction, so refusal sends no model work.
    pub(crate) fn prepare(&self, call: &HostCall) -> std::result::Result<Prepared, Refusal> {
        let cap = self.rt.config.limits.max_arg_bytes;
        if call.args.len() > cap {
            return Err(Refusal::new(
                codes::TOO_LARGE,
                format!(
                    "arguments of {} bytes exceed the cap of {cap}",
                    call.args.len()
                ),
            ));
        }
        let mut args: Value = serde_json::from_str(call.args.as_str())
            .map_err(|_| Refusal::new(codes::INVALID_ARGUMENTS, "arguments are not JSON"))?;
        if matches!(
            call.kind,
            CallKind::Primitive(Primitive::Llm | Primitive::Classify)
        ) && (args.get("model").is_some() || args.get("provider").is_some())
        {
            return Err(Refusal::new(
                "model_not_allowed",
                "scripts cannot select a provider or model",
            ));
        }
        let Some(manifest) = self.manifest else {
            return Err(Refusal::denied("the run has no approved manifest"));
        };
        if matches!(
            call.kind,
            CallKind::Primitive(
                Primitive::SinkDigest
                    | Primitive::SinkStatus
                    | Primitive::SinkBoard
                    | Primitive::Facts
            )
        ) && args["agent"] == "$self"
            && let Some(agent) = self.input["agent_id"].as_str()
        {
            args["agent"] = Value::String(agent.to_owned());
        }
        // Before the board target is looked up in the registry, so a
        // placeholder is refused for what it is rather than as an unknown agent.
        authorize::placeholder_target(manifest, &call.kind, &args)?;
        let resolved_manifest = if matches!(call.kind, CallKind::Primitive(Primitive::SinkBoard))
            && !manifest.board.is_empty()
        {
            let target = args["agent"].as_str().ok_or_else(|| {
                Refusal::new(
                    codes::INVALID_ARGUMENTS,
                    "sink.board needs an agent reference",
                )
            })?;
            let mut refs = vec![target];
            if manifest.audience.is_none() {
                refs.extend(manifest.board.iter().map(|grant| grant.agent.as_str()));
            }
            // Resolve the target and every grant from the same registry read,
            // so aliases are compared as stable ids without a second lookup.
            let resolved = match self.rt.catalog().resolve_agents(&refs) {
                Ok(ids) if ids.len() == refs.len() => ids,
                _ => {
                    return Ok(Prepared {
                        deferred: Some(basal_host::flow_refusal::FlowRefusal::new(
                            basal_host::flow_refusal::RefusalReason::SinkBoardTargetUnavailable,
                            basal_host::subc_catalog::CORE,
                            "agent.list",
                        )),
                        args: Some(args),
                        ..Prepared::default()
                    });
                }
            };
            let agent = resolved[0].as_ref().ok_or_else(|| {
                Refusal::new(
                    "sink_board_target_not_granted",
                    "board target is not in the agent registry",
                )
            })?;
            args["agent"] = Value::String(agent.clone());
            let mut resolved_manifest = manifest.clone();
            if manifest.audience.is_none() {
                resolved_manifest.board = manifest
                    .board
                    .iter()
                    .zip(&resolved[1..])
                    .filter_map(|(grant, id)| {
                        id.as_ref().map(|id| {
                            let mut grant = grant.clone();
                            grant.agent = id.clone();
                            grant
                        })
                    })
                    .collect();
            }
            Some(resolved_manifest)
        } else {
            None
        };
        let manifest = resolved_manifest.as_ref().unwrap_or(manifest);
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkDigest))
            && args.get("withdraw").and_then(Value::as_bool) != Some(true)
            && args.get("action").is_none_or(Value::is_null)
            && let Some(cap) = args
                .get("agent")
                .and_then(Value::as_str)
                .and_then(|agent| manifest.digest_cap(agent))
        {
            args["action"] = serde_json::to_value(cap)
                .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
        }
        if let Err(refusal) = authorize::check(
            manifest,
            &self.rt.config.shell_denylist,
            self.rt.catalog(),
            &call.kind,
            &args,
        ) {
            use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
            let reason = if refusal.code == RefusalReason::ProviderNotCatalogued.as_str() {
                RefusalReason::ProviderNotCatalogued
            } else if refusal.code == RefusalReason::CatalogUnavailable.as_str() {
                RefusalReason::CatalogUnavailable
            } else {
                return Err(refusal);
            };
            let (CallKind::Op { module, op } | CallKind::OpFor { module, op }) = &call.kind else {
                return Err(refusal);
            };
            // Authorization has not built or sent a request. Recheck the live
            // declaration after the same readiness backoff as scope registration.
            return Ok(Prepared {
                deferred: Some(FlowRefusal::new(reason, module, op)),
                args: Some(args),
                ..Prepared::default()
            });
        }
        if matches!(
            call.kind,
            CallKind::Primitive(Primitive::SinkNoticeSend | Primitive::SinkNoticeResolve)
        ) && !self
            .rt
            .shared
            .host
            .supports_core_capability(basal_host::core_host::FLOW_NOTICE_V1)
        {
            return Err(Refusal::new(
                codes::NOTICE_UNSUPPORTED,
                "prefrontal-core does not currently advertise flow-notice/v1",
            ));
        }
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkWaitResolve))
            && !self
                .rt
                .shared
                .host
                .supports_core_capability(basal_host::core_host::FLOW_WAIT_RESOLVE_V1)
        {
            return Err(Refusal::new(
                "wait_resolve_unsupported",
                "prefrontal-core does not currently advertise flow-wait-resolve/v1",
            ));
        }
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkRoomPost))
            && !self
                .rt
                .shared
                .host
                .supports_core_capability(basal_host::core_host::FLOW_ROOM_POST_V1)
        {
            return Err(Refusal::new(
                codes::ROOM_POST_UNSUPPORTED,
                "prefrontal-core does not currently advertise flow-room-post/v1",
            ));
        }
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkDigest))
            && args.get("withdraw").and_then(Value::as_bool) != Some(true)
            && args
                .get("options")
                .and_then(|options| options.get("replace_key"))
                .is_some()
            && !self
                .rt
                .shared
                .host
                .supports_core_capability(basal_host::core_host::FLOW_DIGEST_REPLACE_KEY_V1)
        {
            return Err(Refusal::new(
                codes::DIGEST_REPLACE_KEY_UNSUPPORTED,
                "prefrontal-core does not currently advertise flow-digest-replace-key/v1",
            ));
        }
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkDigest))
            && args.get("withdraw").and_then(Value::as_bool) == Some(true)
            && !self
                .rt
                .shared
                .host
                .supports_core_capability(basal_host::core_host::FLOW_DIGEST_WITHDRAW_V1)
        {
            return Err(Refusal::new(
                codes::DIGEST_WITHDRAW_UNSUPPORTED,
                "prefrontal-core does not currently advertise flow-digest-withdraw/v1",
            ));
        }
        if !matches!(call.kind, CallKind::OpFor { .. }) {
            self.rt
                .shared
                .host
                .provider_ready(
                    &basal_host::flow_scope::route_key(&self.run.flow_id, self.run.flow_version),
                    &call.kind,
                )
                .map_err(|refusal| {
                    let mut rejected = Refusal::new(refusal.reason.as_str(), refusal.message());
                    rejected.module = Some(refusal.provider);
                    rejected
                })?;
        }
        let classify_cost = match &call.kind {
            CallKind::Op { module, op } => {
                match self.tool_policies.get(&(module.clone(), op.clone())) {
                    Some(crate::classify_cost::ToolPolicy::Classify) => true,
                    Some(
                        crate::classify_cost::ToolPolicy::Ordinary
                        | crate::classify_cost::ToolPolicy::LegacyOrdinary,
                    ) => false,
                    None => {
                        return Err(Refusal::new(
                            "tool_policy_missing",
                            "the approved version has no saved policy for this op",
                        ));
                    }
                }
            }
            _ => false,
        };
        let prepared: std::result::Result<Prepared, Refusal> = match &call.kind {
            CallKind::Op { .. } if classify_cost => {
                if manifest.model_cost_usd.is_none() {
                    return Err(Refusal::new(
                        "model_cost_cap_required",
                        "the manifest grants no classifier dollar cap",
                    ));
                }
                Ok(Prepared {
                    classify_cost: true,
                    ..Prepared::default()
                })
            }
            CallKind::Op { module, op }
                if basal_host::core_host::is_core_carrier_op(module, op) =>
            {
                let object = args.as_object_mut().ok_or_else(|| {
                    Refusal::new(
                        codes::INVALID_ARGUMENTS,
                        "core carrier arguments must be an object",
                    )
                })?;
                if ["stamp", "flow_id", "flow_version", "code_hash"]
                    .iter()
                    .any(|field| object.contains_key(*field))
                {
                    return Err(Refusal::new(
                        codes::INVALID_ARGUMENTS,
                        "scripts cannot supply an approved-install stamp",
                    ));
                }
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                // Journal the trusted stamp with the request so a replay uses
                // the same approved install, even after a newer version exists.
                object.insert(
                    "stamp".into(),
                    serde_json::json!({
                        "flow_id": self.run.flow_id,
                        "flow_version": version,
                        "code_hash": crate::ids::hex(&self.run.code_hash),
                    }),
                );
                let request = JsonText::new(args.to_string())
                    .map_err(|e| Refusal::new(codes::TOO_LARGE, e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    ..Prepared::default()
                })
            }
            CallKind::OpFor { .. } => {
                if let CallKind::OpFor { module, op } = &call.kind
                    && basal_host::core_host::is_core_carrier_op(module, op)
                {
                    return Err(Refusal::new(
                        codes::INVALID_ARGUMENTS,
                        "core carrier ops select their audience in arguments, not the for option",
                    ));
                }
                let name = args
                    .get("for")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        Refusal::new(codes::INVALID_ARGUMENTS, "for must name an agent")
                    })?;
                let agent = match self.rt.catalog().agent_id(name) {
                    Ok(Some(agent)) => agent,
                    Ok(None) => {
                        return Err(Refusal::new(
                            "sink_audience_unavailable",
                            "target agent is not in the agent registry",
                        ));
                    }
                    // A failed registry read is not evidence that the target is
                    // unknown. Defer the provably unsent call with the same
                    // durable backoff, bounded by the run deadline, that core's
                    // own transient audience refusal uses.
                    Err(_) => {
                        return Ok(Prepared {
                            deferred: Some(basal_host::flow_refusal::FlowRefusal::new(
                                basal_host::flow_refusal::RefusalReason::SinkAudienceUnavailable,
                                basal_host::subc_catalog::CORE,
                                "agent.list",
                            )),
                            args: Some(args),
                            ..Prepared::default()
                        });
                    }
                };
                let targets = self
                    .state
                    .calls
                    .values()
                    .filter_map(|c| {
                        if !matches!(c.identity.kind, CallKind::OpFor { .. }) {
                            return None;
                        }
                        let value: Value =
                            serde_json::from_str(c.intent.as_ref()?.frozen_request.as_str())
                                .ok()?;
                        value["agent"].as_str().map(str::to_owned)
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                if !targets.contains(&agent) && targets.len() >= manifest.max_targets as usize {
                    return Err(Refusal::new(
                        "too_many_targets",
                        "distinct target limit reached",
                    ));
                }
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("target calls require an installed version"))?;
                let expires_at_ms = self
                    .run
                    .deadline_at
                    .ok_or_else(|| Refusal::denied("target calls require a run deadline"))?;
                let request = JsonText::new(serde_json::json!({"agent":agent,"version":version,"expires_at_ms":expires_at_ms,"args":args.get("args").unwrap_or(&Value::Null)}).to_string())
                    .map_err(|e| Refusal::new(codes::TOO_LARGE,e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(p @ (Primitive::Llm | Primitive::Classify)) => {
                let Some(grant) = &manifest.llm else {
                    return Err(Refusal::denied("the manifest grants no model calls"));
                };
                let send_id = idempotency_key(&self.run.flow_id, &self.lease.run_id, call.position);
                let clamped = tokens::clamp(
                    *p,
                    &call.args,
                    &args,
                    grant,
                    tokens::Identity {
                        flow_id: &self.run.flow_id,
                        run_id: &self.lease.run_id,
                        send_id: &send_id,
                        position: call.position,
                    },
                )?;
                let selection_request = basal_host::selector::SelectionRequest {
                    input_tokens: clamped
                        .reserve
                        .saturating_sub(u64::from(clamped.max_output)),
                    max_output_tokens: u64::from(clamped.max_output),
                    caller_class: "flow".into(),
                    iq: grant.iq,
                    eq: grant.eq,
                    flow_id: self.run.flow_id.clone(),
                    run_id: self.lease.run_id.clone(),
                    send_id,
                };
                let mut retries = 0;
                let selection = loop {
                    match self.rt.config.selector.select(&selection_request) {
                        Ok(selection) => break selection,
                        Err(basal_host::selector::SelectionError::Wait(refusal)) => {
                            return Ok(Prepared {
                                deferred: Some(*refusal),
                                ..Prepared::default()
                            });
                        }
                        Err(basal_host::selector::SelectionError::Refused { code, detail }) => {
                            return Err(Refusal::new(code, detail));
                        }
                        Err(basal_host::selector::SelectionError::Unavailable { detail }) => {
                            if retries >= self.rt.config.unavailable_retries {
                                return Err(Refusal::new("route_unavailable", detail));
                            }
                            retries += 1;
                            std::thread::sleep(self.rt.config.retry_backoff);
                        }
                    }
                };
                let price = selection.price.as_ref().ok_or_else(|| {
                    Refusal::new("price_unknown", "routing did not price the selected model")
                })?;
                let model_cost_micros = basal_host::model_cost::reserve(
                    price,
                    clamped
                        .reserve
                        .saturating_sub(u64::from(clamped.max_output)),
                    u64::from(clamped.max_output),
                )
                .map_err(|e| Refusal::new("price_unknown", e.to_string()))?;
                let mut envelope: Value = serde_json::from_str(clamped.request.as_str())
                    .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                envelope["selection"] = serde_json::to_value(selection)
                    .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                let request = JsonText::new(envelope.to_string())
                    .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    tokens: Some(clamped.reserve),
                    model_cost_micros,
                    args: None,
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(Primitive::SinkWaitResolve) => {
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                // Freeze the approved identity and outcome before sending. A
                // lost reply must reuse the same payload and receipt on retry.
                let mut value = serde_json::json!({
                    "stamp": {"flow_id":self.run.flow_id, "flow_version":version,
                        "code_hash":crate::ids::hex(&self.run.code_hash)},
                    "outcome":args["outcome"],
                });
                if let Some(note) = args.get("note") {
                    value["note"] = note.clone();
                }
                Ok(Prepared {
                    request: Some(
                        JsonText::new(value.to_string())
                            .map_err(|e| Refusal::new(codes::TOO_LARGE, e.to_string()))?,
                    ),
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(Primitive::SinkNoticeSend | Primitive::SinkNoticeResolve) => {
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                // Freeze only the supplied fields with the approved stamp. In
                // particular, omitted body/resolution must not become null.
                let mut value = args.clone();
                value["stamp"] = serde_json::json!({"flow_id":self.run.flow_id,
                    "flow_version":version, "code_hash":crate::ids::hex(&self.run.code_hash)});
                Ok(Prepared {
                    request: Some(
                        JsonText::new(value.to_string())
                            .map_err(|e| Refusal::new(codes::TOO_LARGE, e.to_string()))?,
                    ),
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(Primitive::SinkRoomPost) => {
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                // Freeze the approved identity, room, text and mentions before
                // sending, so a retry after a lost reply posts the same
                // message under the same receipt instead of a newer install's.
                // Core requires a list, so a post that names no one (options
                // omitted, or `mentions: []`) sends an empty one.
                let mentions = match args.get("mentions") {
                    Some(list @ Value::Array(_)) => list.clone(),
                    _ => Value::Array(Vec::new()),
                };
                let value = serde_json::json!({
                    "stamp": {"flow_id":self.run.flow_id, "flow_version":version,
                        "code_hash":crate::ids::hex(&self.run.code_hash)},
                    "room_id":args["room_id"], "text":args["text"], "mentions":mentions,
                });
                Ok(Prepared {
                    request: Some(
                        JsonText::new(value.to_string())
                            .map_err(|e| Refusal::new(codes::TOO_LARGE, e.to_string()))?,
                    ),
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(Primitive::SinkBoard) => {
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                // Record the flow version, code hash and board operation before
                // sending, so recovery cannot switch to a newer install. The
                // host adds receipt_key from this call's journal idempotency key
                // so core can return the original result after a lost reply.
                let value = serde_json::json!({
                    "stamp": {"flow_id":self.run.flow_id, "flow_version":version,
                        "code_hash":crate::ids::hex(&self.run.code_hash)},
                    "agent":args["agent"], "operation":args["operation"],
                });
                Ok(Prepared {
                    request: Some(
                        JsonText::new(value.to_string())
                            .map_err(|e| Refusal::new(codes::TOO_LARGE, e.to_string()))?,
                    ),
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(
                p @ (Primitive::SinkDigest | Primitive::SinkStatus | Primitive::Facts),
            ) => {
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                let due_at = match self.trigger.get("due").and_then(Value::as_str) {
                    Some(due) => due
                        .parse::<jiff::Timestamp>()
                        .map_err(|_| {
                            Refusal::new(codes::INVALID_ARGUMENTS, "invalid schedule due time")
                        })?
                        .as_millisecond(),
                    None => self.run.admitted_at,
                };
                // Store the entire core request with the intent so a restart
                // reuses its timestamps instead of sampling a new clock.
                let value = basal_host::core_host::intent(
                    *p,
                    &args,
                    basal_host::core_host::IntentContext {
                        flow_id: &self.run.flow_id,
                        version,
                        run_id: &self.lease.run_id,
                        position: call.position,
                        due_at,
                        created_at: self.rt.config.clock.now_ms(),
                    },
                );
                let request = JsonText::new(value.to_string())
                    .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    tokens: None,
                    args: None,
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(p) if p.is_builtin() => {
                // The scope travels with the call, so the host checks the
                // same scope again at the moment it acts, and a resend after
                // a crash carries the scope the call was approved under.
                let grant = authorize::builtin_grant(manifest, *p, &args)?;
                let mut envelope = basal_host::builtins::envelope(&args, &grant);
                if *p == Primitive::NetFetch && args["options"].get("secret").is_some() {
                    envelope["flow_code_hash"] =
                        serde_json::json!(crate::ids::hex(&self.run.code_hash));
                    envelope["credential"] =
                        serde_json::json!({"name":args["options"]["secret"],"value":"[secret]"});
                }
                let request = JsonText::new(envelope.to_string())
                    .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    tokens: None,
                    args: None,
                    ..Prepared::default()
                })
            }
            _ => Ok(Prepared::default()),
        };
        Ok(Prepared {
            args: Some(args),
            ..prepared?
        })
    }
}
