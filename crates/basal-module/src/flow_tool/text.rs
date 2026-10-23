//! Short agent-facing replies; the content/v1 envelope retains the complete report.

use basal_core::dry_run_text::{CAPTURED_CODE, CAPTURED_MESSAGE};
use serde_json::Value;

const MAX_OUTCOMES: usize = 5;

fn items(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn field<'a>(value: &'a Value, name: &str) -> &'a str {
    value[name].as_str().unwrap_or("unknown")
}

fn plural(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

// Keep script-supplied errors on one short line, never serialized as JSON.
fn short(text: &str) -> String {
    let mut chars = text.chars().map(|c| if c.is_control() { ' ' } else { c });
    let mut text: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text
}

fn call_name(call: &Value) -> String {
    if call["kind"] == "op" {
        format!("{} {}", field(call, "module"), field(call, "op"))
    } else {
        field(call, "kind").into()
    }
}

fn is_captured(call: &Value) -> bool {
    call["action"] == "captured" || call["outcome"]["rejected"] == CAPTURED_CODE
}

fn outcome(run: &Value) -> String {
    let calls = items(&run["calls"]);
    let capture_error = format!("HostError: {CAPTURED_MESSAGE}");
    let stopped = run["state"] == "failed"
        && run["error"]["kind"] == "script"
        && run["error"]["detail"]
            .as_str()
            .and_then(|detail| detail.lines().next())
            == Some(capture_error.as_str());
    let mut text = if let Some(call) = calls
        .iter()
        .rev()
        .find(|call| is_captured(call))
        .filter(|_| stopped)
    {
        format!(
            "stopped at {}, which the dry run captured and did not execute",
            short(&call_name(call))
        )
    } else if let Some(admission) = run["admission"].as_str() {
        format!("not admitted: {}", short(admission))
    } else {
        let mut text = field(run, "state").to_owned();
        if let Some(kind) = run["error"]["kind"].as_str() {
            text.push_str(&format!(" ({})", short(kind)));
            if let Some(detail) = run["error"]["detail"].as_str() {
                text.push_str(&format!(
                    ": {}",
                    short(detail.lines().next().unwrap_or(detail))
                ));
            }
        }
        let total = run["calls_total"].as_u64().unwrap_or(calls.len() as u64);
        text.push_str(&format!("; {total} call{}", plural(total)));
        let mut call_groups: Vec<(String, usize)> = Vec::new();
        for call in calls {
            let detail = if is_captured(call) {
                " (not executed)".into()
            } else if let Some(code) = call["outcome"]["rejected"].as_str() {
                format!(" (rejected: {})", short(code))
            } else {
                String::new()
            };
            let label = format!(
                "{} {}{detail}",
                field(call, "action"),
                short(&call_name(call))
            );
            if let Some((_, count)) = call_groups
                .iter_mut()
                .find(|(existing, _)| *existing == label)
            {
                *count += 1;
            } else {
                call_groups.push((label, 1));
            }
        }
        if !call_groups.is_empty() {
            text.push_str(": ");
            text.push_str(
                &call_groups
                    .iter()
                    .take(3)
                    .map(|(label, count)| format!("{count} {label}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            if call_groups.len() > 3 {
                text.push_str(&format!(", {} other call outcomes", call_groups.len() - 3));
            }
        }
        if total > calls.len() as u64 {
            text.push_str(" (call listing truncated)");
        }
        text
    };
    if run["partial"] == true {
        text.push_str("; partial");
    }
    text.push('.');
    text
}

fn span(window: &Value) -> String {
    let parse = |name: &str| {
        window["requested"][name]
            .as_str()?
            .parse::<jiff::Timestamp>()
            .ok()
    };
    let Some(ms) = parse("to")
        .zip(parse("from"))
        .and_then(|(to, from)| to.as_millisecond().checked_sub(from.as_millisecond()))
        .filter(|ms| *ms > 0)
    else {
        return "in the requested window".into();
    };
    if ms % 3_600_000 == 0 {
        format!("in the last {} h", ms / 3_600_000)
    } else if ms % 60_000 == 0 {
        format!("in the last {} min", ms / 60_000)
    } else {
        format!("in the last {} s", ms / 1_000)
    }
}

fn dry_run(report: &Value) -> Vec<String> {
    if let Some(error) = report["error"].as_str() {
        return vec![format!("Dry run could not be completed: {}", short(error))];
    }
    let runs = items(&report["runs"]);
    let window = &report["window"];
    let description = match window["kind"].as_str() {
        Some("schedule") => {
            let replayed = window["replayed"].as_u64().unwrap_or(runs.len() as u64);
            let due = window["due_times_in_window"].as_u64().unwrap_or(replayed);
            let lower_bound = window["due_times_is_lower_bound"] == true;
            let mut description = format!(
                "{replayed} of {}{due} scheduled runs {} replayed. Replay {}capped.",
                if lower_bound { "at least " } else { "" },
                span(window),
                if window["capped"] == true { "" } else { "not " }
            );
            if lower_bound {
                description.push_str(" Schedule scan capped; the due count is a lower bound.");
            }
            description
        }
        Some("events") => "event window; no events were replayed because event replay needs the event plane. No replay window was covered.".into(),
        Some("synthetic") => format!("no replay window; {} sample-trigger run{} replayed.", runs.len(), plural(runs.len() as u64)),
        Some("at") => format!("one-time schedule at {}; {} run{} replayed.", field(window, "at"), runs.len(), plural(runs.len() as u64)),
        _ => format!("no replay window; {} run{} replayed.", runs.len(), plural(runs.len() as u64)),
    };
    let mut lines = vec![format!(
        "Dry run ({} mode): {description}",
        field(report, "mode")
    )];
    if report["partial"] == true {
        lines.push("Partial: captured results may leave later behavior untested.".into());
    }
    // Run IDs, trigger times and call arguments differ between equivalent outcomes.
    let mut groups: Vec<(String, usize)> = Vec::new();
    for run in runs {
        let text = outcome(run);
        if let Some((_, count)) = groups.iter_mut().find(|(existing, _)| *existing == text) {
            *count += 1;
        } else {
            groups.push((text, 1));
        }
    }
    for (text, count) in groups.iter().take(MAX_OUTCOMES) {
        lines.push(format!(
            "{count} {}: {text}",
            if *count == 1 { "run" } else { "runs" }
        ));
    }
    if groups.len() > MAX_OUTCOMES {
        let omitted: usize = groups[MAX_OUTCOMES..].iter().map(|(_, count)| count).sum();
        lines.push(format!(
            "{omitted} more runs across {} other outcomes; see structuredContent for details.",
            groups.len() - MAX_OUTCOMES
        ));
    }
    lines
}

fn warnings(lines: &mut Vec<String>, sources: &[&Value]) {
    let mut seen = Vec::new();
    for warning in sources.iter().flat_map(|source| items(source)) {
        let texts = if let Some(message) = warning.as_str() {
            vec![short(message)]
        } else {
            let context = if warning["module"].is_string() {
                format!("{} {}", field(warning, "module"), field(warning, "op"))
            } else {
                warning["path"]
                    .as_str()
                    .or_else(|| warning["host"].as_str())
                    .unwrap_or("")
                    .into()
            };
            let context = if context.is_empty() {
                String::new()
            } else {
                format!("; {}", short(&context))
            };
            // A warning's text may hold several lines, such as one per
            // operating system; each becomes its own warning line.
            let text = warning["text"]
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .unwrap_or("No description available.");
            text.split(['\n', '\r'])
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| format!("{}{}: {}", field(warning, "kind"), context, line))
                .collect()
        };
        for text in texts {
            if !seen.contains(&text) {
                lines.push(format!("Warning: {text}"));
                seen.push(text);
            }
        }
    }
}

fn version(value: &Value) -> String {
    value
        .as_u64()
        .map_or_else(|| "none".into(), |version| version.to_string())
}

fn timestamp(value: &Value) -> String {
    value
        .as_i64()
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map_or_else(|| "unknown time".into(), |stamp| stamp.to_string())
}

pub(super) fn summary(action: &str, result: &Value) -> String {
    let id = field(result, "flow_id");
    let mut lines = match action {
        "install" => {
            let approval = match result["card_id"].as_str() {
                Some(card) => format!("The approval card is on the operator's phone ({card})."),
                None if result["approval"]["kind"] == "policy" => {
                    "Approved by install policy; no approval card is needed.".into()
                }
                None => "No approval card is available.".into(),
            };
            let mut lines = vec![
                format!(
                    "Install requested for {id} v{}. {approval}",
                    version(&result["version"])
                ),
                format!("Code hash (full): {}.", field(result, "code_hash")),
            ];
            lines.extend(dry_run(&result["dry_run"]));
            warnings(
                &mut lines,
                &[&result["warnings"], &result["dry_run"]["warnings"]],
            );
            lines
        }
        "dry_run" => {
            let mut lines = vec![format!(
                "Preview for {id} v{}.",
                version(&result["version"])
            )];
            lines.extend(dry_run(&result["summary"]));
            if result["state_reset"] == true {
                lines.push("The declared state epoch changed; a live run would reset state. This preview did not clear it.".into());
            }
            warnings(
                &mut lines,
                &[&result["warnings"], &result["summary"]["warnings"]],
            );
            lines
        }
        "list" => items(&result["flows"])
            .iter()
            .map(|flow| {
                let last = if flow["last_run"].is_object() {
                    format!(
                        "{} ({})",
                        field(&flow["last_run"], "state"),
                        field(&flow["last_run"], "run_id")
                    )
                } else {
                    "none".into()
                };
                format!(
                    "{}: {}; approved version {}; last run {last}",
                    field(flow, "flow_id"),
                    field(flow, "state"),
                    version(&flow["approved_version"])
                )
            })
            .collect(),
        "enable" | "disable" => vec![format!(
            "{id}: {}{}.",
            field(result, "state"),
            if result["changed"] == false {
                " (unchanged)"
            } else {
                ""
            }
        )],
        "instantiate" => vec![format!(
            "Instance {id} {} from {} v{}; expires {}.",
            if result["new"] == true {
                "created"
            } else {
                "extended"
            },
            field(result, "template"),
            version(&result["version"]),
            timestamp(&result["expires_at"])
        )],
        "end" => vec![format!(
            "Instance {id}: {}.",
            if result["ended"] == true {
                "ended"
            } else {
                "already ended"
            }
        )],
        _ => vec!["Flow action completed.".into()],
    };
    if lines.is_empty() {
        lines.push("No flows.".into());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests;
