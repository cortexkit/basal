use super::*;
use cortexkit_llm_tools::{Usage, errors, extract};

fn fixture(cap: f64, tagged: bool) -> Fixture {
    let f = Fixture::new();
    f.catalog.set_op(
        "provider",
        "classify_alias",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    if tagged {
        f.catalog.set_tool_contract(
            "provider",
            "classify_alias",
            json!({
            "capabilities":[cortexkit_llm_tools::classify::TAG]}),
        );
    }
    f.start(
        &[(
            "llm_classify",
            "provider",
            "classify_alias",
            cortexkit_llm_tools::classify::input_schema(),
        )],
        Limits {
            model_cost_usd: cap,
            ..Limits::default()
        },
        100_000,
    );
    assert!(matches!(
        f.parent.recv_timeout(TIMEOUT).unwrap(),
        ParentMessage::Activate(_)
    ));
    f
}

fn input() -> Value {
    let vectors: Value = serde_json::from_str(cortexkit_llm_tools::vectors::CLASSIFY).unwrap();
    vectors["valid_requests"][0]["value"].clone()
}

fn spent(f: &Fixture) -> f64 {
    f.host
        .store
        .read(|c| crate::classify_cost::spent(c, "program", "r"))
        .unwrap()
}

#[test]
fn program_classify_clamps_and_supplies_remaining_dollars() {
    let f = fixture(0.30, true);
    for (pos, omit) in [(0, false), (1, true)] {
        let mut args = input();
        if omit {
            args.as_object_mut().unwrap().remove("max_cost_usd");
        } else {
            args["max_cost_usd"] = json!(1);
        }
        f.issue(pos, "llm_classify", args);
        let attempt = f.attempt();
        assert_eq!(attempt.input["max_cost_usd"], json!(0.30));
        attempt
            .reply
            .send(Err(WireError::Refused {
                code: "cost_unpriceable".into(),
                message: "no price".into(),
            }))
            .unwrap();
        assert_eq!(f.delivery().settlement, Settlement::Rejected);
    }
    assert_eq!(spent(&f), 0.0);
    f.cleanup(true);
}

#[test]
fn program_classify_exhausted_dollars_never_send() {
    let f = fixture(0.0, true);
    f.issue(0, "llm_classify", input());
    let delivery = f.delivery();
    let value = parse(delivery.value.as_str()).unwrap();
    assert_eq!(value["code"], "budget_exhausted");
    assert_eq!(value["budget"], "model_cost_usd");
    assert!(f.attempts.try_recv().is_err());
    f.cleanup(true);
}

#[test]
fn program_classify_charges_reported_cost_and_overrun() {
    let f = fixture(0.30, true);
    let mut args = input();
    args["max_cost_usd"] = json!(0.001);
    f.issue(0, "llm_classify", args);
    let attempt = f.attempt();
    assert_eq!(attempt.input["max_cost_usd"], 0.001);
    attempt
        .reply
        .send(Ok(
            json!({"model":"example/classifier","answers":{},"usage":{},"cost_usd":0.42}),
        ))
        .unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Fulfilled);
    assert_eq!(spent(&f), 0.42);
    f.finish("1");
    let result = f.terminal();
    assert_eq!(result["budgets"]["model_cost_usd"]["spent"], 0.42);
    assert!(
        (result["budgets"]["model_cost_usd"]["remaining"]
            .as_f64()
            .unwrap()
            + 0.12)
            .abs()
            < 1e-12
    );
    f.cleanup(false);
}

#[test]
fn program_classify_unmeasured_charges_max_and_refusals_are_free() {
    let f = fixture(0.30, true);
    for (pos, code) in [
        "invalid_params",
        "scope_required",
        "flow_scope_required",
        "cost_unpriceable",
        "cost_exceeded",
        "provider_not_allowed",
        "not_started",
        "cost_unmeasured",
    ]
    .into_iter()
    .enumerate()
    {
        let mut args = input();
        args["max_cost_usd"] = json!(0.10);
        f.issue(pos as u64, "llm_classify", args);
        f.attempt()
            .reply
            .send(Err(WireError::RefusedDetails {
                code: code.into(),
                message: code.into(),
                detail: json!({"consent":{}}),
            }))
            .unwrap();
        assert_eq!(f.delivery().settlement, Settlement::Rejected);
        assert_eq!(
            spent(&f),
            if code == "cost_unmeasured" { 0.10 } else { 0.0 }
        );
    }
    f.cleanup(true);
}

#[test]
fn untagged_program_classify_is_an_ordinary_call() {
    let f = fixture(0.0, false);
    f.issue(0, "llm_classify", input());
    let attempt = f.attempt();
    assert_eq!(attempt.input["max_cost_usd"], 0.01);
    attempt
        .reply
        .send(Ok(json!({"answers":{},"usage":{},"cost_usd":1})))
        .unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Fulfilled);
    assert_eq!(spent(&f), 0.0);
    f.cleanup(true);
}

#[test]
fn program_classify_cancelled_before_dispatch_releases_reservation() {
    let f = fixture(0.30, true);
    f.host
        .store
        .write(|tx| {
            store::insert_call(tx, "r", 0, "llm_classify", 1, CallStart::Queued)?;
            let mut args = input();
            crate::classify_cost::reserve(tx, "program", "r", 0, None, 100, 0.30, None, &mut args)?
                .unwrap();
            assert_eq!(store::settle_pending_calls(tx, "r", 101)?, (1, 0));
            Ok(())
        })
        .unwrap();
    assert_eq!(spent(&f), 0.0);
    f.cleanup(true);
}

/// A provider refusal reaches the supervisor with its `detail`, which is where
/// the spend marker is. Charging must follow the marker, not only the code: the
/// reservation here is 0.10, so a free and a measured charge both differ from
/// it, and either would have charged the reservation before.
#[test]
fn program_classify_refusal_charges_by_the_spend_marker_in_its_detail() {
    let f = fixture(0.30, true);
    // The classify contract requires a ceiling; 0.10 is the reservation the
    // supervisor records for each call.
    let mut args = input();
    args["max_cost_usd"] = json!(0.10);
    f.issue(0, "llm_classify", args);
    let first = f.attempt();
    assert_eq!(first.input["max_cost_usd"], 0.10);
    first
        .reply
        .send(Err(WireError::RefusedDetails {
            code: "rate_limited".into(),
            message: "the provider rate-limited the call".into(),
            detail: json!({"class": "transient", "no_spend": true}),
        }))
        .unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Rejected);
    assert_eq!(spent(&f), 0.0);

    let mut args = input();
    args["max_cost_usd"] = json!(0.10);
    f.issue(1, "llm_classify", args);
    let second = f.attempt();
    assert_eq!(second.input["max_cost_usd"], 0.10);
    second
        .reply
        .send(Err(WireError::RefusedDetails {
            code: "provider_error".into(),
            message: "the provider failed after billing".into(),
            detail: json!({"class": "transient", "cost_usd": 0.003}),
        }))
        .unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Rejected);
    assert_eq!(spent(&f), 0.003);
    f.cleanup(true);
}

fn complete_fixture() -> Fixture {
    let f = Fixture::new();
    let catalog = parse(include_str!(
        "../../../../test-vectors/llm-complete-v1/tool-result-v1/catalog.ok.json"
    ))
    .unwrap();
    let entry = catalog["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "llm_complete")
        .unwrap();
    f.catalog.set_op(
        "broca",
        "llm_complete",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    f.catalog
        .set_tool_contract("broca", "llm_complete", entry.clone());
    assert_eq!(entry["result_format"], "tool-result/v1");
    f.host.content_results.store(true, Ordering::SeqCst);
    f.start(
        &[(
            "llm_complete",
            "broca",
            "llm_complete",
            entry["input_schema"].clone(),
        )],
        Limits {
            model_cost_usd: 0.25,
            ..Limits::default()
        },
        100_000,
    );
    assert!(matches!(
        f.parent.recv_timeout(TIMEOUT).unwrap(),
        ParentMessage::Activate(_)
    ));
    f
}

fn complete_input(ceiling: f64) -> Value {
    json!({"demand":{"iq":50},"prompt":"Name a color.","max_output_tokens":500,"max_cost_usd":ceiling})
}

#[test]
fn program_complete_caps_dispatch_and_charges_the_reported_cost() {
    let f = complete_fixture();
    f.issue(0, "llm_complete", complete_input(1.0));
    let attempt = f.attempt();
    assert_eq!(attempt.input["max_cost_usd"], 0.25);
    assert_eq!(spent(&f), 0.25, "before the provider settles");
    let reply = parse(include_str!(
        "../../../../test-vectors/llm-complete-v1/tool-result-v1/text-answer.ok.json"
    ))
    .unwrap();
    attempt.reply.send(Ok(reply)).unwrap();
    let delivery = f.delivery();
    assert_eq!(delivery.settlement, Settlement::Fulfilled);
    assert_eq!(parse(delivery.value.as_str()).unwrap()["text"], "Teal.");
    f.finish("null");
    assert_eq!(f.terminal()["budgets"]["model_cost_usd"]["spent"], 0.0105);
    f.cleanup(false);
}

#[test]
fn program_complete_charges_refusals_by_the_spend_marker() {
    let f = complete_fixture();
    let no_spend = parse(include_str!(
        "../../../../test-vectors/llm-complete-v1/tool-result-v1/refused-cost-exceeded.refused.json"
    ))
    .unwrap();
    let unknown = parse(include_str!(
        "../../../../test-vectors/llm-complete-v1/tool-result-v1/refused-provider-error.refused.json"
    ))
    .unwrap();
    let mut malformed = no_spend.clone();
    malformed["detail"]["no_spend"] = json!("true");
    for (position, (reply, code, ceiling, cumulative_spend)) in [
        (no_spend, "cost_exceeded", 0.005, 0.0),
        (unknown, "provider_error", 0.0625, 0.0625),
        (
            json!({"code":"cost_unmeasured","message":"usage missing","detail":{"class":"permanent"}}),
            "cost_unmeasured",
            0.0625,
            0.125,
        ),
        (malformed, "cost_exceeded", 0.0625, 0.1875),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(reply["code"], code);
        f.issue(position as u64, "llm_complete", complete_input(ceiling));
        let attempt = f.attempt();
        assert_eq!(attempt.input["max_cost_usd"], ceiling);
        attempt
            .reply
            .send(Err(WireError::RefusedDetails {
                code: code.into(),
                message: reply["message"].as_str().unwrap().into(),
                detail: reply["detail"].clone(),
            }))
            .unwrap();
        let delivery = f.delivery();
        assert_eq!(delivery.settlement, Settlement::Rejected);
        assert_eq!(parse(delivery.value.as_str()).unwrap()["code"], code);
        assert_eq!(spent(&f), cumulative_spend, "{code}");
    }
    f.cleanup(true);
}

fn extract_fixture() -> Fixture {
    let f = Fixture::new();
    f.catalog.set_op(
        "broca",
        "llm_extract",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    f.catalog.set_tool_contract(
        "broca",
        "llm_extract",
        json!({"capabilities":[extract::TAG],"result_format":"tool-result/v1"}),
    );
    f.host.content_results.store(true, Ordering::SeqCst);
    f.start(
        &[(
            "llm_extract",
            "broca",
            "llm_extract",
            extract::input_schema(),
        )],
        Limits {
            model_cost_usd: 0.25,
            ..Limits::default()
        },
        100_000,
    );
    assert!(matches!(
        f.parent.recv_timeout(TIMEOUT).unwrap(),
        ParentMessage::Activate(_)
    ));
    f
}

fn extract_input(ceiling: f64) -> Value {
    let request: extract::Request = serde_json::from_value(json!({
        "model":"example/extractor",
        "fields":{"color":{"type":"text","instructions":"Extract the color."}},
        "item":{"state":"A teal kettle"},
        "max_cost_usd":ceiling
    }))
    .unwrap();
    extract::validate_request(&request).unwrap();
    serde_json::to_value(request).unwrap()
}

fn extract_reply(cost: f64) -> Value {
    let result = extract::Result {
        answers: [(
            "color".into(),
            serde_json::from_str(r#"{"type":"text","value":"teal"}"#).unwrap(),
        )]
        .into_iter()
        .collect(),
        truncated: None,
        state_text: None,
        usage: Usage::default(),
        cost_usd: serde_json::Number::from_f64(cost).unwrap(),
    };
    json!({
        "content":[{"type":"text","text":serde_json::to_string(&result).unwrap()}],
        "structuredContent":result
    })
}

#[test]
fn program_extract_caps_dispatch_and_charges_the_reported_cost() {
    let f = extract_fixture();
    for (position, (ceiling, cost, cumulative_spend)) in
        [(0.25, 0.0625, 0.0625), (0.1875, 0.03125, 0.09375)]
            .into_iter()
            .enumerate()
    {
        f.issue(position as u64, "llm_extract", extract_input(1.0));
        let attempt = f.attempt();
        assert_eq!(attempt.input["max_cost_usd"], ceiling);
        assert_eq!(spent(&f), 0.25, "before the provider settles");
        attempt.reply.send(Ok(extract_reply(cost))).unwrap();
        let delivery = f.delivery();
        assert_eq!(delivery.settlement, Settlement::Fulfilled);
        assert_eq!(
            parse(delivery.value.as_str()).unwrap()["answers"]["color"]["value"],
            "teal"
        );
        assert_eq!(spent(&f), cumulative_spend);
    }
    f.finish("null");
    assert_eq!(f.terminal()["budgets"]["model_cost_usd"]["spent"], 0.09375);
    f.cleanup(false);
}

#[test]
fn program_extract_charges_refusals_by_the_spend_marker() {
    let f = extract_fixture();
    let refusal = errors::refusal("provider_error", "extraction provider failed").unwrap();
    for (position, (detail, pending_spend, settled_spend)) in [
        (refusal.detail.clone().with_no_spend(), 0.0625, 0.0),
        (
            refusal.detail.clone().with_cost_usd(0.03125),
            0.0625,
            0.03125,
        ),
        (refusal.detail.clone().with_cost_unknown(), 0.09375, 0.09375),
    ]
    .into_iter()
    .enumerate()
    {
        f.issue(position as u64, "llm_extract", extract_input(0.0625));
        let attempt = f.attempt();
        assert_eq!(spent(&f), pending_spend);
        attempt
            .reply
            .send(Err(WireError::RefusedDetails {
                code: refusal.code.clone(),
                message: refusal.message.clone(),
                detail: serde_json::to_value(detail).unwrap(),
            }))
            .unwrap();
        let delivery = f.delivery();
        assert_eq!(delivery.settlement, Settlement::Rejected);
        assert_eq!(
            parse(delivery.value.as_str()).unwrap()["code"],
            refusal.code
        );
        assert_eq!(spent(&f), settled_spend);
    }
    f.cleanup(true);
}
