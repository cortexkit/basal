//! Fixtures here have the shape the dry runner writes (see `flow.dry_run` in
//! docs/ops.md): runs with their trigger, state, error and listed calls.
use super::*;
use serde_json::json;

fn manifest(value: Value) -> Manifest {
    Manifest::parse(&value.to_string()).unwrap()
}

fn board_manifest() -> Manifest {
    manifest(
        json!({"id":"board-hygiene","version":2,"purpose":"Nudge board owners",
        "trigger":{"schedule":{"cron":"*/5 * * * *","tz":"UTC","missed":"skip"}},
        "audience":{"kind":"global"},
        "ops":[{"module":"prefrontal-core","op":"board.read"}],
        "board_read":["$audience"],
        "facts":{"targets":["$audience"]},
        "sinks":[{"agent":"$audience","digest_max":"wake"}]}),
    )
}

fn hourly_manifest() -> Manifest {
    manifest(
        json!({"id":"morning","version":1,"purpose":"Summarise the night",
        "trigger":{"schedule":{"interval":"1h"}},
        "facts":{"targets":["SYNAPSE"]},
        "sinks":[{"agent":"SYNAPSE","digest_max":"wake"}]}),
    )
}

/// A schedule window of 24 hours ending at 2026-05-02T00:00:00Z.
fn day_window(count: u64, replayed: u64) -> Value {
    json!({"kind":"schedule",
        "requested":{"from":"2026-05-01T00:00:00Z","to":"2026-05-02T00:00:00Z"},
        "covered":{"from":"2026-05-01T23:15:00Z","to":"2026-05-02T00:00:00Z"},
        "due_times_in_window":count,"due_times_is_lower_bound":false,
        "replayed":replayed,"capped":count > replayed})
}

fn due(minutes_before_end: i64) -> String {
    jiff::Timestamp::from_millisecond(1_777_680_000_000 - minutes_before_end * 60_000)
        .unwrap()
        .to_string()
}

fn call(position: u64, kind: &str, args: Value, outcome: Value, action: &str) -> Value {
    json!({"position":position,"kind":kind,"module":null,"op":null,"args":args,
        "outcome":outcome,"action":action,"partial":false})
}

fn captured() -> Value {
    json!({"rejected":CAPTURED_CODE})
}

fn op_call(position: u64, module: &str, op: &str, args: Value) -> Value {
    let mut call = call(position, "op", args, captured(), "captured");
    call["module"] = json!(module);
    call["op"] = json!(op);
    call
}

fn run(due: &str, state: &str, error: Value, partial: bool, calls: Vec<Value>) -> Value {
    json!({"run_id":format!("run-{due}"),"trigger_id":format!("schedule:{due}"),
        "trigger":{"kind":"schedule","due":due},"state":state,"result":null,
        "error":error,"ending":null,"partial":partial,"calls_total":calls.len(),"calls":calls})
}

/// How the dry runner records a run whose script did not catch a captured
/// call's rejection.
fn capture_error() -> Value {
    json!({"kind":"script","detail":format!("HostError: {CAPTURED_MESSAGE}\n    at run (flow.js:142)")})
}

fn summary(window: Value, runs: Vec<Value>) -> Value {
    let partial = runs.iter().any(|r| r["partial"] == true);
    json!({"mode":"capture","window":window,"partial":partial,"runs":runs})
}

/// Every line of a `DryRunText` is within the published byte and line-count
/// limits and holds no control characters.
fn assert_bounded(text: &DryRunText) {
    assert!(text.summary.len() <= SUMMARY_MAX_BYTES, "{text:#?}");
    assert!(text.would.len() <= WOULD_MAX_ITEMS, "{text:#?}");
    assert!(text.problems.len() <= PROBLEMS_MAX_ITEMS, "{text:#?}");
    for line in &text.would {
        assert!(line.len() <= WOULD_ITEM_MAX_BYTES, "{line}");
    }
    for line in &text.problems {
        assert!(line.len() <= PROBLEM_ITEM_MAX_BYTES, "{line}");
    }
    for line in std::iter::once(&text.summary)
        .chain(&text.would)
        .chain(&text.problems)
    {
        assert!(!line.chars().any(invisible), "{line:?}");
    }
}

fn board_runs() -> Vec<Value> {
    (0..10)
        .rev()
        .map(|i| {
            run(
                &due(i * 5),
                "failed",
                capture_error(),
                true,
                vec![op_call(
                    0,
                    "prefrontal-core",
                    "board.read",
                    json!({"limit":16}),
                )],
            )
        })
        .collect()
}

#[test]
fn an_empty_cron_schedule_summary_does_not_nest_parentheses() {
    let scheduled = manifest(json!({
        "id":"annual",
        "version":1,
        "purpose":"Run annually",
        "trigger":{"schedule":{"cron":"0 0 1 1 *"}}
    }));
    let text = render(&scheduled, &summary(day_window(0, 0), vec![]));
    assert_bounded(&text);
    assert_eq!(
        text.summary,
        "In the last 24 hours it would not have run on its cron schedule '0 0 1 1 *' (UTC), so the dry run had nothing to try."
    );
}

#[test]
fn cron_runs_that_all_stop_at_their_first_read_say_so_and_report_no_problem() {
    let text = render(
        &board_manifest(),
        &summary(day_window(288, 10), board_runs()),
    );
    assert_bounded(&text);
    assert_eq!(
        text,
        DryRunText {
            summary: format!(
                "In the last 24 hours it would have run 288 times, every 5 minutes (cron '*/5 * * * *' UTC); the dry run tried the newest 10. {EACH_RUN_STOPPED}"
            ),
            would: vec!["Read every board in its audience".into()],
            problems: vec![],
        }
    );
}

#[test]
fn only_some_runs_stopping_at_a_read_says_some() {
    let mut runs = board_runs();
    runs[0]["state"] = json!("succeeded");
    runs[0]["error"] = Value::Null;
    let text = render(&board_manifest(), &summary(day_window(288, 10), runs));
    assert!(text.summary.ends_with(SOME_RUNS_STOPPED), "{text:#?}");
    assert!(text.problems.is_empty(), "{text:#?}");
}

#[test]
fn a_flow_that_completes_and_wakes_an_agent_lists_each_thing_once() {
    let runs: Vec<Value> = (0..2)
        .map(|i| {
            let mut digest = call(
                1,
                "sink.digest",
                json!({"agent":"SYNAPSE","item":{"title":"Morning summary","body":"…"},"action":"wake"}),
                captured(),
                "captured",
            );
            digest["sink"] = json!({"agent":"SYNAPSE","requested":"wake","cap":"wake","effective":"wake","policy":"not consulted"});
            run(
                &due(i * 60),
                "succeeded",
                Value::Null,
                false,
                vec![
                    call(0, "facts", json!({"agent":"SYNAPSE","options":null}), captured(), "captured"),
                    digest,
                    call(2, "now", Value::Null, json!({"fulfilled":true}), "local"),
                    call(3, "kv.set", json!({"key":"last","value":1}), json!({"fulfilled":true}), "local"),
                    call(4, "kv.set", json!({"key":format!("run:{i}"),"value":1}), json!({"fulfilled":true}), "local"),
                ],
            )
        })
        .collect();
    let text = render(&hourly_manifest(), &summary(day_window(24, 2), runs));
    assert_bounded(&text);
    assert_eq!(
        text,
        DryRunText {
            summary: "In the last 24 hours it would have run 24 times, every hour; the dry run tried the newest 2.".into(),
            would: vec![
                "Read facts about 'SYNAPSE'".into(),
                "Wake 'SYNAPSE': 'Morning summary'".into(),
                "Store 3 values".into(),
            ],
            problems: vec![],
        }
    );
}

#[test]
fn a_digest_below_wake_is_an_added_item_and_a_status_names_its_agent() {
    let mut digest = call(
        0,
        "sink.digest",
        json!({"agent":"SYNAPSE","item":{"title":"Two new posts"},"action":"piggyback"}),
        captured(),
        "captured",
    );
    digest["sink"] = json!({"agent":"SYNAPSE","requested":"piggyback","cap":"wake","effective":"piggyback","policy":"not consulted"});
    let status = call(
        1,
        "sink.status",
        json!({"agent":"SYNAPSE","value":"2 new"}),
        captured(),
        "captured",
    );
    let runs = vec![run(
        &due(0),
        "succeeded",
        Value::Null,
        false,
        vec![digest, status],
    )];
    let text = render(&hourly_manifest(), &summary(day_window(1, 1), runs));
    assert_eq!(
        text.would,
        [
            "Add a digest item for 'SYNAPSE': 'Two new posts'",
            "Set status for 'SYNAPSE'"
        ]
    );
    assert_eq!(
        text.summary,
        "In the last 24 hours it would have run once, every hour; the dry run tried that run."
    );
}

#[test]
fn a_script_that_throws_before_any_call_is_one_problem() {
    let runs = vec![run(
        &due(0),
        "failed",
        json!({"kind":"script","detail":"TypeError: cannot read property 'boards' of undefined\n    at run (flow.js:3)"}),
        false,
        vec![],
    )];
    let text = render(&hourly_manifest(), &summary(day_window(1, 1), runs));
    assert_bounded(&text);
    assert_eq!(
        text.problems,
        [
            "The run at 2026-05-02 00:00 UTC failed: the script threw 'TypeError: cannot read property \u{2019}boards\u{2019} of undefined'."
        ]
    );
    assert!(text.would.is_empty());
    assert!(!text.summary.contains("stopped"), "{}", text.summary);
}

#[test]
fn refused_calls_and_real_failures_are_problems_but_captured_ones_are_not() {
    let mut refused = op_call(1, "mock", "post", json!({}));
    refused["outcome"] = json!({"rejected":"op_not_in_manifest"});
    refused["action"] = json!("refused");
    let kv_limit = call(
        2,
        "kv.set",
        json!({"key":"k","value":"v"}),
        json!({"rejected":"kv_limit"}),
        "local",
    );
    let runs = vec![
        run(
            &due(60),
            "failed",
            json!({"kind":"script","detail":"HostError: op not in manifest"}),
            false,
            vec![op_call(0, "mock", "read", json!({})), refused],
        ),
        run(&due(30), "succeeded", Value::Null, false, vec![kv_limit]),
        run(
            &due(0),
            "failed",
            json!({"kind":"deadline","detail":"the run passed its wall-clock deadline"}),
            false,
            vec![],
        ),
    ];
    let text = render(&hourly_manifest(), &summary(day_window(3, 3), runs));
    assert_bounded(&text);
    assert_eq!(
        text.problems,
        [
            "The run at 2026-05-01 23:00 UTC had a call refused (op_not_in_manifest): Call mock post.",
            "The run at 2026-05-01 23:30 UTC had a call refused (kv_limit): Store a value.",
            "The run at 2026-05-02 00:00 UTC failed: it ran past its deadline.",
        ]
    );
    assert_eq!(text.would, ["Call mock read"]);
}

#[test]
fn a_captured_rejection_alone_never_becomes_a_problem() {
    // The script caught the captured read and returned: the run succeeded.
    let caught = run(
        &due(0),
        "succeeded",
        Value::Null,
        false,
        vec![op_call(
            0,
            "prefrontal-core",
            "board.read",
            json!({"limit":16}),
        )],
    );
    let text = render(&board_manifest(), &summary(day_window(1, 1), vec![caught]));
    assert!(text.problems.is_empty(), "{text:#?}");
    // A failure that only quotes the capture message, with no captured call
    // behind it, is the script's own error.
    let imitated = run(&due(0), "failed", capture_error(), false, vec![]);
    let text = render(
        &board_manifest(),
        &summary(day_window(1, 1), vec![imitated]),
    );
    assert_eq!(text.problems.len(), 1, "{text:#?}");
    assert!(!text.summary.contains("stopped"), "{text:#?}");
}

#[test]
fn an_event_triggered_flow_names_its_events_and_says_none_were_replayed() {
    let m = manifest(
        json!({"id":"watch","version":1,"purpose":"Watch pull requests",
        "trigger":{"events":[
            {"module":"plexus","name":"github_pr_changed","version":1},
            {"module":"plexus","name":"github_issue_changed","version":1}]},
        "sinks":[{"agent":"SYNAPSE","digest_max":"wake"}]}),
    );
    let window = json!({"kind":"events","covered":null,"note":"event replay needs the event plane; no events were replayed"});
    let text = render(&m, &summary(window, vec![]));
    assert_eq!(
        text,
        DryRunText {
            summary: "Runs on plexus github_pr_changed and github_issue_changed events; no events were replayed in the dry run.".into(),
            would: vec![],
            problems: vec![],
        }
    );
}

#[test]
fn many_distinct_calls_are_cut_to_eight_lines_ending_in_and_n_more() {
    let calls: Vec<Value> = (0..12)
        .map(|i| op_call(i, "mock", &format!("op_{i}"), json!({})))
        .chain([op_call(12, "mock", "op_0", json!({}))])
        .collect();
    let runs = vec![run(&due(0), "succeeded", Value::Null, false, calls)];
    let text = render(&hourly_manifest(), &summary(day_window(1, 1), runs));
    assert_bounded(&text);
    assert_eq!(text.would.len(), WOULD_MAX_ITEMS);
    assert_eq!(text.would[0], "Call mock op_0");
    assert_eq!(text.would[6], "Call mock op_6");
    assert_eq!(text.would[7], "and 5 more");
}

#[test]
fn long_script_text_is_quoted_cut_on_a_char_boundary_and_bounded() {
    let title = "é".repeat(300);
    let mut digest = call(
        0,
        "sink.digest",
        json!({"agent":"SYNAPSE","item":{"title":title},"action":"wake"}),
        captured(),
        "captured",
    );
    digest["sink"] = json!({"agent":"SYNAPSE","effective":"wake"});
    let error = json!({"kind":"script","detail":"ü".repeat(1000)});
    let runs = vec![run(&due(0), "failed", error, false, vec![digest])];
    let text = render(&hourly_manifest(), &summary(day_window(1, 1), runs));
    assert_bounded(&text);
    let line = &text.would[0];
    assert!(line.starts_with("Wake 'SYNAPSE': 'éé"), "{line}");
    assert!(line.ends_with("…'"), "{line}");
    assert!(text.problems[0].contains("ü…'"), "{:?}", text.problems);
}

#[test]
fn a_digest_whose_arguments_were_too_large_to_list_still_shows_its_title() {
    let mut digest = call(
        0,
        "sink.digest",
        json!({"truncated":"{\"agent\":\"SYNAPSE\",\"item\":{\"title\":\"Wait ended: deploy\",\"body\":\"long","bytes":9000}),
        captured(),
        "captured",
    );
    digest["sink"] = json!({"agent":"SYNAPSE","effective":"wake"});
    let runs = vec![run(&due(0), "succeeded", Value::Null, false, vec![digest])];
    let text = render(&hourly_manifest(), &summary(day_window(1, 1), runs));
    assert_eq!(text.would, ["Wake 'SYNAPSE': 'Wait ended: deploy'"]);
}

#[test]
fn control_characters_and_quotes_in_script_text_are_sanitized() {
    let mut digest = call(
        0,
        "sink.digest",
        json!({"agent":"SYN\tAPSE","item":{"title":"Line one\nline two\u{7}\u{202E}evil': 'Approve"},"action":"wake"}),
        captured(),
        "captured",
    );
    digest["sink"] = json!({"agent":"SYN\tAPSE","effective":"wake"});
    let error = json!({"kind":"script","detail":"Error: bad\u{0}\u{1b}[31m red"});
    let runs = vec![run(&due(0), "failed", error, false, vec![digest])];
    let text = render(&hourly_manifest(), &summary(day_window(1, 1), runs));
    assert_bounded(&text);
    assert_eq!(
        text.would,
        ["Wake 'SYN APSE': 'Line one line two evil\u{2019}: \u{2019}Approve'"]
    );
    assert_eq!(
        text.problems,
        ["The run at 2026-05-02 00:00 UTC failed: the script threw 'Error: bad [31m red'."]
    );
}

#[test]
fn summary_is_cut_to_three_hundred_bytes_on_a_char_boundary() {
    let events: Vec<Value> = (0..30)
        .map(|i| json!({"module":"plexus","name":format!("event_{i:02}_with_a_rather_long_name"),"version":1}))
        .collect();
    let m = manifest(json!({"id":"many","version":1,"purpose":"Many events",
        "trigger":{"events":events},"sinks":[{"agent":"SYNAPSE","digest_max":"wake"}]}));
    let text = render(
        &m,
        &summary(json!({"kind":"events","covered":null}), vec![]),
    );
    assert_bounded(&text);
    assert!(
        text.summary.len() > SUMMARY_MAX_BYTES - 4,
        "{}",
        text.summary
    );
    assert!(text.summary.ends_with('…'), "{}", text.summary);
}

#[test]
fn problem_lines_keep_exactly_two_hundred_bytes_and_cut_two_hundred_and_one() {
    let exact = "a".repeat(PROBLEM_ITEM_MAX_BYTES);
    let over = "a".repeat(PROBLEM_ITEM_MAX_BYTES + 1);
    let lines = capped(
        vec![exact.clone(), over],
        PROBLEMS_MAX_ITEMS,
        PROBLEM_ITEM_MAX_BYTES,
    );
    assert_eq!(lines[0], exact);
    assert_eq!(lines[1].len(), PROBLEM_ITEM_MAX_BYTES);
    assert!(lines[1].ends_with('…'), "{}", lines[1]);
    // A multibyte character straddling the limit is dropped whole.
    let straddle = format!("{}é", "a".repeat(PROBLEM_ITEM_MAX_BYTES - 1));
    let lines = capped(vec![straddle], PROBLEMS_MAX_ITEMS, PROBLEM_ITEM_MAX_BYTES);
    assert!(lines[0].len() <= PROBLEM_ITEM_MAX_BYTES);
    assert!(lines[0].ends_with('…'), "{}", lines[0]);
}

#[test]
fn more_than_eight_problems_end_in_and_n_more() {
    let runs: Vec<Value> = (0..10)
        .map(|i| {
            run(
                &due(i * 60),
                "failed",
                json!({"kind":"deadline"}),
                false,
                vec![],
            )
        })
        .collect();
    let text = render(&hourly_manifest(), &summary(day_window(10, 10), runs));
    assert_bounded(&text);
    assert_eq!(text.problems.len(), PROBLEMS_MAX_ITEMS);
    assert_eq!(text.problems[7], "and 3 more");
}

#[test]
fn a_dry_run_that_could_not_run_says_so_as_its_only_problem() {
    let text = render(
        &hourly_manifest(),
        &json!({"error":"dry run failed: no worker"}),
    );
    assert_bounded(&text);
    assert_eq!(
        text.problems,
        ["The dry run failed: 'dry run failed: no worker'."]
    );
    assert!(text.would.is_empty());
}

#[test]
fn a_one_shot_reminder_names_its_time() {
    let m = manifest(json!({"id":"remind","version":1,"purpose":"Remind",
        "trigger":{"schedule":{"interval":"1h"}},
        "sinks":[{"agent":"SYNAPSE","digest_max":"wake"}]}));
    let window = json!({"kind":"at","at":"2026-05-02T09:30:00Z","replayed":1,"dry_run":true});
    let ended = call(1, "instance.end", json!({}), captured(), "captured");
    let runs = vec![run(&due(0), "succeeded", Value::Null, false, vec![ended])];
    let text = render(&m, &summary(window, runs));
    assert_eq!(
        text.summary,
        "Runs once, at 2026-05-02 09:30 UTC; the dry run ran it ahead of time."
    );
    assert_eq!(
        text.would,
        ["End this flow instance, so it doesn't run again"]
    );
}
