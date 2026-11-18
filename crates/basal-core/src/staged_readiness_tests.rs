use super::*;
use basal_host::flow_refusal::{FlowRefusal, RefusalReason};

fn refusal(reason: RefusalReason) -> FlowRefusal {
    let mut refusal = FlowRefusal::new(reason, "test", "query");
    refusal.detail = Some(json!({"targets":["core"],"source":"scope"}));
    refusal
}

fn query() -> HostCall {
    let WorkerMessage::HostCall(call) = op(0, "query", json!({})) else {
        unreachable!()
    };
    call
}

fn begin(f: &Fixture, extra: Value) -> String {
    f.install("scope-readiness", 1, extra);
    f.admit("scope-readiness", "first")
}

fn wait(f: &Fixture, id: &str, first: bool) {
    let mut messages = Vec::new();
    if first {
        messages.push(WorkerMessage::HostCall(query()));
    }
    messages.extend([
        WorkerMessage::Blocked { awaiting: vec![0] },
        suspended(vec![0]),
    ]);
    let mut worker = ScriptedWorker::new(messages);
    assert_eq!(
        f.rt.activate(id, &mut worker).unwrap(),
        ActivationEnd::Suspended { awaited: vec![0] }
    );
    assert!(
        !worker
            .sent
            .iter()
            .any(|m| matches!(m, ParentMessage::Deliver(_)))
    );
    let deferred = f.rt.fixture_deferral(id, 0).unwrap();
    assert!(deferred.proven_unsent && deferred.refund_dispatch);
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
}

fn resume(f: &Fixture, id: &str) {
    let mut worker = ScriptedWorker::new(vec![
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    assert!(matches!(
        f.rt.activate(id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
}

#[test]
fn scope_readiness_preparation_retries_then_succeeds_once() {
    let f = Fixture::new(true);
    let id = begin(&f, json!({}));
    *f.host.readiness.lock().unwrap() = Some(refusal(RefusalReason::NoFlowScope));
    assert!(matches!(
        f.rt.fixture_prepare(&id, &query()).unwrap(),
        Prepared::Deferred { .. }
    ));
    wait(&f, &id, true);
    assert!(f.host.sent.lock().unwrap().is_empty());
    *f.host.readiness.lock().unwrap() = None;
    f.clock.advance(10);
    resume(&f, &id);
    assert_eq!(f.host.sent.lock().unwrap().len(), 1);
    assert!(f.host.refused.lock().unwrap().is_empty());
    let call = f.rt.fixture_call_state(&id, 0).unwrap();
    assert_eq!(call.arrival.unwrap().outcome, "fulfilled");
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
}

#[test]
fn scope_readiness_dispatch_expires_with_original_refusal_and_no_penalty() {
    for reason in [
        RefusalReason::NoFlowScope,
        RefusalReason::TargetFlowUnsupported,
    ] {
        let f = Fixture::new(true);
        let id = begin(&f, json!({}));
        let refusal = refusal(reason);
        f.host.transports.lock().unwrap().extend([
            Err(TransportError::Refused(refusal.clone())),
            Err(TransportError::Refused(refusal.clone())),
        ]);
        let start = f.clock.now_ms();
        wait(&f, &id, true);
        f.clock.advance(59_999);
        wait(&f, &id, false);
        assert_eq!(
            f.rt.fixture_deferral(&id, 0).unwrap().retry_not_before,
            start + 60_000
        );
        f.clock.advance(1);
        f.host.transports.lock().unwrap().clear();
        let mut worker = ScriptedWorker::new(vec![
            WorkerMessage::Blocked { awaiting: vec![0] },
            finished(),
        ]);
        assert!(matches!(
            f.rt.activate(&id, &mut worker).unwrap(),
            ActivationEnd::Succeeded { .. }
        ));
        let call = f.rt.fixture_call_state(&id, 0).unwrap();
        let arrival = call.arrival.unwrap();
        assert_eq!(arrival.outcome, "rejected");
        assert_eq!(arrival.value, refusal.outcome().value);
        assert!(
            call.prior_deferrals
                .iter()
                .all(|d| d.proven_unsent && d.refund_dispatch)
        );
        assert_eq!(
            f.host.sent.lock().unwrap().len(),
            2,
            "a ready provider must not be called after grace expires"
        );
        assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
        assert!(f.rt.staged_snapshot(&id).unwrap().deadline_exempt);
    }
}

#[test]
fn scope_readiness_cutoff_does_not_erase_a_later_ambiguous_send() {
    let f = Fixture::new(true);
    let id = begin(&f, json!({"ops":[{"module":"test","op":"keyed"}]}));
    f.host.transports.lock().unwrap().extend([
        Err(TransportError::Refused(refusal(RefusalReason::NoFlowScope))),
        Err(TransportError::Unavailable {
            sent: basal_host::Sent::Maybe(basal_host::UnknownReason::ConnectionLost),
            detail: "connection lost after sending".into(),
        }),
    ]);
    for first in [true, false] {
        let mut messages = Vec::new();
        if first {
            messages.push(op(0, "keyed", json!({})));
        }
        messages.extend([
            WorkerMessage::Blocked { awaiting: vec![0] },
            suspended(vec![0]),
        ]);
        let mut worker = ScriptedWorker::new(messages);
        assert_eq!(
            f.rt.activate(&id, &mut worker).unwrap(),
            ActivationEnd::Suspended { awaited: vec![0] }
        );
        f.clock.advance(10);
    }
    f.clock.advance(60_000);
    let mut worker = ScriptedWorker::new(vec![
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    let end = f.rt.activate(&id, &mut worker).unwrap();
    assert!(
        matches!(end, ActivationEnd::NeedsReconcile { .. }),
        "{end:?}"
    );
    let call = f.rt.fixture_call_state(&id, 0).unwrap();
    assert!(call.arrival.is_none());
    assert!(call.unknown.is_some());
    assert_eq!(f.host.sent.lock().unwrap().len(), 2);
}

#[test]
fn scope_readiness_run_deadline_caps_wait_without_auto_disable_count() {
    for preparing in [false, true] {
        let f = Fixture::new(true);
        let id = begin(&f, json!({"deadline":"1s"}));
        let refusal = refusal(RefusalReason::NoFlowScope);
        if preparing {
            *f.host.readiness.lock().unwrap() = Some(refusal.clone());
        } else {
            f.host.transports.lock().unwrap().extend([
                Err(TransportError::Refused(refusal.clone())),
                Err(TransportError::Refused(refusal.clone())),
            ]);
        }
        let start = f.clock.now_ms();
        wait(&f, &id, true);
        f.clock.advance(995);
        wait(&f, &id, false);
        assert_eq!(
            f.rt.fixture_deferral(&id, 0).unwrap().retry_not_before,
            start + 1_000
        );
        f.clock.advance(5);
        assert_eq!(f.rt.enforce_deadlines().unwrap(), vec![id.clone()]);
        let call = f.rt.fixture_call_state(&id, 0).unwrap();
        let value: Value = serde_json::from_str(call.arrival.unwrap().value.as_str()).unwrap();
        assert_eq!(value["code"], "no_flow_scope");
        assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
    }
}

#[test]
fn scope_readiness_restart_keeps_first_refusal_time() {
    let f = Fixture::new(false);
    let id = begin(&f, json!({}));
    *f.host.readiness.lock().unwrap() = Some(refusal(RefusalReason::NoFlowScope));
    let start = f.clock.now_ms();
    wait(&f, &id, true);
    f.clock.advance(30_000);
    wait(&f, &id, false);
    let before = f.rt.fixture_deferral(&id, 0).unwrap();
    f.rt.stop_for_recovery();
    f.rt.quiesce();
    f.rt.store().cut();
    let Fixture {
        rt,
        root,
        host,
        catalog,
        clock,
    } = f;
    drop(rt);
    let store = Arc::new(Store::open(root.join("store.db"), Durability::default()).unwrap());
    let rt = Runtime::new(
        store,
        host.clone(),
        catalog.clone(),
        Arc::new(NoHooks),
        None,
        config(&clock),
    )
    .unwrap();
    let f = Fixture {
        rt,
        root,
        host,
        catalog,
        clock,
    };
    assert_eq!(f.rt.fixture_deferral(&id, 0).unwrap(), before);
    f.clock.advance(29_999);
    wait(&f, &id, false);
    assert_eq!(
        f.rt.fixture_deferral(&id, 0).unwrap().retry_not_before,
        start + 60_000
    );
    f.clock.advance(1);
    let WorkerMessage::HostCall(query) = op(0, "query", json!({})) else {
        unreachable!()
    };
    let Prepared::Refused { value, .. } = f.rt.fixture_prepare(&id, &query).unwrap() else {
        panic!("restart extended the readiness grace");
    };
    let value: Value = serde_json::from_str(value.as_str()).unwrap();
    assert_eq!(value["code"], "no_flow_scope");
}

#[test]
fn scope_readiness_legacy_cause_starts_at_its_original_audit_time() {
    let f = Fixture::new(true);
    let id = begin(&f, json!({}));
    *f.host.readiness.lock().unwrap() = Some(refusal(RefusalReason::TargetFlowUnsupported));
    let start = f.clock.now_ms();
    wait(&f, &id, true);
    f.rt.fixture_update_state(&id, |state| {
        let deferred = state.calls.get_mut(&0).unwrap().deferred.as_mut().unwrap();
        let mut cause: Value = serde_json::from_str(&deferred.cause).unwrap();
        assert_eq!(
            cause.as_object_mut().unwrap().remove("readiness_since_ms"),
            Some(json!(start))
        );
        deferred.cause = cause.to_string();
        let decoded: FlowRefusal = serde_json::from_str(&deferred.cause).unwrap();
        assert_eq!(decoded.reason, RefusalReason::TargetFlowUnsupported);
    })
    .unwrap();
    f.clock.advance(59_999);
    wait(&f, &id, false);
    let deferred = f.rt.fixture_deferral(&id, 0).unwrap();
    let cause: Value = serde_json::from_str(&deferred.cause).unwrap();
    assert_eq!(cause["readiness_since_ms"], start);
    assert_eq!(deferred.retry_not_before, start + 60_000);
    // Production's existing FlowRefusal decoder ignores the extra JSON member.
    let decoded: FlowRefusal = serde_json::from_str(&deferred.cause).unwrap();
    assert_eq!(decoded.reason, RefusalReason::TargetFlowUnsupported);
    f.clock.advance(1);
    assert!(matches!(
        f.rt.fixture_prepare(&id, &query()).unwrap(),
        Prepared::Refused { .. }
    ));
}
