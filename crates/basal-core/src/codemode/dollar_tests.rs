use super::*;

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
        .send(Ok(json!({"answers":{},"usage":{},"cost_usd":0.42})))
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
