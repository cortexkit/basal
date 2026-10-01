//! Replay of the recorded prefix inside the worker.

mod common;

use basal_proto::{
    ActivationResult, ArgsDigest, CallKind, CallSignature, JsonText, Nondeterminism, Primitive,
    Settlement,
};
use serde_json::json;

#[test]
fn replay_crosses_the_channel_only_for_new_calls() {
    let mut parent = common::parent();
    let script = r#"
        const a = await ops.call('mock', 'echo', { n: 1 });
        const b = await facts('BASAL');
        const c = await ops.call('mock', 'echo', { n: 3 });
        const d = await kv.get('k');
        return [a, b, c, d];
    "#;
    let first = parent.run("t", script);
    assert_eq!(first.host_calls.len(), 4);
    let expected = first.value();

    let full = parent.run("t", script);
    assert!(full.host_calls.is_empty(), "{:?}", full.host_calls);
    assert_eq!(full.value(), expected);

    // Cut the journal after two calls: only the last two cross again.
    parent.journal_mut("t").truncate(2);
    let cut = parent.run("t", script);
    let positions: Vec<u64> = cut.host_calls.iter().map(|c| c.position).collect();
    assert_eq!(positions, vec![2, 3]);
    assert_eq!(cut.value(), expected);
}

#[test]
fn recorded_outcomes_are_released_in_delivery_order() {
    let mut parent = common::parent();
    let script = r#"
        const seen = [];
        await Promise.all([
            ops.call('mock', 'echo', { id: 'a', delay: 5 }).then((v) => seen.push(v.id)),
            ops.call('mock', 'echo', { id: 'b', delay: 1 }).then((v) => seen.push(v.id)),
            ops.call('mock', 'echo', { id: 'c', delay: 3 }).then((v) => seen.push(v.id)),
        ]);
        return seen;
    "#;
    let first = parent.run("t", script);
    assert_eq!(first.value(), json!(["b", "c", "a"]));
    let replay = parent.run("t", script);
    assert!(replay.host_calls.is_empty());
    assert_eq!(replay.value(), json!(["b", "c", "a"]));
}

/// Releasing recorded outcomes in issue order would let the slow call win a
/// race it lost originally, and the next call would diverge.
#[test]
fn race_and_any_keep_their_winner_on_replay() {
    for combinator in ["race", "any"] {
        let mut parent = common::parent();
        let script = format!(
            "const winner = await Promise.{combinator}([
                ops.call('mock', 'echo', {{ id: 'slow', delay: 8 }}),
                ops.call('mock', 'echo', {{ id: 'fast', delay: 0 }})]);
             return await ops.call('mock', 'echo', winner.id);"
        );
        let first = parent.run("t", &script);
        assert_eq!(first.value(), json!("fast"), "{combinator}");
        let replay = parent.run("t", &script);
        assert!(replay.host_calls.is_empty(), "{combinator}");
        assert_eq!(replay.value(), json!("fast"), "{combinator}");
    }
}

fn replays_identically(script: &str) -> serde_json::Value {
    let mut parent = common::parent();
    let first = parent.run("t", script);
    let value = first.value();
    let replay = parent.run("t", script);
    assert!(replay.host_calls.is_empty(), "{:?}", replay.host_calls);
    assert_eq!(replay.value(), value);
    value
}

#[test]
fn caught_rejection_replays_as_a_rejection() {
    let value = replays_identically(
        r#"
        let first;
        try {
            first = await ops.call('mock', 'fail', { message: 'nope' });
        } catch (e) {
            first = ['caught', e.name, e.message, e.data.code];
        }
        return [first, await ops.call('mock', 'echo', 'after')];
        "#,
    );
    assert_eq!(
        value,
        json!([["caught", "HostError", "nope", "denied"], "after"])
    );
}

#[test]
fn all_rejected_any_replays_in_order() {
    let value = replays_identically(
        r#"
        try {
            await Promise.any([
                ops.call('mock', 'fail', { message: 'first', delay: 5 }),
                ops.call('mock', 'fail', { message: 'second', delay: 1 }),
            ]);
            return 'fulfilled';
        } catch (e) {
            return [e.name, e.errors.map((x) => x.message)];
        }
        "#,
    );
    assert_eq!(value, json!(["AggregateError", ["first", "second"]]));
}

#[test]
fn early_all_rejection_replays_with_calls_in_flight() {
    let value = replays_identically(
        r#"
        try {
            await Promise.all([
                ops.call('mock', 'echo', { id: 'slow', delay: 8 }),
                ops.call('mock', 'fail', { message: 'early', delay: 1 }),
                ops.call('mock', 'echo', { id: 'mid', delay: 4 }),
            ]);
            return 'all fulfilled';
        } catch (e) {
            return ['caught', await ops.call('mock', 'echo', { after: e.message })];
        }
        "#,
    );
    assert_eq!(value, json!(["caught", {"after": "early"}]));
}

#[test]
fn divergent_call_fails_with_a_typed_error() {
    let mut parent = common::parent();
    parent
        .run("t", "return await ops.call('mock', 'echo', 1)")
        .value();
    let report = parent.run("t", "return await ops.call('mock', 'echo', 2)");
    let op = CallKind::Op {
        module: "mock".into(),
        op: "echo".into(),
    };
    assert_eq!(
        common::nondeterminism(&report),
        &Nondeterminism::Divergence {
            position: 0,
            recorded: CallSignature {
                kind: op.clone(),
                args_digest: ArgsDigest::of(&JsonText::new("1").expect("small")),
            },
            observed: CallSignature {
                kind: op,
                args_digest: ArgsDigest::of(&JsonText::new("2").expect("small")),
            },
        }
    );
    assert!(
        report.host_calls.is_empty(),
        "the divergent call crossed the channel"
    );

    // A different kind at the same position diverges too.
    let report = parent.run("t", "return await facts('x')");
    match common::nondeterminism(&report) {
        Nondeterminism::Divergence {
            position: 0,
            recorded,
            observed,
        } => {
            assert_eq!(recorded.kind.to_string(), "op (mock, echo)");
            assert_eq!(observed.kind, CallKind::Primitive(Primitive::Facts));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn unreplayed_journal_entries_are_detected() {
    let mut parent = common::parent();
    parent
        .run(
            "t",
            "await ops.call('mock', 'echo', 1); await ops.call('mock', 'echo', 2); return 1",
        )
        .value();
    let report = parent.run("t", "await ops.call('mock', 'echo', 1); return 1");
    assert_eq!(
        common::nondeterminism(&report),
        &Nondeterminism::UnreleasedOutcome {
            position: 1,
            delivery_order: 1
        }
    );
}

#[test]
fn blocked_only_on_long_calls_suspends_and_resumes() {
    let mut parent = common::parent();
    parent.deadline = std::time::Duration::from_secs(5);
    let script = r#"
        const a = await ops.call('mock', 'echo', 'before');
        const [answer, other] = await Promise.all([llm({ prompt: 'hi' }), ops.call('mock', 'long', {})]);
        return [a, answer, other];
    "#;
    let first = parent.run("t", script);
    assert_eq!(
        common::finished(&first),
        &ActivationResult::Suspended {
            awaited: vec![1, 2]
        }
    );
    parent.complete(
        "t",
        1,
        Settlement::Fulfilled,
        JsonText::new("\"done\"").expect("small"),
    );
    let partial = parent.run("t", script);
    assert_eq!(
        common::finished(&partial),
        &ActivationResult::Suspended { awaited: vec![2] }
    );
    assert!(partial.host_calls.is_empty());
    parent.complete(
        "t",
        2,
        Settlement::Fulfilled,
        JsonText::new("7").expect("small"),
    );
    let resumed = parent.run("t", script);
    assert_eq!(resumed.value(), json!(["before", "done", 7]));
}
