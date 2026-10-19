use super::*;
use cortexkit_role_tool_provider::result::{MAX_RESULT_BYTES, ToolResult, validate};

// Preserve the former renderer independently: it returned model-facing text
// next to the machine fields in one JSON object. The current renderer puts that
// same text in content[0].text, separate from structuredContent. Comparing both
// outputs catches any accidental change to the bytes the model receives.
fn legacy_render(mut result: Value) -> Value {
    let status = result["status"]
        .as_str()
        .unwrap_or("failed")
        .replace("budget_exhausted:", "budget_exhausted: ");
    let calls = result["calls"].as_array().cloned().unwrap_or_default();
    let mut warnings = result["warnings"].as_array().cloned().unwrap_or_default();
    if result["keyless"] == true {
        warnings.push(json!({"code":"keyless","message":"This call had no key and can't be recovered if its reply is lost."}));
    }
    if calls
        .iter()
        .any(|call| call["outcome"] == "outcome_unknown")
    {
        warnings.push(json!({"code":"calls_unsettled","message":"Some calls have unknown outcomes and may still be running; they were never retried."}));
    }
    let mut text = format!(
        "{status} · {} ms · {} calls · person wait {} ms\n",
        result["duration_ms"],
        result
            .get("call_count")
            .and_then(Value::as_u64)
            .unwrap_or(calls.len() as u64),
        result["person_wait_ms"]
    );
    if let Some(error) = result.get("error") {
        text.push_str(&format!("Error: {error}\n"));
    }
    if let Some(value) = result.get("value") {
        text.push_str(&format!("\nReturn value:\n{value}\n"));
    }
    text.push_str(&format!(
        "\nOutput:\n{}\n\nTool | Outcome | Waited on person | Duration (ms)\n",
        result["output"].as_str().unwrap_or("")
    ));
    for call in &calls {
        text.push_str(&format!(
            "{} | {} | {} | {}\n",
            call["tool"].as_str().unwrap_or(""),
            call["outcome"].as_str().unwrap_or(""),
            call["waited_on_person"],
            call.get("duration_ms")
                .map_or_else(|| "—".into(), Value::to_string)
        ));
    }
    for warning in &warnings {
        text.push_str(&format!(
            "Warning: {}\n",
            warning
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| warning.to_string(), str::to_owned)
        ));
    }
    result["warnings"] = warnings.into();
    result["text"] = text.into();
    result
}

fn successful_run() -> Value {
    json!({"status":"completed","value":{"answer":42},"output":"partial\nλ\n",
        "calls":[{"tool":"echo","outcome":"completed","waited_on_person":false,"duration_ms":8}],
        "warnings":[],"duration_ms":10,"person_wait_ms":0,"call_count":1,"keyless":false,
        "catalog_digest":"catalog"})
}

#[test]
fn content_text_is_byte_identical_to_the_legacy_renderer() {
    let success = successful_run();
    let mut budget = success.clone();
    budget["status"] = "budget_exhausted:tool_calls".into();
    budget["error"] =
        json!({"code":"budget_exhausted:tool_calls","message":"run budget exhausted"});
    budget.as_object_mut().unwrap().remove("value");
    let mut refused = success.clone();
    refused["status"] = "failed".into();
    refused["error"] = json!({"code":"script","message":"tool call refused"});
    refused["calls"][0]["outcome"] = "refused".into();
    refused["calls"][0]["code"] = "not_permitted".into();
    refused.as_object_mut().unwrap().remove("value");
    let mut unsettled = success.clone();
    unsettled["keyless"] = true.into();
    unsettled["calls"][0]["outcome"] = "outcome_unknown".into();
    unsettled["calls"][0]
        .as_object_mut()
        .unwrap()
        .remove("duration_ms");
    unsettled["warnings"] = json!([{"message":"Explicit warning"},{"code":"unknown"}]);
    for run in [success, budget, refused, unsettled] {
        let old = legacy_render(run.clone());
        let new = render("codemode:example", run);
        assert_eq!(new["content"], json!([{"type":"text","text":old["text"]}]));
        let decoded: ToolResult = serde_json::from_value(new.clone()).unwrap();
        validate(&decoded).unwrap();
        assert!(new.get("text").is_none());
        let mut fields = new["structuredContent"].clone();
        assert_eq!(fields["run_id"], "codemode:example");
        assert_eq!(fields["termination_reason"], old["status"]);
        for key in ["run_id", "termination_reason", "budget"] {
            fields.as_object_mut().unwrap().remove(key);
        }
        let mut old_fields = old;
        old_fields.as_object_mut().unwrap().remove("text");
        assert_eq!(fields, old_fields);
    }
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
        run["detail"] = detail;
        let text = legacy_render(run.clone())["text"]
            .as_str()
            .unwrap()
            .to_owned();
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
    let result = running("codemode:example");
    let decoded: ToolResult = serde_json::from_value(result.clone()).unwrap();
    validate(&decoded).unwrap();
    assert_eq!(
        result["content"][0]["text"],
        "running: the final result will be retained in late_results."
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
    let text = legacy_render(run.clone())["text"]
        .as_str()
        .unwrap()
        .to_owned();
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
