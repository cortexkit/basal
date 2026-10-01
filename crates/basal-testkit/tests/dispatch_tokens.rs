//! Token reservations for model calls: reserved atomically with the call's
//! journal row, refused as a journaled rejection when they do not fit, the
//! clamped request journaled so a re-issue sends identical bytes under the
//! same `send_id`, usage applied once per `send_id`, in the window the
//! reservation was made in.

mod common;

use basal_core::tokens::{Usage, window_start};
use basal_core::{ActivationEnd, Clock, Config, RunState, Runtime};
use basal_host::{Completion, CompletionAck, HostOutcome};
use basal_proto::{JsonText, Settlement};
use basal_testkit::harness::{Point, World, run_with_cuts, test_manifest};
use common::{config, finish, result};
use serde_json::{Value, json};

fn manifest(tokens: u64, window: &str, max_output: u32) -> Value {
    let mut m = test_manifest();
    m["llm"] =
        json!({ "token_cap": { "tokens": tokens, "window": window }, "max_output": max_output });
    m["deadline"] = json!("24h");
    m
}

fn admit_under(
    rt: &Runtime,
    world: &World,
    script: &str,
    manifest: &Value,
    trigger: &str,
) -> String {
    let mut spec = world.spec_with(rt, script, manifest).expect("approve");
    spec.trigger_id = trigger.to_owned();
    rt.admit(&spec)
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned()
}

fn manual(at_ms: i64) -> (Config, Clock) {
    let clock = Clock::manual(at_ms);
    (
        Config {
            clock: clock.clone(),
            ..config()
        },
        clock,
    )
}

/// An hour boundary in 2027, plus ten minutes.
const T0: i64 = 1_798_761_600_000 + 600_000;

/// Each call reserves about 1,450 tokens (its envelope's bytes, 256 of
/// overhead and its 1,000 of output), so one fits under 2,000 and two do
/// not. Both are issued before either completes; the second must see the
/// first's reservation.
#[test]
fn concurrent_llm_calls_cannot_spend_the_same_remainder() {
    let world = World::new("tokens-concurrent");
    let (config, _clock) = manual(T0);
    let rt = world
        .runtime(std::sync::Arc::new(basal_core::NoHooks), config)
        .expect("runtime");
    let m = manifest(2_000, "1h", 1_000);
    let run_id = admit_under(
        &rt,
        &world,
        r#"
        const r = await Promise.allSettled([llm({ prompt: 'a' }), llm({ prompt: 'b' })]);
        return r.map((x) => (x.status === 'fulfilled' ? 'ok' : x.reason.data.code));
        "#,
        &m,
        "t1",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(result(&run), json!(["ok", "token_cap"]), "{run:#?}");
    assert_eq!(world.mock.broca_sends().len(), 1);
    let usage = rt
        .token_usage("flow-test", T0)
        .expect("usage")
        .expect("a window");
    assert_eq!(usage.reserved, 0, "{usage:?}");
    assert!(usage.counted() <= 2_000, "{usage:?}");
    assert!(usage.input_tokens > 0, "{usage:?}");
}

#[test]
fn llm_call_over_the_cap_is_a_journaled_rejection() {
    let world = World::new("tokens-over");
    let (config, _clock) = manual(T0);
    let rt = world
        .runtime(std::sync::Arc::new(basal_core::NoHooks), config)
        .expect("runtime");
    let m = manifest(500, "1h", 100);
    let run_id = admit_under(
        &rt,
        &world,
        r#"
        try {
            await llm({ prompt: 'too much' });
            return 'sent';
        } catch (e) {
            return e.data.code;
        }
        "#,
        &m,
        "t1",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(result(&run), json!("token_cap"), "{run:#?}");
    assert!(world.mock.broca_sends().is_empty());
    let calls = rt.calls(&run_id).expect("calls");
    let outcome = calls[0].outcome.as_ref().expect("journaled");
    assert_eq!(outcome.settlement, Settlement::Rejected);
    let audit = rt
        .store()
        .read(|c| basal_core::audit::rows(c, &run_id))
        .expect("audit");
    assert_eq!(audit[0].outcome, "token_cap");
    let usage = rt
        .token_usage("flow-test", T0)
        .expect("usage")
        .expect("window");
    assert_eq!(usage.counted(), 0, "{usage:?}");
}

/// A cut after the host answered the first send, and one before the call
/// was ever sent: recovery sends the call (again) from its journaled
/// request, so every send under its `send_id` carries the same bytes, and
/// the requested output was clamped to the manifest's ceiling in all of
/// them.
#[test]
fn reissue_after_a_cut_sends_identical_bytes_under_the_same_send_id() {
    let script = "const x = await llm({ prompt: 'x', max_output: 999999 }); return x.done;";
    for (cut, min_sends) in [
        ("HostAnswered { position: 0 }#1", 2),
        ("CallCommitted { position: 0 }#1", 1),
    ] {
        let world = World::new("tokens-reissue");
        let point = Point::parse(cut).expect("point");
        let run = run_with_cuts(&world, script, &[point], &config()).expect("runs");
        assert_eq!(run.fired, vec![true], "{cut}");
        assert_eq!(run.summary.result, Some(json!(true)), "{cut}: {run:#?}");
        let sends = world.mock.broca_sends();
        assert!(sends.len() >= min_sends, "{cut}: {sends:?}");
        assert!(
            sends.iter().all(|s| s == &sends[0]),
            "{cut}: sends differ: {sends:#?}"
        );
        assert!(
            sends[0].1.contains("\"max_output\":2000"),
            "{cut}: {sends:?}"
        );
        assert_eq!(run.summary.broca_reuse, 0, "{cut}");
        assert_eq!(run.summary.tokens.get("open"), Some(&0), "{cut}");
    }
}

fn usage_outcome(input: u64, cache_write: u64, output: u64, cached: u64) -> HostOutcome {
    let mut outcome =
        HostOutcome::fulfilled(JsonText::new(json!({"text": "hi"}).to_string()).expect("small"));
    outcome.usage = Some(basal_host::TokenUsage {
        input_tokens: Some(input),
        cache_write_tokens: Some(cache_write),
        output_tokens: Some(output),
        cached_input_tokens: Some(cached),
    });
    outcome
}

/// Usage is applied once per `send_id`: a report arriving apart from the
/// outcome, a redelivered completion and a second report change nothing
/// after the first.
#[test]
fn duplicate_usage_report_is_applied_once() {
    let world = World::new("tokens-dedup");
    let (config, _clock) = manual(T0);
    let rt = world
        .runtime(std::sync::Arc::new(basal_core::NoHooks), config)
        .expect("runtime");
    let m = manifest(1_000_000, "1h", 2_000);
    let script = "return (await llm({ prompt: 'u' })).text;";
    let run_id = admit_under(&rt, &world, script, &m, "t1");
    assert!(matches!(
        rt.resume(&run_id).expect("activation"),
        ActivationEnd::Suspended { .. }
    ));
    let send_id = rt.calls(&run_id).expect("calls")[0].idempotency_key.clone();
    let handle = world.mock.handle_for(&run_id, 0).expect("accepted");
    let outcome = usage_outcome(100, 10, 50, 1_000);
    assert!(world.mock.complete(&handle, outcome.clone()));
    let run = finish(&rt, &world, &run_id);
    assert_eq!(result(&run), json!("hi"));
    let other = Usage {
        input_tokens: 7,
        cache_write_tokens: 7,
        output_tokens: 7,
        cached_input_tokens: 7,
    };
    assert!(!rt.report_usage(&send_id, other).expect("report"));
    assert!(!rt.report_usage(&send_id, other).expect("report again"));
    let again = rt
        .complete(&Completion {
            run_id: run_id.clone(),
            position: 0,
            handle,
            outcome,
        })
        .expect("redeliver");
    assert_eq!(again, CompletionAck::Duplicate);
    let usage = rt
        .token_usage("flow-test", T0)
        .expect("usage")
        .expect("window");
    assert_eq!(
        (
            usage.reserved,
            usage.input_tokens,
            usage.cache_write_tokens,
            usage.output_tokens,
            usage.cached_input_tokens
        ),
        (0, 100, 10, 50, 1_000)
    );
    // Cached input is recorded, not counted against the cap.
    assert_eq!(usage.counted(), 160);

    // Reported apart from the outcome first: the outcome's own usage, when
    // it arrives, is the duplicate.
    let second = admit_under(&rt, &world, script, &m, "t2");
    assert!(matches!(
        rt.resume(&second).expect("activation"),
        ActivationEnd::Suspended { .. }
    ));
    let send_id = rt.calls(&second).expect("calls")[0].idempotency_key.clone();
    assert!(rt.report_usage(&send_id, other).expect("report"));
    let handle = world.mock.handle_for(&second, 0).expect("accepted");
    assert!(
        world
            .mock
            .complete(&handle, usage_outcome(100, 10, 50, 1_000))
    );
    finish(&rt, &world, &second);
    let usage = rt
        .token_usage("flow-test", T0)
        .expect("usage")
        .expect("window");
    assert_eq!(usage.input_tokens, 107, "{usage:?}");
    assert_eq!(usage.counted(), 160 + 21, "{usage:?}");
}

/// A reservation made in one window is settled in that window, even when
/// the run was suspended across a window rollover.
#[test]
fn usage_lands_in_the_reservation_window_across_suspension_and_rollover() {
    let world = World::new("tokens-window");
    let (config, clock) = manual(T0);
    let rt = world
        .runtime(std::sync::Arc::new(basal_core::NoHooks), config)
        .expect("runtime");
    let m = manifest(1_000_000, "1h", 2_000);
    let script = "return (await llm({ prompt: 'w' })).text;";
    let run_id = admit_under(&rt, &world, script, &m, "t1");
    assert!(matches!(
        rt.resume(&run_id).expect("activation"),
        ActivationEnd::Suspended { .. }
    ));
    let before = rt
        .token_usage("flow-test", T0)
        .expect("usage")
        .expect("window");
    assert!(before.reserved > 0, "{before:?}");

    // Two hours later the call completes and the run resumes.
    let later = T0 + 2 * 3_600_000;
    clock.set(later);
    assert_ne!(window_start(T0, 3_600_000), window_start(later, 3_600_000));
    let handle = world.mock.handle_for(&run_id, 0).expect("accepted");
    assert!(world.mock.complete(&handle, usage_outcome(300, 0, 40, 0)));
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");

    let first = rt
        .token_usage("flow-test", T0)
        .expect("usage")
        .expect("window");
    assert_eq!(
        (first.reserved, first.input_tokens, first.output_tokens),
        (0, 300, 40),
        "{first:?}"
    );
    let current = rt
        .token_usage("flow-test", later)
        .expect("usage")
        .expect("window");
    assert_eq!(current.counted(), 0, "{current:?}");

    // A call made now reserves in the current window.
    let next = admit_under(&rt, &world, script, &m, "t2");
    assert!(matches!(
        rt.resume(&next).expect("activation"),
        ActivationEnd::Suspended { .. }
    ));
    let current = rt
        .token_usage("flow-test", later)
        .expect("usage")
        .expect("window");
    assert!(current.reserved > 0, "{current:?}");
}
