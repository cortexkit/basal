//! A dry run's result in plain words, for an install card read on a phone.
//!
//! The install card carries the dry run twice: as the raw summary the dry
//! runner returns (kept for audits, unchanged) and as the short text built
//! here, which core and cingulate show verbatim. [`render`] reads only the
//! raw summary and the manifest it was run for; it has no other inputs, so
//! the same summary always gives the same text.
//!
//! Every string it returns is bounded (see the `*_MAX_*` constants), has no
//! control characters, and is cut on a char boundary. Text the flow's
//! script supplied (agent names, digest titles, error messages, paths) is
//! always put in single quotes and shortened, so it never reads as basal's
//! own words.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::Manifest;
use crate::schedule::spec::{self, When};

/// The rejection code a dry run answers a captured call with.
pub const CAPTURED_CODE: &str = "captured";
/// The message of a captured call's rejection. A script that does not catch
/// it fails with `HostError: ` followed by this text, which is how a run
/// that stopped at a captured call is told apart from a real failure.
pub const CAPTURED_MESSAGE: &str = "captured by a dry run: not executed";

/// The longest `summary`, in bytes.
pub const SUMMARY_MAX_BYTES: usize = 300;
/// The most `would` lines; when there are more, the last says how many
/// were left out.
pub const WOULD_MAX_ITEMS: usize = 8;
/// The longest `would` line, in bytes.
pub const WOULD_ITEM_MAX_BYTES: usize = 200;
/// The most `problems` lines; when there are more, the last says how many
/// were left out.
pub const PROBLEMS_MAX_ITEMS: usize = 8;
/// The longest `problems` line, in bytes. Cingulate refuses a card with a
/// longer one, so this is a hard limit, not a target.
pub const PROBLEM_ITEM_MAX_BYTES: usize = 200;
/// The longest piece of script-supplied text inside a line, in bytes,
/// before its quotes.
const QUOTED_MAX_BYTES: usize = 80;
const ELLIPSIS: &str = "…";

/// Added to the summary when every run stopped at a captured read.
pub const EACH_RUN_STOPPED: &str = "Each run stopped at its first read, because a flow that isn't approved yet doesn't read live data.";
/// Added to the summary when only some runs stopped at a captured read.
pub const SOME_RUNS_STOPPED: &str = "Some runs stopped at their first read, because a flow that isn't approved yet doesn't read live data.";

/// The plain-language dry run, as sent to core in `dry_run_text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DryRunText {
    /// When the flow runs and how the dry run went, in one or two sentences.
    pub summary: String,
    /// What the flow would do, one distinct action per line.
    pub would: Vec<String>,
    /// Real failures only: never the refusals the dry run itself made.
    pub problems: Vec<String>,
}

/// Turns a dry run's raw summary (the `summary` of `flow.dry_run`, which the
/// install card keeps as `dry_run_summary`, or `{"error": …}` when the dry
/// run could not run) into plain text, for the manifest it was run against.
pub fn render(manifest: &Manifest, summary: &Value) -> DryRunText {
    if let Some(error) = summary.get("error").and_then(Value::as_str) {
        return DryRunText {
            summary: bounded(
                "The dry run could not be completed, so this card can't show what the flow would do.",
                SUMMARY_MAX_BYTES,
            ),
            would: Vec::new(),
            problems: capped(
                vec![format!("The dry run failed: {}.", quote(error))],
                PROBLEMS_MAX_ITEMS,
                PROBLEM_ITEM_MAX_BYTES,
            ),
        };
    }
    let runs: &[Value] = summary
        .get("runs")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let stopped = runs.iter().filter(|r| stopped_at_captured_read(r)).count();
    let stop_sentence = match stopped {
        0 => None,
        n if n == runs.len() => Some(EACH_RUN_STOPPED),
        _ => Some(SOME_RUNS_STOPPED),
    };
    // The stop sentence is fixed text and always fits; the trigger sentence
    // takes what is left, so cutting it never cuts the stop sentence.
    let room = SUMMARY_MAX_BYTES - stop_sentence.map_or(0, |s| s.len() + 1);
    let mut text = bounded(&trigger_sentence(manifest, &summary["window"], runs), room);
    if let Some(sentence) = stop_sentence {
        text.push(' ');
        text.push_str(sentence);
    }
    DryRunText {
        summary: bounded(&text, SUMMARY_MAX_BYTES),
        would: capped(would(manifest, runs), WOULD_MAX_ITEMS, WOULD_ITEM_MAX_BYTES),
        problems: capped(
            problems(manifest, runs),
            PROBLEMS_MAX_ITEMS,
            PROBLEM_ITEM_MAX_BYTES,
        ),
    }
}

// ---- The summary --------------------------------------------------------

fn trigger_sentence(manifest: &Manifest, window: &Value, runs: &[Value]) -> String {
    match window["kind"].as_str() {
        Some("schedule") => schedule_sentence(manifest, window, runs),
        Some("at") => match window["at"].as_str().and_then(time_words) {
            Some(at) => format!("Runs once, at {at}; the dry run ran it ahead of time."),
            None => "Runs once, at a set time; the dry run ran it ahead of time.".into(),
        },
        Some("events") => events_sentence(manifest),
        Some("synthetic") => "The dry run ran the flow once, on a test trigger.".into(),
        _ => format!("The dry run ran the flow {}.", times(runs.len() as u64)),
    }
}

fn events_sentence(manifest: &Manifest) -> String {
    let events = manifest.trigger.events.as_deref().unwrap_or_default();
    let mut modules: Vec<(&str, Vec<&str>)> = Vec::new();
    for event in events {
        match modules.iter_mut().find(|(m, _)| *m == event.module) {
            Some((_, names)) if !names.contains(&event.name.as_str()) => names.push(&event.name),
            Some(_) => {}
            None => modules.push((&event.module, vec![&event.name])),
        }
    }
    if modules.is_empty() {
        return "Runs on events; no events were replayed in the dry run.".into();
    }
    let groups: Vec<String> = modules
        .iter()
        .map(|(module, names)| format!("{module} {} events", and_list(names)))
        .collect();
    let mut sentence = format!(
        "Runs on {}; no events were replayed in the dry run.",
        and_list(&groups)
    );
    let ages: Vec<_> = events
        .iter()
        .map(|e| e.max_age.as_deref().and_then(crate::manifest::duration_ms))
        .collect();
    if let Some(age) = ages
        .first()
        .copied()
        .flatten()
        .filter(|age| ages.iter().all(|a| *a == Some(*age)))
    {
        sentence.push_str(&format!(" Ignores events older than {}.", span_words(age)));
    } else {
        for (event, age) in events.iter().zip(ages) {
            if let Some(age) = age {
                sentence.push_str(&format!(
                    " Ignores {} {} v{} events older than {}.",
                    event.module,
                    event.name,
                    event.version,
                    span_words(age)
                ));
            }
        }
    }
    sentence
}

fn schedule_sentence(manifest: &Manifest, window: &Value, runs: &[Value]) -> String {
    let span = match (
        window["requested"]["from"].as_str().and_then(timestamp),
        window["requested"]["to"].as_str().and_then(timestamp),
    ) {
        (Some(from), Some(to)) => {
            format!(
                "In the last {}",
                span_words(to.as_millisecond() - from.as_millisecond())
            )
        }
        _ => "In the window replayed".into(),
    };
    let count = window["due_times_in_window"].as_u64().unwrap_or(0);
    let replayed = window["replayed"].as_u64().unwrap_or(runs.len() as u64);
    let often = how_often(manifest, runs);
    if count == 0 {
        let often = if let Some(schedule) = often.strip_prefix("on the cron schedule") {
            format!("on its cron schedule{schedule}")
        } else {
            often
        };
        return format!("{span} it would not have run {often}, so the dry run had nothing to try.");
    }
    let at_least = if window["due_times_is_lower_bound"].as_bool() == Some(true) {
        "at least "
    } else {
        ""
    };
    let tried = if window["capped"].as_bool() == Some(true) {
        format!("the dry run tried the newest {replayed}")
    } else if count == 1 {
        "the dry run tried that run".into()
    } else {
        "the dry run tried all of them".into()
    };
    format!(
        "{span} it would have run {at_least}{}, {often}; {tried}.",
        times(count)
    )
}

/// How often the schedule fires, in words: an interval as written, a cron
/// pattern by the gap between the replayed due times when they are evenly
/// spaced, and always with the pattern itself.
fn how_often(manifest: &Manifest, runs: &[Value]) -> String {
    let Some(schedule) = &manifest.trigger.schedule else {
        return "on its schedule".into();
    };
    match spec::validate(schedule).map(|c| c.when) {
        Ok(When::Interval { every_secs }) => every_words(every_secs.saturating_mul(1000) as i64),
        Ok(When::Cron { pattern, zone, .. }) => {
            let dues: Vec<i64> = runs
                .iter()
                .filter_map(|r| r["trigger"]["due"].as_str().and_then(timestamp))
                .map(|t| t.as_millisecond())
                .collect();
            let gaps: BTreeSet<i64> = dues.windows(2).map(|w| w[1] - w[0]).collect();
            let zone = if zone.is_empty() { "UTC" } else { &zone };
            match gaps.iter().next() {
                Some(gap) if gaps.len() == 1 && *gap > 0 => {
                    format!("{} (cron {} {zone})", every_words(*gap), quote(&pattern))
                }
                _ => format!("on the cron schedule {} ({zone})", quote(&pattern)),
            }
        }
        Ok(When::At { at }) => format!("once, at {}", stamp_words(at)),
        Err(_) => "on its schedule".into(),
    }
}

const SECOND: i64 = 1000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// A window's length: "24 hours", "7 days", "90 minutes".
fn span_words(ms: i64) -> String {
    let (n, unit) = if ms % DAY == 0 && ms > 2 * DAY {
        (ms / DAY, "day")
    } else if ms % HOUR == 0 && ms >= HOUR {
        (ms / HOUR, "hour")
    } else if ms % MINUTE == 0 && ms >= MINUTE {
        (ms / MINUTE, "minute")
    } else {
        (ms / SECOND, "second")
    };
    plural(n, unit)
}

/// A period: "every 5 minutes", "every hour", "every day".
fn every_words(ms: i64) -> String {
    let (n, unit) = if ms % DAY == 0 {
        (ms / DAY, "day")
    } else if ms % HOUR == 0 {
        (ms / HOUR, "hour")
    } else if ms % MINUTE == 0 {
        (ms / MINUTE, "minute")
    } else {
        (ms / SECOND, "second")
    };
    if n == 1 {
        format!("every {unit}")
    } else {
        format!("every {}", plural(n, unit))
    }
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

fn times(n: u64) -> String {
    match n {
        1 => "once".into(),
        2 => "twice".into(),
        n => format!("{n} times"),
    }
}

fn and_list<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_owned(),
        [rest @ .., last] => format!(
            "{} and {}",
            rest.iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

fn timestamp(text: &str) -> Option<jiff::Timestamp> {
    text.parse().ok()
}

fn stamp_words(t: jiff::Timestamp) -> String {
    t.strftime("%Y-%m-%d %H:%M UTC").to_string()
}

fn time_words(text: &str) -> Option<String> {
    timestamp(text).map(stamp_words)
}

// ---- How runs ended -----------------------------------------------------

fn calls(run: &Value) -> &[Value] {
    run.get("calls")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The rejection code of a listed call's outcome, if it was rejected.
fn rejection(call: &Value) -> Option<&str> {
    call["outcome"]["rejected"].as_str()
}

fn is_captured(call: &Value) -> bool {
    call["action"] == "captured" || rejection(call) == Some(CAPTURED_CODE)
}

/// The captured call a run stopped at, when it failed only because its
/// script did not catch a captured call's rejection: the error is the
/// `HostError` the prelude builds from [`CAPTURED_MESSAGE`], and the run
/// holds a captured call for it to come from. Any other failure, including
/// one whose message merely mentions capture, is not a capture stop.
fn capture_stop(run: &Value) -> Option<&Value> {
    if run["state"] != "failed" || run["error"]["kind"] != "script" {
        return None;
    }
    let detail = run["error"]["detail"].as_str()?;
    let first_line = detail.lines().next()?;
    if first_line != format!("HostError: {CAPTURED_MESSAGE}") {
        return None;
    }
    calls(run).iter().rev().find(|c| is_captured(c))
}

/// Whether a call changes something or asks a model, rather than reading.
/// Module ops are taken as reads: the summary does not say which ones a
/// module marks as mutations.
fn is_write(call: &Value) -> bool {
    match call["kind"].as_str() {
        Some(
            "sink.digest"
            | "sink.status"
            | "sink.board"
            | "sink.wait.resolve"
            | "sink.room.post"
            | "fs.write"
            | "instance.end" | "llm" | "classify" | "sh" | "kv.set" | "kv.delete"
            | "unsupported_tool",
        ) => true,
        Some("net.fetch") => !matches!(fetch_method(call).as_str(), "GET" | "HEAD"),
        _ => false,
    }
}

fn stopped_at_captured_read(run: &Value) -> bool {
    capture_stop(run).is_some_and(|call| !is_write(call))
}

/// When a run was due, for naming it in a problem.
fn run_name(run: &Value) -> String {
    match run["trigger"]["due"].as_str().and_then(time_words) {
        Some(at) => format!("The run at {at}"),
        None if run["trigger_id"]
            .as_str()
            .is_some_and(|t| t.starts_with("dry-run:")) =>
        {
            "The test run".into()
        }
        None => "A run".into(),
    }
}

// ---- What the flow would do ---------------------------------------------

/// The lines of `would`, distinct and in the order the flow first did each
/// thing. `kv.set` calls are folded into one line counting distinct keys.
fn would(manifest: &Manifest, runs: &[Value]) -> Vec<String> {
    enum Line {
        Text(String),
        Stored,
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut stored_keys = BTreeSet::new();
    let mut unnamed_keys = 0usize;
    for call in runs.iter().flat_map(calls) {
        // A call basal refused is a problem, not something the flow does.
        if rejection(call).is_some_and(|code| code != CAPTURED_CODE) {
            continue;
        }
        if call["kind"] == "kv.set" {
            match text_field(&call["args"], &["key"]) {
                Some(key) => {
                    stored_keys.insert(key);
                }
                None => unnamed_keys += 1,
            }
            if seen.insert("kv.set".to_owned()) {
                lines.push(Line::Stored);
            }
            continue;
        }
        if let Some(text) = describe(manifest, call)
            && seen.insert(text.clone())
        {
            lines.push(Line::Text(text));
        }
    }
    let stored = stored_keys.len() + unnamed_keys;
    lines
        .into_iter()
        .map(|line| match line {
            Line::Text(text) => text,
            Line::Stored if stored == 1 => "Store 1 value".into(),
            Line::Stored => format!("Store {stored} values"),
        })
        .collect()
}

/// One call in words, or `None` for calls that are not worth a line (clock
/// and random reads).
fn describe(manifest: &Manifest, call: &Value) -> Option<String> {
    let args = &call["args"];
    // A built-in's journaled request wraps the script's arguments with the
    // grant they were checked against.
    let inner = if args.get("grant").is_some() {
        &args["args"]
    } else {
        args
    };
    Some(match call["kind"].as_str()? {
        "op" => describe_op(manifest, call),
        "facts" => match text_field(args, &["agent"]) {
            Some(agent) => format!("Read facts about {}", quote(&agent)),
            None => "Read facts about an agent".into(),
        },
        "classify" => "Ask a model to classify text".into(),
        "llm" => match &manifest.llm {
            Some(llm) => format!("Ask a model (iq {})", llm.iq),
            None => "Ask a model".into(),
        },
        "sink.digest" => {
            let agent = call["sink"]["agent"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| text_field(args, &["agent"]))
                .unwrap_or_default();
            if args["withdraw"] == true {
                format!("Withdraw the pending digest item for {}", quote(&agent))
            } else {
                let lead = if call["sink"]["effective"] == "wake" {
                    format!("Wake {}", quote(&agent))
                } else {
                    format!("Add a digest item for {}", quote(&agent))
                };
                match text_field(args, &["item", "title"]) {
                    Some(title) => format!("{lead}: {}", quote(&title)),
                    None => lead,
                }
            }
        }
        "sink.wait.resolve" => format!(
            "Resolve its board waits as {}",
            quote(&string_or(args, "outcome"))
        ),
        "sink.room.post" => match text_field(args, &["room_id"]) {
            Some(room) => {
                let mentioned = call["args"]["mentions"]
                    .as_array()
                    .filter(|ids| !ids.is_empty());
                match mentioned {
                    Some(ids) => {
                        let names: Vec<String> = ids
                            .iter()
                            .filter_map(Value::as_str)
                            .map(quote)
                            .collect();
                        format!(
                            "Post to the room {} mentioning {}",
                            quote(&room),
                            and_list(&names)
                        )
                    }
                    None => format!("Post to the room {}", quote(&room)),
                }
            }
            None => "Post to one of its rooms".into(),
        },
        "sink.board" => {
            let agent = text_field(args, &["agent"]).unwrap_or_default();
            match text_field(args, &["operation", "op"]).as_deref() {
                Some("append") => format!("Add an item to its board lane for {}", quote(&agent)),
                Some("set_state") => format!(
                    "Set an item's state in its board lane for {}",
                    quote(&agent)
                ),
                Some("retire") => format!("Retire its board lane for {}", quote(&agent)),
                _ => format!("Write its board lane for {}", quote(&agent)),
            }
        }
        "sink.status" => match text_field(args, &["agent"]) {
            Some(agent) => format!("Set status for {}", quote(&agent)),
            None => "Set a status line".into(),
        },
        "kv.get" => "Read stored values".into(),
        "kv.set" => "Store a value".into(),
        "kv.delete" => "Delete stored values".into(),
        "fs.read" | "fs.list" | "fs.stat" => format!("Read files under {}", quote(&fs_root(args))),
        "fs.write" => format!("Write files under {}", quote(&fs_root(args))),
        "git.show" => format!(
            "Read a file from git in {}",
            quote(&string_or(inner, "repo"))
        ),
        "git.diff" => format!("Read a git diff in {}", quote(&string_or(inner, "repo"))),
        kind if kind.starts_with("git.") => {
            format!("Read git history in {}", quote(&string_or(inner, "repo")))
        }
        "net.fetch" => {
            let host = quote(&url_host(&string_or(inner, "url")));
            match fetch_method(call).as_str() {
                "GET" | "HEAD" => format!("Make an HTTPS request to {host}"),
                method => format!("Make an HTTPS {method} request to {host}"),
            }
        }
        "instance.end" => "End this flow instance, so it doesn't run again".into(),
        "sh" => "Run a shell command".into(),
        "unsupported_tool" => "Call a tool".into(),
        _ => return None,
    })
}

fn describe_op(manifest: &Manifest, call: &Value) -> String {
    let module = sanitize(call["module"].as_str().unwrap_or("a module"));
    let op = sanitize(call["op"].as_str().unwrap_or("an op"));
    let args = &call["args"];
    // A call made for an agent of the audience (`ops.call(…, {for})`) is
    // journaled as `{for, args}`.
    let target = args
        .as_object()
        .filter(|o| o.len() == 2 && o.contains_key("args"))
        .and_then(|o| o.get("for"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if module == basal_host::subc_catalog::CORE && op == "board.read" {
        let agent = target.or_else(|| text_field(args, &["agent_id"]));
        return match agent {
            Some(agent) => format!("Read {}'s board", quote(&agent)),
            None if manifest.audience.is_some() => "Read every board in its audience".into(),
            None => format!("Read boards from {module}"),
        };
    }
    if op == basal_host::catalog::EVENT_BODY_OP {
        return format!("Fetch an event body from {module}");
    }
    match target {
        Some(agent) => format!("Call {module} {op} for {}", quote(&agent)),
        None => format!("Call {module} {op}"),
    }
}

fn fetch_method(call: &Value) -> String {
    let args = &call["args"];
    let inner = if args.get("grant").is_some() {
        &args["args"]
    } else {
        args
    };
    let method = inner["options"]["method"]
        .as_str()
        .unwrap_or("GET")
        .to_ascii_uppercase();
    // Only the methods the built-in accepts are named; anything else would
    // be the script's own text.
    match method.as_str() {
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" => method,
        _ => "GET".into(),
    }
}

/// The granted root a file call's path lies under, or the path itself when
/// none matches as written.
fn fs_root(args: &Value) -> String {
    let path = string_or(&args["args"], "path");
    let roots = args["grant"]["roots"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    roots
        .iter()
        .filter_map(Value::as_str)
        .find(|root| {
            path == *root
                || path
                    .strip_prefix(*root)
                    .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
        })
        .map_or(path.clone(), str::to_owned)
}

/// The host of an `https://` URL, or the URL as given.
fn url_host(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    host.split(':').next().unwrap_or(host).to_owned()
}

fn string_or(value: &Value, field: &str) -> String {
    value[field].as_str().unwrap_or("").to_owned()
}

/// A string field of a call's arguments, at `path`. Arguments too large to
/// list are only a prefix of their JSON (`{truncated, bytes}`); from those,
/// the last field of `path` is read where it first appears, if it appears
/// whole.
fn text_field(args: &Value, path: &[&str]) -> Option<String> {
    if let Some(prefix) = args.get("truncated").and_then(Value::as_str)
        && args.get("bytes").is_some()
    {
        let name = path.last()?;
        let from = prefix.find(&format!("\"{name}\":"))? + name.len() + 3;
        let mut values =
            serde_json::Deserializer::from_str(prefix[from..].trim_start()).into_iter::<String>();
        return values.next()?.ok();
    }
    let mut value = args;
    for field in path {
        value = value.get(field)?;
    }
    value.as_str().map(str::to_owned)
}

// ---- Problems -----------------------------------------------------------

/// Real failures, one sentence each, in run order. A captured call's
/// rejection is the dry run's own doing and is never listed. Nor is a script
/// error raised after the script received a captured rejection, because the
/// fake rejection may be what caused it.
fn problems(manifest: &Manifest, runs: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |line: String| {
        if !out.contains(&line) {
            out.push(line);
        }
    };
    for run in runs {
        let name = run_name(run);
        if let Some(admission) = run.get("admission") {
            push(format!(
                "{name} wasn't started: {}.",
                quote(admission.as_str().unwrap_or("refused"))
            ));
            continue;
        }
        let mut refused = false;
        for call in calls(run) {
            if let Some(code) = rejection(call).filter(|code| *code != CAPTURED_CODE) {
                refused = true;
                let what = describe(manifest, call).unwrap_or_else(|| "a call".into());
                push(format!(
                    "{name} had a call refused ({}): {what}.",
                    sanitize(code)
                ));
            }
        }
        let detail = run["error"]["detail"].as_str().unwrap_or("");
        let first_line = detail.lines().next().unwrap_or("");
        match (run["state"].as_str(), run["error"]["kind"].as_str()) {
            (Some("succeeded"), _) => {}
            (Some("failed"), _) if capture_stop(run).is_some() => {}
            // The script threw the refused call's own rejection, and the refusal is
            // already listed above as this run's problem.
            (Some("failed"), Some("script")) if refused && first_line.starts_with("HostError:") => {
            }
            (Some("failed"), Some("script")) if run["partial"] == true => {}
            (Some("failed"), Some("script")) => {
                push(format!(
                    "{name} failed: the script threw {}.",
                    quote(first_line)
                ));
            }
            (Some("failed"), Some("deadline")) => {
                push(format!("{name} failed: it ran past its deadline."));
            }
            (Some("failed"), Some("stalled")) => push(format!(
                "{name} failed: it waited for something that never happened."
            )),
            (Some("failed"), Some("budget_exhausted")) => push(format!(
                "{name} failed: it ran out of its {} budget.",
                match first_line {
                    "Cpu" => "CPU",
                    "Memory" => "memory",
                    "Stack" => "stack",
                    _ => "time or memory",
                }
            )),
            (Some("failed"), kind) => push(format!(
                "{name} failed with a {} error: {}.",
                sanitize(kind.unwrap_or("basal")),
                quote(first_line)
            )),
            (state, _) => push(format!(
                "{name} didn't finish (it was left {}).",
                sanitize(state.unwrap_or("unknown"))
            )),
        }
    }
    out
}

// ---- Bounds and quoting -------------------------------------------------

/// Whether a character is dropped from every line: control characters and
/// the invisible marks that reorder or break text.
fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200E}' | '\u{200F}' | '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Text with control characters replaced by spaces and runs of whitespace
/// collapsed, so a script's newlines and tabs read as one space.
fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        let c = if invisible(c) { ' ' } else { c };
        if c.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

/// At most `max` bytes, cut on a char boundary with "…" when cut.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max.saturating_sub(ELLIPSIS.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", text[..end].trim_end())
}

fn bounded(text: &str, max: usize) -> String {
    cut(&sanitize(text), max)
}

/// Script-supplied text, quoted and shortened. A straight quote inside it
/// becomes a typographic one, so the text cannot end its own quotes early.
fn quote(text: &str) -> String {
    let clean = sanitize(text).replace('\'', "\u{2019}");
    format!("'{}'", cut(&clean, QUOTED_MAX_BYTES))
}

/// At most `max_items` lines of at most `max_bytes` each; when there are
/// more lines, the last one kept says how many were left out.
fn capped(lines: Vec<String>, max_items: usize, max_bytes: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        let line = bounded(&line, max_bytes);
        if !line.is_empty() && !out.contains(&line) {
            out.push(line);
        }
    }
    if out.len() > max_items {
        let kept = max_items - 1;
        let more = out.len() - kept;
        out.truncate(kept);
        out.push(format!("and {more} more"));
    }
    out
}

#[cfg(test)]
#[path = "dry_run_text_tests.rs"]
mod tests;
