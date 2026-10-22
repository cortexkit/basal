use super::*;
use cortexkit_role_tool_provider::result::{MAX_RESULT_BYTES, ToolResult, validate};

fn successful_run() -> Value {
    json!({"status":"completed","value":{"answer":42},"output":"partial\nλ\n",
        "calls":[{"tool":"echo","outcome":"completed","waited_on_person":false,"duration_ms":8}],
        "warnings":[],"duration_ms":10,"person_wait_ms":0,"call_count":1,"keyless":false,
        "catalog_digest":"catalog"})
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

const TABLE: &str = "\nTool | Outcome | Waited on person | Duration (ms)\n--- | --- | --- | ---\n";

// The exact model-visible text, written out by hand. A change to these bytes
// changes what every agent reads, so it must be a deliberate edit here.
#[test]
fn content_text_snapshot() {
    let success = successful_run();
    let mut budget = success.clone();
    budget["status"] = "budget_exhausted:tool_calls".into();
    budget["error"] =
        json!({"code":"budget_exhausted:tool_calls","message":"run budget exhausted"});
    budget.as_object_mut().unwrap().remove("value");
    let mut refused = success.clone();
    refused["status"] = "failed".into();
    refused["error"] =
        json!({"code":"no_session","message":"browser_use (call 1) failed: No session."});
    refused["calls"][0]["outcome"] = "refused".into();
    refused["calls"][0]["code"] = "no_session".into();
    refused.as_object_mut().unwrap().remove("value");
    let mut unsettled = success.clone();
    unsettled["keyless"] = true.into();
    unsettled["calls"][0]["outcome"] = "outcome_unknown".into();
    unsettled["calls"][0]
        .as_object_mut()
        .unwrap()
        .remove("duration_ms");
    unsettled["warnings"] = json!([{"message":"Explicit warning"},{"code":"unknown"}]);
    let mut silent = success.clone();
    silent["output"] = "".into();
    silent["calls"] = json!([]);
    silent["call_count"] = 0.into();
    let cases = [
        (
            success,
            format!(
                "completed · 10 ms · 1 calls · person wait 0 ms\n\nReturn value:\n{{\"answer\":42}}\n\nOutput:\npartial\nλ\n{TABLE}echo | completed | false | 8\n"
            ),
            json!([]),
        ),
        (
            budget,
            format!(
                "budget_exhausted: tool_calls · 10 ms · 1 calls · person wait 0 ms\nError (budget_exhausted:tool_calls): run budget exhausted\n\nOutput:\npartial\nλ\n{TABLE}echo | completed | false | 8\n"
            ),
            json!([]),
        ),
        (
            refused,
            format!(
                "failed · 10 ms · 1 calls · person wait 0 ms\nError (no_session): browser_use (call 1) failed: No session.\n\nOutput:\npartial\nλ\n{TABLE}echo | refused | false | 8\n"
            ),
            json!([]),
        ),
        (
            unsettled,
            format!(
                "completed · 10 ms · 1 calls · person wait 0 ms\n\nReturn value:\n{{\"answer\":42}}\n\nOutput:\npartial\nλ\n{TABLE}echo | outcome_unknown | false | —\n\nWarning: Explicit warning\nWarning: {{\"code\":\"unknown\"}}\nWarning: This call had no key and can't be recovered if its reply is lost.\nWarning: Some calls have unknown outcomes and may still be running; they were never retried.\n"
            ),
            json!([{"message":"Explicit warning"},{"code":"unknown"},
                {"code":"keyless","message":"This call had no key and can't be recovered if its reply is lost."},
                {"code":"calls_unsettled","message":"Some calls have unknown outcomes and may still be running; they were never retried."}]),
        ),
        // No console output and no calls: neither heading is printed.
        (
            silent,
            "completed · 10 ms · 0 calls · person wait 0 ms\n\nReturn value:\n{\"answer\":42}\n"
                .to_owned(),
            json!([]),
        ),
    ];
    for (run, expected, warnings) in cases {
        let new = render("codemode:example", run.clone());
        assert_eq!(new["content"], json!([{"type":"text","text":expected}]));
        let decoded: ToolResult = serde_json::from_value(new.clone()).unwrap();
        validate(&decoded).unwrap();
        assert!(new.get("text").is_none());
        let mut fields = new["structuredContent"].clone();
        assert_eq!(fields["run_id"], "codemode:example");
        assert_eq!(fields["termination_reason"], run["status"]);
        for key in ["run_id", "termination_reason", "budget"] {
            fields.as_object_mut().unwrap().remove(key);
        }
        let mut expected_fields = run;
        expected_fields["warnings"] = warnings;
        assert_eq!(fields, expected_fields);
    }
}

#[test]
fn error_detail_is_plain_text_on_its_own_line() {
    let mut run = successful_run();
    run["status"] = "not_granted".into();
    run.as_object_mut().unwrap().remove("value");
    run["error"] =
        json!({"code":"not_granted","message":"refused \"here\"","detail":{"reason":"x"}});
    let result = render("codemode:example", run);
    assert!(
        text(&result).starts_with(
            "not_granted · 10 ms · 1 calls · person wait 0 ms\nError (not_granted): refused \"here\"\nError detail: {\"reason\":\"x\"}\n"
        ),
        "{}",
        text(&result)
    );
}

#[test]
fn content_errors_preserve_the_run_outcome_and_code() {
    let success = render("codemode:example", successful_run());
    assert!(success.get("isError").is_none());
    assert!(success.get("errorCode").is_none());
    let mut cancelled = successful_run();
    cancelled["status"] = "cancelled".into();
    cancelled.as_object_mut().unwrap().remove("value");
    let cancellation = render("codemode:example", cancelled);
    assert_eq!(cancellation["isError"], true);
    assert_eq!(cancellation["errorCode"], "cancelled");
    for (status, code, expected) in [
        (
            "budget_exhausted:js_cpu",
            "budget_exhausted:js_cpu",
            "budget_exhausted",
        ),
        (
            "budget_exhausted:tool_calls",
            "budget_exhausted:tool_calls",
            "budget_exhausted",
        ),
        ("failed", "script", "script"),
        ("cancelled", "cancelled", "cancelled"),
        ("not_granted", "not_granted", "not_granted"),
    ] {
        let mut run = successful_run();
        run["status"] = status.into();
        run["error"] = json!({"code":code,"message":"stopped"});
        run.as_object_mut().unwrap().remove("value");
        let result = render("codemode:example", run);
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(result["errorCode"], expected);
        assert_eq!(result["structuredContent"]["status"], status);
        assert_eq!(result["structuredContent"]["error"]["code"], code);
        if let Some(budget) = code.strip_prefix("budget_exhausted:") {
            assert_eq!(result["structuredContent"]["budget"], budget);
        }
    }
}

#[test]
fn content_size_cap_marks_omissions_without_changing_success_or_failure() {
    for error in [None, Some("script")] {
        let mut run = successful_run();
        if let Some(code) = error {
            run["status"] = "failed".into();
            run["error"] = json!({"code":code,"message":"provider refused"});
        }
        // This detail is program-facing only; dropping it must not alter the
        // original model text or the outcome of a run that may have made writes.
        let detail = json!({"provider":"\u{0}λ".repeat(MAX_RESULT_BYTES)});
        let original_bytes = serialized_size(&detail);
        let text = text(&render("codemode:example", run.clone())).to_owned();
        run["detail"] = detail;
        let result = render("codemode:example", run);
        assert!(serialized_size(&result) < MAX_RESULT_BYTES);
        let decoded: ToolResult = serde_json::from_value(result.clone()).unwrap();
        validate(&decoded).unwrap();
        assert_eq!(
            result["content"][0]["text"],
            format!("{text}{STRUCTURED_OMITTED}")
        );
        let fields = &result["structuredContent"];
        assert_eq!(
            fields["detail"],
            json!({"omitted":true,"original_bytes":original_bytes})
        );
        assert_eq!(
            fields["status"],
            if error.is_some() {
                "failed"
            } else {
                "completed"
            }
        );
        assert_eq!(fields["termination_reason"], fields["status"]);
        assert_eq!(fields["run_id"], "codemode:example");
        assert_eq!(fields["duration_ms"], 10);
        assert_eq!(fields["call_count"], 1);
        assert_eq!(fields["person_wait_ms"], 0);
        assert_eq!(fields["value"], json!({"answer":42}));
        assert_eq!(result.get("errorCode").and_then(Value::as_str), error);
        if let Some(code) = error {
            assert_eq!(fields["error"]["code"], code);
        }
        assert_eq!(
            result.get("isError").and_then(Value::as_bool),
            error.map(|_| true)
        );
    }
}

#[test]
fn oversized_console_and_summaries_truncate_text_explicitly_on_utf8_boundaries() {
    let mut run = successful_run();
    run["output"] = "\u{0}λ".repeat(MAX_RESULT_BYTES).into();
    run["calls"][0]["tool"] = "λ".repeat(MAX_RESULT_BYTES).into();
    let result = render("codemode:example", run);
    assert!(serialized_size(&result) < MAX_RESULT_BYTES);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(TEXT_TRUNCATED)
    );
    assert_eq!(result["structuredContent"]["output"]["omitted"], true);
    assert_eq!(result["structuredContent"]["calls"]["omitted"], true);
    assert!(result.get("isError").is_none());
}

#[test]
fn running_reply_is_valid_content_with_no_error_flag() {
    let result = running("codemode:example", &[]);
    let decoded: ToolResult = serde_json::from_value(result.clone()).unwrap();
    validate(&decoded).unwrap();
    assert_eq!(
        result["content"][0]["text"],
        "Still running: the program had not finished when this reply was due. When the program finishes, basal retains its final result for this call."
    );
    assert_eq!(
        result["structuredContent"],
        json!({"status":"running","run_id":"codemode:example"})
    );
    assert!(result.get("isError").is_none());
}

#[test]
fn running_reply_names_each_held_call_and_what_the_person_must_do() {
    let hold = |position, tool: &str, reason: Option<&str>| Hold {
        position,
        tool: tool.into(),
        reason: reason.map(str::to_owned),
    };
    let result = running(
        "codemode:example",
        &[
            hold(1, "browser_use", Some("consent")),
            hold(2, "login", Some("login_handoff")),
            hold(4, "other", Some("site_review")),
            hold(5, "quiet", None),
        ],
    );
    let decoded: ToolResult = serde_json::from_value(result.clone()).unwrap();
    validate(&decoded).unwrap();
    assert_eq!(
        result["content"][0]["text"],
        "Waiting on the person: browser_use (call 2) needs their consent; login (call 3) needs them to sign in; other (call 5) is waiting on them (site_review); quiet (call 6) is waiting on them. The program resumes when they answer; a run waits on people for at most 10 minutes in total. When the program finishes, basal retains its final result for this call."
    );
    assert_eq!(
        result["structuredContent"],
        json!({"status":"running","run_id":"codemode:example"})
    );
    assert!(result.get("isError").is_none());
}

#[test]
fn nested_refusal_details_are_bounded_without_omitting_the_error_code() {
    let mut run = successful_run();
    run["status"] = "not_granted".into();
    let detail = json!({"provider":"x".repeat(MAX_RESULT_BYTES)});
    let bytes = serialized_size(&detail);
    run["detail"] = detail.clone();
    run["error"] = json!({"code":"not_granted","message":"refused","detail":detail});
    let result = render("codemode:example", run);
    assert!(serialized_size(&result) < MAX_RESULT_BYTES);
    assert_eq!(result["isError"], true);
    assert_eq!(result["errorCode"], "not_granted");
    assert_eq!(result["structuredContent"]["error"]["code"], "not_granted");
    let marker = json!({"omitted":true,"original_bytes":bytes});
    assert_eq!(result["structuredContent"]["detail"], marker);
    assert_eq!(result["structuredContent"]["error"]["detail"], marker);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(TEXT_TRUNCATED)
    );
}

#[test]
fn structured_omission_keeps_large_but_deliverable_model_text() {
    let mut run = successful_run();
    run["status"] = "failed".into();
    run["error"] = json!({"code":"script","message":"x".repeat(2 * 1024 * 1024)});
    let text = format!(
        "failed · 10 ms · 1 calls · person wait 0 ms\nError (script): {}\n\nReturn value:\n{{\"answer\":42}}\n\nOutput:\npartial\nλ\n{TABLE}echo | completed | false | 8\n",
        "x".repeat(2 * 1024 * 1024)
    );
    let result = render("codemode:example", run);
    assert_eq!(
        result["content"][0]["text"],
        format!("{text}{STRUCTURED_OMITTED}")
    );
    assert_eq!(result["errorCode"], "script");
    assert_eq!(
        result["structuredContent"]["error"]["message"]["omitted"],
        true
    );
    assert!(serialized_size(&result) < MAX_RESULT_BYTES);
}

#[test]
fn withdrawn_call_is_distinct_from_unsent_cancellation_in_text_and_structure() {
    let mut run = successful_run();
    run["status"] = json!("cancelled");
    run.as_object_mut().unwrap().remove("value");
    run["calls"] = json!([
        {"tool":"browser_use","outcome":"withdrawn","code":"withdrawn","waited_on_person":true,"duration_ms":120},
        {"tool":"queued","outcome":"cancelled","waited_on_person":false}
    ]);
    run["call_count"] = json!(2);
    let result = render("codemode:withdrawal", run);
    let decoded: ToolResult = serde_json::from_value(result.clone()).unwrap();
    validate(&decoded).unwrap();
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("browser_use | withdrawn | true | 120"),
        "{text}"
    );
    assert!(text.contains("queued | cancelled | false"), "{text}");
    assert_eq!(
        result["structuredContent"]["calls"][0]["outcome"],
        "withdrawn"
    );
    assert_eq!(
        result["structuredContent"]["calls"][1]["outcome"],
        "cancelled"
    );
}
