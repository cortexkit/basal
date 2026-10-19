//! Custody of onward calls. Cursors belong to basal, not to an individual run:
//! one provider page can contain results from several concurrently running scopes.

use super::*;
use cortexkit_role_tool_provider::late_results::{
    AckRequest, CURSOR_INCARNATION_CHANGED, Cursor, EntryKind, IntakeKey, LateEntry,
    LateResultsReply, LateResultsRequest,
};
use cortexkit_role_tool_provider::scope::ScopeIdentity;

pub(super) enum Reply {
    Final(std::result::Result<Value, WireError>),
    Held { detail: Option<Value> },
}

/// Local decoding until the pending tool-provider held-reply/v1 release adds
/// the shared held field to ToolResult; replace only this classifier then.
/// Custody, polling and the program's value mapping stay unchanged.
pub(super) fn classify(value: Value) -> Reply {
    let decoded = if value.get("held").is_some() || value.get("content").is_some() {
        match serde_json::from_value::<cortexkit_role_tool_provider::result::ToolResult>(
            value.clone(),
        ) {
            Ok(result) => {
                if let Err(error) = cortexkit_role_tool_provider::result::validate(&result) {
                    return Reply::Final(Err(WireError::Unreadable(error.to_string())));
                }
                Some(result)
            }
            Err(error) => return Reply::Final(Err(WireError::Unreadable(error.to_string()))),
        }
    } else {
        None
    };
    if let Some(held) = value.get("held") {
        let reason = held.get("reason").and_then(Value::as_str);
        let valid_reason = reason.is_some_and(|reason| {
            !reason.is_empty()
                && reason.split('_').all(|word| {
                    !word.is_empty()
                        && word
                            .bytes()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                })
                && reason.as_bytes()[0].is_ascii_lowercase()
        });
        let detail = held.get("detail").cloned();
        if !held.is_object()
            || !valid_reason
            || detail.as_ref().is_some_and(|v| !v.is_object())
            || value.get("isError").is_some_and(|v| v != false)
            || content_text(&value).is_err()
        {
            return Reply::Final(Err(WireError::Unreadable("malformed held result".into())));
        }
        return Reply::Held { detail };
    }
    // Providers without content/v1 retain their existing JSON value contract.
    if value.get("content").is_none() {
        return Reply::Final(Ok(value));
    }
    let text = match content_text(&value) {
        Ok(text) => text,
        Err(error) => return Reply::Final(Err(error)),
    };
    match value.get("isError") {
        Some(Value::Bool(true)) => Reply::Final(Err(WireError::Refused {
            code: decoded
                .and_then(|r| r.error_code)
                .unwrap_or_else(|| "tool_error".into()),
            message: text,
        })),
        None | Some(Value::Bool(false)) => Reply::Final(Ok(value
            .get("structuredContent")
            .cloned()
            .unwrap_or(Value::String(text)))),
        _ => Reply::Final(Err(WireError::Unreadable(
            "invalid content/v1 isError".into(),
        ))),
    }
}

fn content_text(value: &Value) -> std::result::Result<String, WireError> {
    let parts = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| WireError::Unreadable("content/v1 content is not an array".into()))?;
    let mut text = Vec::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) == Some("text") {
            text.push(
                part.get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WireError::Unreadable("invalid content/v1 text part".into()))?,
            );
        }
    }
    Ok(text.join("\n"))
}

#[derive(Clone)]
struct Target {
    run: String,
    scope: ScopeIdentity,
    call: HostCall,
    events: mpsc::Sender<Event>,
    held: bool,
}

#[derive(Default)]
pub(super) struct Custody {
    calls: Mutex<BTreeMap<(String, String), Target>>,
    // Serialize pulls and acknowledgements across runs: acking a page before
    // another run has recorded its entry would lose that run's only answer.
    cursors: Mutex<BTreeMap<String, Arc<Mutex<Option<Cursor>>>>>,
}

impl Custody {
    pub(super) fn register(
        &self,
        module: &str,
        run: &str,
        scope: ScopeIdentity,
        call: &HostCall,
        events: mpsc::Sender<Event>,
    ) {
        self.calls.lock().unwrap().insert(
            (module.into(), store::call_key(run, call.position)),
            Target {
                run: run.into(),
                scope,
                call: call.clone(),
                events,
                held: false,
            },
        );
    }

    pub(super) fn mark_held(&self, module: &str, run: &str, position: u64, held: bool) {
        if let Some(target) = self
            .calls
            .lock()
            .unwrap()
            .get_mut(&(module.into(), store::call_key(run, position)))
        {
            target.held = held;
        }
    }

    pub(super) fn has_held(&self, route: &str, module: &str) -> bool {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .any(|((provider, _), target)| {
                provider == module && route == format!("codemode:{}", target.run) && target.held
            })
    }

    pub(super) fn remove(&self, module: &str, run: &str, position: u64) {
        self.calls
            .lock()
            .unwrap()
            .remove(&(module.into(), store::call_key(run, position)));
    }

    pub(super) fn end(&self, run: &str) {
        self.calls
            .lock()
            .unwrap()
            .retain(|_, target| target.run != run);
    }

    pub(super) fn pull(
        &self,
        transport: &dyn Transport,
        route: &str,
        module: &str,
        options: &basal_host::transport::RunToolOptions,
    ) -> std::result::Result<bool, WireError> {
        let cursor = self
            .cursors
            .lock()
            .unwrap()
            .entry(module.into())
            .or_default()
            .clone();
        let mut cursor = cursor.lock().unwrap();
        // Another run may have consumed our final entry while this pull waited
        // for the custodian cursor. Do not send a now-unnecessary request.
        if !self.has_held(route, module) {
            return Ok(false);
        }
        let mut request = LateResultsRequest::new().with_limit(100);
        request.since = cursor.clone();
        let reply = transport.role_for_run(
            route,
            module,
            "late_results",
            serde_json::to_value(request).unwrap(),
            options,
        );
        let value = match reply {
            Err(
                WireError::Refused { ref code, .. } | WireError::RefusedDetails { ref code, .. },
            ) if code == CURSOR_INCARNATION_CHANGED => {
                *cursor = None;
                return Ok(true);
            }
            other => other?,
        };
        let reply: LateResultsReply =
            serde_json::from_value(value).map_err(|e| WireError::Unreadable(e.to_string()))?;
        let mut seen = BTreeSet::<IntakeKey>::new();
        for entry in reply.entries {
            if entry.custodian != "reserved:basal" {
                return Err(WireError::Unreadable(
                    "foreign late result custodian".into(),
                ));
            }
            if !seen.insert(entry.intake_key(module)) {
                continue;
            }
            let target = self
                .calls
                .lock()
                .unwrap()
                .get(&(module.into(), entry.call_key.clone()))
                .cloned();
            if let Some(target) = target {
                if entry.owner != target.scope.owner
                    || entry.scope_ref != target.scope.scope_ref
                    || entry.scope_epoch != target.scope.scope_epoch
                {
                    return Err(WireError::Unreadable("late result scope mismatch".into()));
                }
                let (ack, done) = mpsc::sync_channel(1);
                if target
                    .events
                    .send(Event::LateAnswer {
                        call: target.call,
                        entry,
                        ack,
                    })
                    .is_ok()
                {
                    done.recv_timeout(Duration::from_secs(10)).map_err(|_| {
                        WireError::TimedOut("late result intake did not finish".into())
                    })?;
                }
            }
            // No active target means the call already settled or its run ended.
            // Restart recovery interrupts every old run; none can be replayed.
        }
        transport.role_for_run(
            route,
            module,
            "late_results.ack",
            serde_json::to_value(AckRequest {
                through: reply.cursor.clone(),
            })
            .unwrap(),
            options,
        )?;
        *cursor = Some(reply.cursor);
        Ok(reply.more)
    }
}

pub(super) fn map_entry(entry: LateEntry) -> Mapped {
    match entry.kind() {
        EntryKind::NotStarted => Mapped::rejected(
            Outcome::Withdrawn,
            "not_started",
            entry
                .reason
                .as_deref()
                .unwrap_or("provider did not start the call"),
        ),
        EntryKind::Result if !entry.reduced && entry.result.is_some() => {
            match classify(entry.result.unwrap()) {
                Reply::Final(result) => map_answer(result),
                Reply::Held { .. } => Mapped::rejected(
                    Outcome::OutcomeUnknown,
                    "reply_unreadable",
                    "a late result cannot hold the call again",
                ),
            }
        }
        EntryKind::Result => Mapped::rejected(
            Outcome::OutcomeUnknown,
            "result_reduced",
            entry
                .outcome
                .as_deref()
                .unwrap_or("provider retained no result"),
        ),
        EntryKind::Expired => Mapped::rejected(
            Outcome::OutcomeUnknown,
            "result_expired",
            "provider result expired before intake",
        ),
        EntryKind::Unknown(_) => Mapped::rejected(
            Outcome::OutcomeUnknown,
            "late_result_unknown",
            "provider returned an unknown late entry kind",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Provider {
        page: Value,
        settled: Arc<AtomicUsize>,
        reads: AtomicUsize,
    }
    impl Transport for Provider {
        fn role_for_run(
            &self,
            _: &str,
            _: &str,
            name: &str,
            arguments: Value,
            _: &basal_host::transport::RunToolOptions,
        ) -> std::result::Result<Value, WireError> {
            match name {
                "late_results" => {
                    assert_eq!(arguments["since"], Value::Null);
                    self.reads.fetch_add(1, Ordering::SeqCst);
                    Ok(self.page.clone())
                }
                "late_results.ack" => {
                    assert_eq!(
                        self.settled.load(Ordering::SeqCst),
                        2,
                        "both runs must take custody before ack"
                    );
                    Ok(json!({}))
                }
                _ => panic!("unexpected role request"),
            }
        }
        fn catalog(&self) -> std::result::Result<Value, WireError> {
            panic!("no catalog")
        }
        fn management(&self, _: &str, _: &str, _: Value) -> std::result::Result<Value, WireError> {
            panic!("no management")
        }
        fn tool(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> std::result::Result<Value, WireError> {
            panic!("no tool send")
        }
    }

    #[test]
    fn custodian_page_reaches_both_runs_before_ack_and_rechecks_held_eligibility() {
        let custody = Custody::default();
        let settled = Arc::new(AtomicUsize::new(0));
        let mut entries = Vec::new();
        let mut receivers = Vec::new();
        for run in ["a", "b"] {
            let (events, receiver) = mpsc::channel();
            let call = HostCall {
                position: 0,
                kind: CallKind::Tool {
                    name: "browser_use".into(),
                },
                args: JsonText::null(),
            };
            let scope = ScopeIdentity {
                owner: "reserved:core".into(),
                scope_ref: run.into(),
                scope_epoch: 7,
            };
            custody.register("provider", run, scope, &call, events);
            custody.mark_held("provider", run, 0, true);
            entries.push(
                LateEntry::new(
                    "result",
                    "reserved:core",
                    run,
                    7,
                    "reserved:basal",
                    store::call_key(run, 0),
                    format!("e-{run}"),
                    100,
                )
                .with_result(json!(run)),
            );
            let settled = settled.clone();
            receivers.push(thread::spawn(move || {
                let Event::LateAnswer { entry, ack, .. } =
                    receiver.recv_timeout(Duration::from_secs(5)).unwrap()
                else {
                    panic!("expected final entry")
                };
                assert_eq!(entry.result, Some(json!(run)));
                settled.fetch_add(1, Ordering::SeqCst);
                ack.send(()).unwrap();
            }));
        }
        let provider = Provider {
            page: serde_json::to_value(LateResultsReply {
                entries,
                cursor: Cursor {
                    provider_incarnation: "i".into(),
                    seq: 2,
                },
                more: false,
            })
            .unwrap(),
            settled,
            reads: AtomicUsize::new(0),
        };
        let options = basal_host::transport::RunToolOptions {
            identity: subc_protocol::BindIdentity::new("/", "basal", "test"),
            reply_timeout: Duration::from_secs(1),
        };
        assert!(
            !custody
                .pull(&provider, "codemode:a", "provider", &options)
                .unwrap()
        );
        for receiver in receivers {
            receiver.join().unwrap();
        }
        custody.remove("provider", "a", 0);
        assert!(!custody.has_held("codemode:a", "provider"));
        assert!(custody.has_held("codemode:b", "provider"));
        // A pull scheduled by the first run before its final entry arrived
        // must recheck eligibility after obtaining the shared cursor.
        assert!(
            !custody
                .pull(&provider, "codemode:a", "provider", &options)
                .unwrap()
        );
        assert_eq!(provider.reads.load(Ordering::SeqCst), 1);
        custody.end("b");
        assert!(!custody.has_held("codemode:b", "provider"));
    }
}
