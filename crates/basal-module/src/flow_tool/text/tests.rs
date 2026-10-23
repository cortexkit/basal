use super::summary;
use serde_json::{Value, json};

const HASH: &str = "c9731d419ce0771c0123456789abcdef0123456789abcdef0123456789abcdef0123";

fn captured_run(id: usize, module: &str, op: &str) -> Value {
    json!({
        "run_id": format!("run-{id}"),
        "trigger_id": format!("schedule-{id}"),
        "trigger": {"kind":"schedule", "due":format!("2026-01-01T00:{id:02}:00Z")},
        "state":"failed", "partial":true,
        "error":{"kind":"script", "detail":"HostError: captured by a dry run: not executed\n    at <eval>"},
        "calls_total":1,
        "calls":[{"position":0,"kind":"op","module":module,"op":op,
            "args":{"different_per_run":id},"action":"captured","partial":false,
            "outcome":{"rejected":"captured"}}]
    })
}

fn schedule(runs: Vec<Value>) -> Value {
    json!({
        "mode":"capture", "partial":runs.iter().any(|run| run["partial"] == true),
        "window":{"kind":"schedule", "requested":{"from":"2026-01-01T00:00:00Z", "to":"2026-01-02T00:00:00Z"},
            "due_times_in_window":288, "due_times_is_lower_bound":false,
            "replayed":runs.len(),"capped":true},
        "warnings":[], "runs":runs
    })
}

fn install(report: Value) -> Value {
    json!({"flow_id":"board-hygiene", "version":15, "code_hash":HASH,
        "card_id":format!("card:board-hygiene:v15:{HASH}"), "state":"pending",
        "warnings":[],"dry_run":report})
}

fn events() -> Value {
    json!({"flow_id":"board-hygiene","version":15,"state_reset":false,"warnings":[],
        "summary":{"mode":"capture","partial":false,"warnings":[],"runs":[],
            "window":{"kind":"events","covered":null,"note":"event replay needs the event plane; no events were replayed"}}})
}

#[test]
fn install_groups_identical_captured_runs_and_omits_empty_warnings() {
    let result = install(schedule(
        (0..10)
            .map(|id| captured_run(id, "prefrontal-core", "board.read"))
            .collect(),
    ));
    assert_eq!(
        summary("install", &result),
        format!(
            "Install requested for board-hygiene v15. The approval card is on the operator's phone (card:board-hygiene:v15:{HASH}).\n\
         Code hash (full): {HASH}.\n\
         Dry run (capture mode): 10 of 288 scheduled runs in the last 24 h replayed. Replay capped.\n\
         Partial: captured results may leave later behavior untested.\n\
         10 runs: stopped at prefrontal-core board.read, which the dry run captured and did not execute; partial."
        )
    );
}

#[test]
fn dry_run_lists_distinct_outcomes_with_counts_and_does_not_hide_real_errors() {
    let mut failed = captured_run(9, "prefrontal-core", "board.read");
    failed["error"]["detail"] = json!("TypeError: real failure after catching capture");
    let result = json!({"flow_id":"board-hygiene", "version":15,"state_reset":false,"warnings":[],
    "summary":schedule(vec![
        captured_run(0, "prefrontal-core", "board.read"),
        captured_run(1, "prefrontal-core", "board.read"),
        captured_run(2, "other-module", "items.read"),
        json!({"state":"succeeded","calls":[],"calls_total":0,"partial":false}),
        json!({"state":"succeeded","calls":[],"calls_total":0,"partial":false}),
        failed,
        json!({"admission":"Coalesced"})
    ])});
    assert_eq!(
        summary("dry_run", &result),
        "Preview for board-hygiene v15.\n\
         Dry run (capture mode): 7 of 288 scheduled runs in the last 24 h replayed. Replay capped.\n\
         Partial: captured results may leave later behavior untested.\n\
         2 runs: stopped at prefrontal-core board.read, which the dry run captured and did not execute; partial.\n\
         1 run: stopped at other-module items.read, which the dry run captured and did not execute; partial.\n\
         2 runs: succeeded; 0 calls.\n\
         1 run: failed (script): TypeError: real failure after catching capture; 1 call: 1 captured prefrontal-core board.read (not executed); partial.\n\
         1 run: not admitted: Coalesced."
    );
}

#[test]
fn event_trigger_dry_run_explains_why_nothing_was_replayed() {
    assert_eq!(
        summary("dry_run", &events()),
        "Preview for board-hygiene v15.\n\
         Dry run (capture mode): event window; no events were replayed because event replay needs the event plane. No replay window was covered."
    );
}

#[test]
fn warnings_are_plain_lines_with_context_and_duplicates_are_omitted() {
    let mut result = events();
    result["warnings"] = json!([
        {"kind":"private_text_with_outbound_op", "module":"mail", "op":"send", "text":"the flow reads private text and holds an op that can send data out"},
        {"kind":"protected_root", "path":"~/Documents/notes", "text":"On macOS, the first access to ~/Documents asks the operator for permission.\nOn Windows, writes under ~/Documents are blocked while Controlled Folder Access is on."}
    ]);
    result["summary"]["warnings"] = result["warnings"].clone();
    assert_eq!(
        summary("dry_run", &result),
        "Preview for board-hygiene v15.\n\
         Dry run (capture mode): event window; no events were replayed because event replay needs the event plane. No replay window was covered.\n\
         Warning: private_text_with_outbound_op; mail send: the flow reads private text and holds an op that can send data out\n\
         Warning: protected_root; ~/Documents/notes: On macOS, the first access to ~/Documents asks the operator for permission.\n\
         Warning: protected_root; ~/Documents/notes: On Windows, writes under ~/Documents are blocked while Controlled Folder Access is on."
    );
}

#[test]
fn outcome_limit_reports_how_many_runs_were_omitted() {
    let runs = (0..8)
        .flat_map(|id| vec![captured_run(id, "module", &format!("read-{id}")); 2])
        .collect();
    let text = summary("install", &install(schedule(runs)));
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("2 runs:"))
            .count(),
        5
    );
    assert!(
        text.ends_with("6 more runs across 3 other outcomes; see structuredContent for details.")
    );
}

#[test]
fn lower_bound_schedule_count_is_not_reported_as_exact() {
    let mut report = schedule(vec![]);
    report["window"]["due_times_in_window"] = json!(20_000);
    report["window"]["due_times_is_lower_bound"] = json!(true);
    assert!(summary("install", &install(report)).ends_with(
        "Dry run (capture mode): 0 of at least 20000 scheduled runs in the last 24 h replayed. Replay capped. Schedule scan capped; the due count is a lower bound."));
}

#[test]
fn no_due_runs_and_uncapped_window_are_explicit() {
    let mut report = schedule(vec![]);
    report["window"]["due_times_in_window"] = json!(0);
    report["window"]["capped"] = json!(false);
    assert!(summary("install", &install(report)).ends_with(
        "Dry run (capture mode): 0 of 0 scheduled runs in the last 24 h replayed. Replay not capped."));
}

#[test]
fn preview_names_sample_trigger_and_state_reset_without_dumping_result() {
    let result = json!({"flow_id":"watch", "version":2, "state_reset":true,"warnings":[],
        "summary":{"mode":"capture","window":{"kind":"synthetic"},"partial":false,"warnings":[],
            "runs":[{"state":"succeeded","result":{"private":"not agent-facing"},"calls":[],"calls_total":0,"partial":false}]}});
    assert_eq!(
        summary("dry_run", &result),
        "Preview for watch v2.\n\
         Dry run (capture mode): no replay window; 1 sample-trigger run replayed.\n\
         1 run: succeeded; 0 calls.\n\
         The declared state epoch changed; a live run would reset state. This preview did not clear it."
    );
}

#[test]
fn install_failure_and_policy_approval_have_plain_text() {
    let mut result = install(json!({"error":"worker unavailable"}));
    result["card_id"] = Value::Null;
    result["approval"] = json!({"kind":"policy"});
    assert_eq!(
        summary("install", &result),
        format!(
            "Install requested for board-hygiene v15. Approved by install policy; no approval card is needed.\n\
         Code hash (full): {HASH}.\n\
         Dry run could not be completed: worker unavailable"
        )
    );
}

#[test]
fn remaining_actions_have_plain_text_instead_of_json() {
    for (action, result, expected) in [
        (
            "enable",
            json!({"flow_id":"watch","state":"enabled","changed":true}),
            "watch: enabled.",
        ),
        (
            "disable",
            json!({"flow_id":"watch","state":"disabled","changed":false}),
            "watch: disabled (unchanged).",
        ),
        (
            "instantiate",
            json!({"flow_id":"watch/a","template":"watch","version":2,"new":true,"expires_at":0,"params":{"secret":"not printed"}}),
            "Instance watch/a created from watch v2; expires 1970-01-01T00:00:00Z.",
        ),
        (
            "instantiate",
            json!({"flow_id":"watch/a","template":"watch","version":2,"new":false,"expires_at":0}),
            "Instance watch/a extended from watch v2; expires 1970-01-01T00:00:00Z.",
        ),
        (
            "end",
            json!({"flow_id":"watch/a","ended":true}),
            "Instance watch/a: ended.",
        ),
        (
            "end",
            json!({"flow_id":"watch/a","ended":false}),
            "Instance watch/a: already ended.",
        ),
        ("list", json!({"flows":[]}), "No flows."),
        (
            "list",
            json!({"flows":[
                {"flow_id":"watch","state":"enabled","approved_version":2,"last_run":{"run_id":"run-1","state":"failed","ended_at":0}},
                {"flow_id":"pending","state":"pending","approved_version":null,"last_run":null}
            ]}),
            "watch: enabled; approved version 2; last run failed (run-1)\npending: pending; approved version none; last run none",
        ),
    ] {
        assert_eq!(summary(action, &result), expected, "{action}");
    }
}

#[test]
fn successful_runs_still_show_captured_and_refused_call_outcomes() {
    let mut run = captured_run(0, "prefrontal-core", "board.read");
    run["state"] = json!("succeeded");
    run["error"] = Value::Null;
    run["calls_total"] = json!(4);
    for position in 1..4 {
        run["calls"].as_array_mut().unwrap().push(json!({"position":position,"kind":"op","module":"mail","op":"send","action":"refused","outcome":{"rejected":"denied"}}));
    }
    let result = json!({"flow_id":"board-hygiene","version":15,"state_reset":false,"warnings":[],
        "summary":schedule(vec![run])});
    assert!(summary("dry_run", &result).ends_with(
        "1 run: succeeded; 4 calls: 1 captured prefrontal-core board.read (not executed), 3 refused mail send (rejected: denied); partial."));
}
