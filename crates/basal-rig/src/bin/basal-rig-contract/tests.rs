use super::*;

#[test]
fn model_abort_records_every_named_case_without_losing_completed_evidence() {
    let mut completed = Case::new(MODEL_CASES[0]);
    completed.check("completed check", true, json!("saved"));
    let mut cases = vec![completed];
    report_model_abort(&mut cases, "unreadable checkpoint");
    assert_eq!(cases.len(), MODEL_CASES.len());
    assert!(cases[0].passed());
    for name in MODEL_CASES {
        assert_eq!(cases.iter().filter(|case| case.name == name).count(), 1);
    }
    assert!(cases[1..].iter().all(|case| {
        case.not_run
            .as_ref()
            .is_some_and(|why| why.contains("unreadable checkpoint"))
    }));
}

#[test]
fn failed_store_read_is_not_empty_evidence() {
    let mut case = Case::new("required store read");
    let records = rows::<Call>(&mut case, "read journal", Err("unreadable store".into()));
    case.check("no call was recorded", records.is_empty(), Value::Null);
    assert!(!case.passed(), "unreadable evidence was treated as absence");
}

#[test]
fn status_receipts_must_match_the_agent_and_revision() {
    let flow = flows::sinks("flow", "Agent", "Other");
    let call = Call {
        run_id: "run".into(),
        position: 0,
        kind_code: KIND_SINK_STATUS,
        dispatch: "sent".into(),
        attempts: 1,
        settlement: Some("fulfilled".into()),
        value: Some(json!({"accepted_revision": 17})),
        request: Some(json!({"agent":"Agent", "revision":17})),
    };
    for key in [
        json!(["sink.status", "flow", "Other", 17]),
        json!(["sink.status", "flow", "Agent", 99]),
    ] {
        let mut case = Case::new("status receipt identity");
        check_receipts(
            &mut case,
            &flow,
            std::slice::from_ref(&call),
            &[Receipt {
                key,
                reply: json!({"accepted_revision": 17}),
            }],
        );
        assert!(
            !case.passed(),
            "a different agent or requested revision was accepted"
        );
    }
    let mut case = Case::new("matching receipt");
    check_receipts(
        &mut case,
        &flow,
        &[call],
        &[Receipt {
            key: json!(["sink.status", "flow", "Agent", 17]),
            reply: json!({"accepted_revision":17}),
        }],
    );
    assert!(case.passed());
}
