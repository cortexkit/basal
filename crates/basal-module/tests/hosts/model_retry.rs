use super::*;
use basal_host::broca::{StateStore, wire::RunFinishReason};

#[test]
fn transient_model_send_defers_then_succeeds_without_spending_dispatch_budget() {
    let selector = Arc::new(basal_host::selector::FakeSelector::default());
    let (f, model, fake, snapshots) =
        model_fixture("model-transient", Fake::new(), selector, Arc::new(NoHooks));
    common::install(
        &f,
        &Caller::Operator,
        "return (await llm({prompt:'hello'})).text;",
        &model_manifest(),
    );
    approve_model(&f);
    fake.refuse_next_send("transient");
    let run = common::admit(&f, "host-flow", "retry");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(
        f.module.rt.calls(&run).unwrap()[0].dispatch,
        basal_core::DispatchState::Deferred
    );
    let deferred = f.module.rt.fixture_deferral(&run, 0).unwrap();
    assert!(deferred.proven_unsent && deferred.refund_dispatch);
    assert_eq!(deferred.retry_not_before, common::T0 + 1000);
    assert_eq!(deferred.attempt, Some(1));
    let saved = snapshots.load().unwrap().remove(0);
    assert!(saved.deferred && saved.handle.is_none() && saved.outcome.is_none());
    model.poll().unwrap();
    f.clock.set(deferred.retry_not_before - 1);
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(fake.sends().len(), 1);
    assert_eq!(
        f.module.rt.flow_health().unwrap()[0].consecutive_failures,
        0
    );
    assert!(f.module.rt.flow("host-flow").unwrap().unwrap().enabled);

    f.clock.set(deferred.retry_not_before);
    f.module.engine.run_until_idle(50).unwrap();
    let row = f.module.rt.calls(&run).unwrap()[0].clone();
    assert_eq!(row.attempts, 2);
    let sends = fake.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0], sends[1], "retry uses frozen bytes and send id");
    fake.finish(
        &row.idempotency_key,
        "answer",
        RunFinishReason::Completed,
        None,
    )
    .unwrap();
    model.poll().unwrap();
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
    assert_eq!(
        f.module.rt.run(&run).unwrap().result.as_deref(),
        Some("\"answer\"")
    );
    let dispatches: i64 = f
        .module
        .rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(dispatches),0) FROM rate_windows WHERE flow_id='host-flow'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        dispatches, 1,
        "only the accepted attempt consumes rate budget"
    );
}

#[test]
fn transient_model_send_keeps_retrying_until_typed_deadline_refusal() {
    let selector = Arc::new(basal_host::selector::FakeSelector::default());
    let (f, model, fake, snapshots) = model_fixture(
        "model-transient-deadline",
        Fake::new(),
        selector,
        Arc::new(NoHooks),
    );
    common::install(
        &f,
        &Caller::Operator,
        "return await llm({prompt:'hello'});",
        &model_manifest(),
    );
    approve_model(&f);
    let run = common::admit(&f, "host-flow", "deadline");
    let mut times = Vec::new();
    for attempt in 1..=15 {
        times.push(f.clock.now_ms() - common::T0);
        fake.refuse_next_send("transient");
        f.module.engine.run_until_idle(50).unwrap();
        let row = &f.module.rt.calls(&run).unwrap()[0];
        assert_eq!(row.attempts, attempt);
        assert_eq!(row.dispatch, basal_core::DispatchState::Deferred);
        let deferred = f.module.rt.fixture_deferral(&run, 0).unwrap();
        assert!(deferred.proven_unsent && deferred.refund_dispatch);
        assert_eq!(
            f.module
                .rt
                .fixture_refusal(&run, 0)
                .unwrap()
                .reason
                .as_str(),
            "transient"
        );
        assert_eq!(
            f.module.rt.flow_health().unwrap()[0].consecutive_failures,
            0
        );
        assert!(f.module.rt.flow("host-flow").unwrap().unwrap().enabled);
        f.clock.set(deferred.retry_not_before);
    }
    assert_eq!(
        times,
        [
            0, 1000, 3000, 7000, 15000, 31000, 63000, 123000, 183000, 243000, 303000, 363000,
            423000, 483000, 543000
        ]
    );
    assert_eq!(f.clock.now_ms(), common::T0 + 600_000);
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Failed);
    assert_eq!(done.error_kind.as_deref(), Some("deadline"));
    let call = f.module.rt.fixture_call_state(&run, 0).unwrap();
    assert!(call.unknown.is_none());
    assert!(f.module.rt.calls(&run).unwrap()[0].handle.is_none());
    let value: Value = serde_json::from_str(call.arrival.unwrap().value.as_str()).unwrap();
    assert_eq!(value["code"], "transient");
    assert_eq!(value["module"], "broca");
    assert_eq!(value["action"], "session.send");
    assert_eq!(fake.sends().len(), 15);
    assert!(snapshots.load().unwrap()[0].deferred);
    model.poll().unwrap();
    assert_eq!(
        fake.sends().len(),
        15,
        "the poller never sends deferred calls"
    );
    let dispatches: i64 = f
        .module
        .rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(dispatches),0) FROM rate_windows WHERE flow_id='host-flow'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(dispatches, 0);
}

#[test]
fn nonretryable_model_send_codes_reject_immediately() {
    for code in ["variant_unsupported", "open_failed"] {
        let selector = Arc::new(basal_host::selector::FakeSelector::default());
        let (f, _model, fake, _) = model_fixture(code, Fake::new(), selector, Arc::new(NoHooks));
        common::install(
            &f,
            &Caller::Operator,
            "return await llm({prompt:'hello'}).catch(e => e.data.code);",
            &model_manifest(),
        );
        approve_model(&f);
        fake.refuse_next_send(code);
        let run = common::admit(&f, "host-flow", code);
        f.module.engine.run_until_idle(50).unwrap();
        let done = f.module.rt.run(&run).unwrap();
        assert_eq!(done.state, RunState::Succeeded);
        assert_eq!(
            done.result.as_deref(),
            Some(json!(code).to_string().as_str())
        );
        assert_eq!(fake.sends().len(), 1);
        assert_eq!(f.module.rt.calls(&run).unwrap()[0].attempts, 1);
    }
}

#[test]
fn account_model_send_wait_honors_retry_hint_then_succeeds() {
    for code in ["account_selection_refused", "account_limit_exhausted"] {
        let selector = Arc::new(basal_host::selector::FakeSelector::default());
        let (f, model, fake, _) = model_fixture(code, Fake::new(), selector, Arc::new(NoHooks));
        common::install(
            &f,
            &Caller::Operator,
            "return (await llm({prompt:'hello'})).text;",
            &model_manifest(),
        );
        approve_model(&f);
        fake.refuse_next_send_with_data(code, json!({"retry_after_s":2.5}));
        let run = common::admit(&f, "host-flow", code);
        f.module.engine.run_until_idle(50).unwrap();
        let deferred = f.module.rt.fixture_deferral(&run, 0).unwrap();
        assert_eq!(deferred.retry_not_before, common::T0 + 2500);
        assert!(deferred.proven_unsent && deferred.refund_dispatch);
        f.clock.set(common::T0 + 2499);
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(fake.sends().len(), 1);
        f.clock.set(deferred.retry_not_before);
        f.module.engine.run_until_idle(50).unwrap();
        let key = f.module.rt.calls(&run).unwrap()[0].idempotency_key.clone();
        let sends = fake.sends();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0], sends[1]);
        fake.finish(&key, "answer", RunFinishReason::Completed, None)
            .unwrap();
        model.poll().unwrap();
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
    }
}

#[test]
fn account_model_send_hint_is_capped_at_typed_deadline() {
    for code in ["account_selection_refused", "account_limit_exhausted"] {
        let selector = Arc::new(basal_host::selector::FakeSelector::default());
        let (f, _, fake, _) = model_fixture(code, Fake::new(), selector, Arc::new(NoHooks));
        common::install(
            &f,
            &Caller::Operator,
            "return await llm({prompt:'hello'});",
            &model_manifest(),
        );
        approve_model(&f);
        fake.refuse_next_send_with_data(code, json!({"retry_after_s":1e100}));
        let run = common::admit(&f, "host-flow", code);
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(
            f.module
                .rt
                .fixture_deferral(&run, 0)
                .unwrap()
                .retry_not_before,
            common::T0 + 600_000
        );
        f.clock.set(common::T0 + 600_000);
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(
            f.module.rt.run(&run).unwrap().error_kind.as_deref(),
            Some("deadline")
        );
        let call = f.module.rt.fixture_call_state(&run, 0).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(call.arrival.unwrap().value.as_str()).unwrap()["code"],
            code
        );
        assert_eq!(fake.sends().len(), 1);
    }
}

#[test]
fn account_model_send_invalid_hint_uses_normal_backoff() {
    for hint in [json!(-1), json!("2"), json!(null)] {
        let selector = Arc::new(basal_host::selector::FakeSelector::default());
        let (f, _, fake, _) =
            model_fixture("account-invalid", Fake::new(), selector, Arc::new(NoHooks));
        common::install(
            &f,
            &Caller::Operator,
            "return await llm({prompt:'hello'});",
            &model_manifest(),
        );
        approve_model(&f);
        fake.refuse_next_send_with_data("account_selection_refused", json!({"retry_after_s":hint}));
        let run = common::admit(&f, "host-flow", "invalid");
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(
            f.module
                .rt
                .fixture_deferral(&run, 0)
                .unwrap()
                .retry_not_before,
            common::T0 + 1000
        );
    }
}
