//! Unknown outcomes, safe retry, reconcile, and completions that repeat or
//! contradict what is recorded.

mod common;

use std::sync::Arc;
use std::time::Duration;

use basal_core::{ActivationEnd, CoreError, Resolution, RunState};
use basal_host::mock::Fault;
use basal_host::{Completion, CompletionAck, HostOutcome, UnknownReason};
use basal_proto::JsonText;
use basal_testkit::harness::{Point, Probe, World};
use common::{admit, finish, result, runtime, runtime_with};
use serde_json::json;

fn text(v: serde_json::Value) -> JsonText {
    JsonText::new(v.to_string()).expect("small")
}

/// Runs `script`, simulates a crash when the runtime reaches the boundary
/// named by `cut` (see `harness::Point`), then opens a recovering runtime.
fn crash_then_recover(world: &World, script: &str, cut: &str) -> (basal_core::Runtime, String) {
    let probe = Arc::new(Probe::crash_at(Point::parse(cut).expect("point")));
    let rt = runtime_with(world, probe.clone());
    let run_id = admit(&rt, world, script);
    let _ = rt.run_to_rest(&run_id, Duration::from_secs(30));
    rt.quiesce();
    assert!(probe.fired(), "the cut was not reached");
    drop(rt);
    world.mock.detach();
    (runtime(world), run_id)
}

/// A mutation whose op ignores idempotency keys took effect, and the
/// process died before its outcome committed. Recovery cannot prove what
/// happened, so the run stops in `needs_reconcile` naming the call. An
/// operator's observed result lets it continue without a second effect.
#[test]
fn unknown_unkeyed_mutation_needs_reconcile() {
    let world = World::new("unknown-unkeyed");
    let script = "const r = await ops.call('mock', 'post', { n: 1 }); return r.applied.n;";
    let (rt, run_id) = crash_then_recover(&world, script, "HostAnswered { position: 0 }#1");
    let run = rt
        .run_to_rest(&run_id, Duration::from_secs(30))
        .expect("run");
    assert_eq!(run.state, RunState::NeedsReconcile, "{run:#?}");
    assert!(
        run.error_detail.as_deref().unwrap_or("").contains("[0]"),
        "{run:#?}"
    );
    let health = rt.health().expect("health");
    assert_eq!(health.unknown_calls, vec![(run_id.clone(), 0)]);
    assert_eq!(world.mock.effects().len(), 1);
    assert_eq!(
        rt.unknown_reason(&run_id, 0).expect("reason"),
        Some(UnknownReason::BasalRestarted)
    );

    let state = rt
        .reconcile(
            &run_id,
            0,
            Resolution::ObservedResult(HostOutcome::fulfilled(text(
                json!({"applied": {"n": 1}, "op": "post"}),
            ))),
            "operator",
        )
        .expect("reconcile");
    assert_eq!(state, RunState::Pending);
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert_eq!(result(&run), json!(1));
    assert_eq!(world.mock.effects().len(), 1, "the mutation ran twice");
}

/// The same crash with a mutation whose op honours idempotency keys: it is
/// sent again with the same key, and the host applies it once.
#[test]
fn unknown_keyed_mutation_is_reissued_with_the_same_key() {
    let world = World::new("unknown-keyed");
    let script = "const r = await ops.call('mock', 'send', { n: 1 }); return r.applied.n;";
    let (rt, run_id) = crash_then_recover(&world, script, "HostAnswered { position: 0 }#1");
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    let key = rt.calls(&run_id).expect("calls")[0].idempotency_key.clone();
    assert_eq!(
        world.mock.send_count(&key),
        2,
        "the call was not sent again"
    );
    assert_eq!(world.mock.effect_count(&key), 1);
    assert_eq!(world.mock.effects().len(), 1, "a second key was minted");
}

/// `Unavailable` is retried inside the call only for a query, an op that
/// honours idempotency keys, or a send the transport proves never left.
/// Any other mutation may have taken effect, so it goes to
/// `needs_reconcile` without a retry.
#[test]
fn unavailable_is_retried_only_when_safe() {
    let lost = Fault {
        proven_unsent: false,
        effect_applied: true,
    };
    let unsent = Fault {
        proven_unsent: true,
        effect_applied: false,
    };
    let cases: [(&str, Fault, RunState, usize); 4] = [
        ("echo", lost, RunState::Succeeded, 0),
        ("send", lost, RunState::Succeeded, 1),
        ("post", unsent, RunState::Succeeded, 1),
        ("post", lost, RunState::NeedsReconcile, 1),
    ];
    for (op, fault, expected, effects) in cases {
        let world = World::new("unavailable");
        world.mock.inject("mock", op, &[fault, fault]);
        let rt = runtime(&world);
        let run_id = admit(
            &rt,
            &world,
            &format!("const r = await ops.call('mock', '{op}', {{ n: 1 }}); return 1;"),
        );
        let run = finish(&rt, &world, &run_id);
        assert_eq!(run.state, expected, "{op} {fault:?}: {run:#?}");
        assert_eq!(world.mock.effects().len(), effects, "{op} {fault:?}");
        if expected == RunState::NeedsReconcile {
            let key = rt.calls(&run_id).expect("calls")[0].idempotency_key.clone();
            assert_eq!(
                world.mock.send_count(&key),
                1,
                "{op}: retried an unsafe mutation"
            );
            // The mock's ambiguous failure is a reply lost with its
            // connection.
            assert_eq!(
                rt.unknown_reason(&run_id, 0).expect("reason"),
                Some(UnknownReason::ConnectionLost)
            );
        }
    }
}

/// A call whose op honours idempotency keys is sent again on every
/// ambiguous failure; still ambiguous when the retries run out, it is
/// unknown with the reason `retries_exhausted`.
#[test]
fn a_keyed_call_ambiguous_through_its_retries_is_unknown_as_retries_exhausted() {
    let world = World::new("retries-exhausted");
    let lost = Fault {
        proven_unsent: false,
        effect_applied: true,
    };
    world.mock.inject("mock", "send", &[lost; 8]);
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        "await ops.call('mock', 'send', { n: 1 }); return 1;",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::NeedsReconcile, "{run:#?}");
    let key = rt.calls(&run_id).expect("calls")[0].idempotency_key.clone();
    let retries = rt.config().unavailable_retries as usize;
    assert_eq!(world.mock.send_count(&key), retries + 1);
    assert_eq!(world.mock.effect_count(&key), 1, "the key deduplicated");
    assert_eq!(
        rt.unknown_reason(&run_id, 0).expect("reason"),
        Some(UnknownReason::RetriesExhausted)
    );
}

/// An operator reconciles an unknown mutation as not applied: it is sent
/// again with the same idempotency key, and takes effect once.
#[test]
fn reconcile_not_applied_reissues_with_the_same_key() {
    let world = World::new("not-applied");
    world.mock.inject(
        "mock",
        "post",
        &[Fault {
            proven_unsent: false,
            effect_applied: false,
        }],
    );
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        "const r = await ops.call('mock', 'post', { n: 1 }); return r.applied.n;",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::NeedsReconcile, "{run:#?}");
    assert!(world.mock.effects().is_empty());
    assert_eq!(
        rt.reconcile(&run_id, 0, Resolution::NotApplied, "operator")
            .expect("reconcile"),
        RunState::Pending
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    let key = rt.calls(&run_id).expect("calls")[0].idempotency_key.clone();
    let effects = world.mock.effects();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].key, key);
    let audited = common::query_i64(
        &rt,
        "SELECT COUNT(*) FROM audit WHERE action = 'reconcile.not_applied'",
    );
    assert_eq!(audited, 1);
}

/// Reconcile `cancel` ends the run. A completion that arrives later is
/// refused and logged, which still settles the call's obligation.
#[test]
fn reconcile_cancel_ends_the_run_and_refuses_later_completions() {
    let world = World::new("cancel");
    world.mock.inject(
        "mock",
        "post",
        &[Fault {
            proven_unsent: false,
            effect_applied: true,
        }],
    );
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        r#"
        const l = ops.call('mock', 'long', {});
        await ops.call('mock', 'post', { n: 1 });
        return (await l).done;
        "#,
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::NeedsReconcile, "{run:#?}");
    assert_eq!(
        rt.reconcile(&run_id, 1, Resolution::Cancel, "operator")
            .expect("reconcile"),
        RunState::Cancelled
    );
    let handle = world.mock.handle_for(&run_id, 0).expect("accepted");
    let ack = rt
        .complete(&Completion {
            run_id: run_id.clone(),
            position: 0,
            handle,
            outcome: HostOutcome::fulfilled(text(json!({"done": true}))),
        })
        .expect("complete");
    assert_eq!(ack, CompletionAck::Refused);
    assert_eq!(rt.run(&run_id).expect("run").state, RunState::Cancelled);
    // The refused completion settled position 0; the unknown mutation at 1
    // is still an open obligation.
    let open = rt.health().expect("health").open_obligations;
    assert_eq!(open, vec![(run_id.clone(), 1)]);
    assert!(matches!(
        rt.reconcile(&run_id, 1, Resolution::NotApplied, "operator"),
        Err(CoreError::WrongState { .. })
    ));
}

/// An identical redelivered completion is a no-op; a contradictory one is
/// quarantined and never applied.
#[test]
fn redelivered_completion_is_a_noop_and_contradictory_one_is_quarantined() {
    let world = World::new("redelivery");
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        "const x = await llm({ prompt: 'p' }); return x.v;",
    );
    let first = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(first, ActivationEnd::Suspended { .. }),
        "{first:?}"
    );
    let handle = world.mock.handle_for(&run_id, 0).expect("accepted");
    let completion = |v: i64| Completion {
        run_id: run_id.clone(),
        position: 0,
        handle: handle.clone(),
        outcome: HostOutcome::fulfilled(text(json!({"v": v}))),
    };
    assert_eq!(
        rt.complete(&completion(1)).expect("first"),
        CompletionAck::Accepted
    );
    assert_eq!(
        rt.complete(&completion(1)).expect("again"),
        CompletionAck::Duplicate
    );
    assert_eq!(
        rt.complete(&completion(2)).expect("other"),
        CompletionAck::Quarantined
    );
    let mut wrong_handle = completion(1);
    wrong_handle.handle = "someone-else".into();
    assert_eq!(
        rt.complete(&wrong_handle).expect("handle"),
        CompletionAck::Quarantined
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(result(&run), json!(1));
    // Once the outcome has been released into the journal, a redelivery is
    // compared with the journal's copy.
    assert_eq!(
        rt.complete(&completion(1)).expect("late"),
        CompletionAck::Duplicate
    );
    assert_eq!(
        rt.complete(&completion(3)).expect("late other"),
        CompletionAck::Quarantined
    );
    assert_eq!(rt.health().expect("health").quarantined, 3);
}
