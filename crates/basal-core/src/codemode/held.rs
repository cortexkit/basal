//! Custody of onward calls. Cursors belong to basal, not to an individual run:
//! one provider page can contain results from several concurrently running scopes.

use super::*;
use cortexkit_role_tool_provider::late_results::{
    AckRequest, CURSOR_INCARNATION_CHANGED, Cursor, EntryKind, IntakeKey, LateEntry,
    LateResultsRequest,
};
use cortexkit_role_tool_provider::scope::ScopeIdentity;

pub(super) enum Reply {
    Final(std::result::Result<Value, WireError>),
    /// `reason` is the provider's snake_case reason, such as `consent`.
    Held {
        reason: String,
        detail: Option<Value>,
    },
}

/// Decode only tools that explicitly negotiated content/v1; legacy JSON is
/// opaque even when it has members named content or held.
pub(super) fn classify(
    value: Value,
    format: Option<&cortexkit_role_tool_provider::result::ResultFormat>,
) -> Reply {
    use cortexkit_role_tool_provider::result::{ContentPart, ResultFormat, ToolResult};
    if !matches!(format, Some(ResultFormat::ContentV1)) {
        return Reply::Final(Ok(value));
    }
    let decoded = match serde_json::from_value::<ToolResult>(value) {
        Ok(decoded) => decoded,
        Err(error) => return Reply::Final(Err(WireError::Unreadable(error.to_string()))),
    };
    if let Err(error) = cortexkit_role_tool_provider::result::validate(&decoded) {
        return Reply::Final(Err(WireError::Unreadable(error.to_string())));
    }
    if let Some(held) = decoded.held() {
        return Reply::Held {
            reason: held.reason.clone(),
            detail: held.detail.clone(),
        };
    }
    let text = decoded
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if decoded.is_error == Some(true) {
        Reply::Final(Err(WireError::Refused {
            code: decoded.error_code.unwrap_or_else(|| "tool_error".into()),
            message: text,
        }))
    } else {
        Reply::Final(Ok(decoded
            .structured_content
            .map(Value::Object)
            .unwrap_or(Value::String(text))))
    }
}

#[path = "custody.rs"]
mod custody;
pub(super) use custody::{Custody, Target};

pub(super) fn map_entry(
    entry: LateEntry,
    format: Option<&cortexkit_role_tool_provider::result::ResultFormat>,
) -> Mapped {
    match entry.kind() {
        EntryKind::NotStarted => {
            let mut mapped = Mapped::rejected(
                Outcome::Withdrawn,
                "not_started",
                entry
                    .reason
                    .as_deref()
                    .unwrap_or("provider did not start the call"),
            );
            mapped.detail = entry.extra.get("detail").cloned();
            mapped
        }
        EntryKind::Result if !entry.reduced && entry.result.is_some() => {
            match classify(entry.result.unwrap(), format) {
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

/// Withdrawal is cleanup after a run stops, so it has a short independent
/// deadline. Retrying these custody operations never resends the tool action.
pub(super) fn withdraw(
    transport: &dyn Transport,
    route: &str,
    module: &str,
    request: cortexkit_role_tool_provider::withdraw::WithdrawArguments,
    mut options: basal_host::transport::RunToolOptions,
    format: Option<&cortexkit_role_tool_provider::result::ResultFormat>,
) -> Mapped {
    use cortexkit_role_tool_provider::withdraw::{CompletedResult, WithdrawAnswer};
    let until = Instant::now() + Duration::from_secs(2);
    let mut backoff = Duration::from_millis(25);
    loop {
        options.reply_timeout = until.saturating_duration_since(Instant::now());
        let reply = transport.role_for_run(
            route,
            module,
            "tool.withdraw",
            serde_json::to_value(&request).unwrap(),
            &options,
        );
        match reply {
            Err(error) if transient(&error) && Instant::now() + backoff < until => {
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(200));
            }
            Ok(value) => match WithdrawAnswer::decode(&value) {
                Ok(WithdrawAnswer::Completed {
                    result: CompletedResult::Retained(result),
                }) => {
                    return match classify(result, format) {
                        Reply::Final(result) => map_answer(result),
                        Reply::Held { .. } => Mapped::rejected(
                            Outcome::OutcomeUnknown,
                            "no_outcome",
                            "completed withdrawal retained an interim reply",
                        ),
                    };
                }
                Ok(answer) if answer.guarantees_not_run() => {
                    return Mapped::rejected(
                        Outcome::Withdrawn,
                        "withdrawn",
                        "provider confirmed the action never started",
                    );
                }
                _ => {
                    return Mapped::rejected(
                        Outcome::OutcomeUnknown,
                        "no_outcome",
                        "provider could not prove the action's outcome",
                    );
                }
            },
            Err(_) => {
                return Mapped::rejected(
                    Outcome::OutcomeUnknown,
                    "no_outcome",
                    "withdrawal outcome is unknown",
                );
            }
        }
    }
}

fn transient(error: &WireError) -> bool {
    match error {
        WireError::TimedOut(_) | WireError::Unknown(_) => true,
        WireError::Typed(error) => matches!(
            error.reason,
            basal_host::flow_refusal::RefusalReason::ScopeNotSynced
                | basal_host::flow_refusal::RefusalReason::ScopeChanged
        ),
        WireError::Refused { code, .. } | WireError::RefusedDetails { code, .. } => {
            cortexkit_role_tool_provider::errors::is_transient(code)
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "custody_intake_tests.rs"]
mod intake_tests;

#[cfg(test)]
#[path = "held_cursor_tests.rs"]
mod tests;
