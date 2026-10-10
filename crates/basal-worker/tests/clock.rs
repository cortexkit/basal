//! Clock reads and random samples are host calls: journaled on the first
//! run, replayed from the journal afterwards.

mod common;

use std::time::Duration;

use basal_proto::{ActivationResult, CallKind, JsonText, Primitive, Settlement};
use serde_json::json;

const READS: &str = r#"
    return [Date.now(), new Date().getTime(), Date(), await now(), Math.random(), await random(),
        Date.prototype.constructor.now()];
"#;

#[test]
fn clock_and_random_are_journaled_and_replayed() {
    let mut parent = common::parent();
    let t0 = parent.host.clock_ms;
    let first = parent.run("t", READS);
    let kinds: Vec<CallKind> = first.host_calls.iter().map(|c| c.kind.clone()).collect();
    let now = CallKind::Primitive(Primitive::Now);
    let random = CallKind::Primitive(Primitive::Random);
    assert_eq!(
        kinds,
        vec![
            now.clone(),
            now.clone(),
            now.clone(),
            now.clone(),
            random.clone(),
            random,
            now
        ]
    );
    let value = first.value();
    // The mock clock advances 1 ms per read.
    assert_eq!(value[0], json!(t0));
    assert_eq!(value[1], json!(t0 + 1));
    assert_eq!(value[2], json!("2026-01-01T00:00:00.002Z"));
    assert_eq!(value[3], json!(t0 + 3));
    assert_eq!(value[6], json!(t0 + 4));
    for i in [4, 5] {
        let sample = value[i].as_f64().expect("a number");
        assert!((0.0..1.0).contains(&sample));
    }

    // Much later, the replay still sees the recorded values and asks the
    // host for nothing.
    parent.host.advance(Duration::from_secs(3600));
    let replay = parent.run("t", READS);
    assert!(replay.host_calls.is_empty(), "{:?}", replay.host_calls);
    assert_eq!(replay.value(), value);
}

#[test]
fn fresh_clock_read_after_a_gap_returns_the_new_time() {
    let mut parent = common::parent();
    let script = r#"
        const before = Date.now();
        await ops.call('mock', 'long', {});
        const after = Date.now();
        return [before, after];
    "#;
    let first = parent.run("t", script);
    assert_eq!(
        common::finished(&first),
        &ActivationResult::Suspended { awaited: vec![1] }
    );

    // The long call finishes an hour later.
    parent.host.advance(Duration::from_secs(3600));
    parent.complete(
        "t",
        1,
        Settlement::Fulfilled,
        JsonText::new("{}").expect("small"),
    );
    let resumed = parent.run("t", script);
    let value = resumed.value();
    let before = value[0].as_u64().expect("number");
    let after = value[1].as_u64().expect("number");
    assert!(
        after - before >= 3_600_000,
        "the read after the gap returned {after}, only {} ms after {before}",
        after - before
    );
    // Only the new read crossed to the host.
    assert_eq!(resumed.host_calls.len(), 1);

    // Replaying the whole run returns the recorded times, not today's.
    parent.host.advance(Duration::from_secs(3600));
    let replay = parent.run("t", script);
    assert!(replay.host_calls.is_empty());
    assert_eq!(replay.value(), value);
}
