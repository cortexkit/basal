//! The durable run rate limit and dispatch budget, auto-disable after K
//! consecutive saturated windows, operator and own-agent disable (which
//! fences a resumed run's new calls while its calls in flight settle), and
//! the per-run limits: host calls, journal bytes, and byte caps on every
//! argument and result. Time is a manual clock.

mod common;

use std::sync::Arc;
use std::time::Duration;

use basal_core::{
    ActivationEnd, Actor, Admission, Clock, Config, InstallError, NoHooks, RateLimits, RunLimits,
    RunState, Runtime, TriggerSpec,
};
use basal_testkit::harness::World;
use common::{config, finish, result};
use serde_json::json;

/// A minute boundary.
const T0: i64 = 1_798_761_600_000;

fn rt_with(world: &World, change: impl FnOnce(&mut Config)) -> (Runtime, Clock) {
    let clock = Clock::manual(T0);
    let mut config = Config {
        clock: clock.clone(),
        ..config()
    };
    change(&mut config);
    let rt = world.runtime(Arc::new(NoHooks), config).expect("runtime");
    (rt, clock)
}

fn rate(max_runs: u32, max_dispatches: u32, k: u32) -> RateLimits {
    RateLimits {
        window: Duration::from_secs(60),
        max_runs,
        max_dispatches,
        saturated_windows_to_disable: k,
    }
}

fn with_trigger(spec: &TriggerSpec, trigger: &str) -> TriggerSpec {
    TriggerSpec {
        trigger_id: trigger.to_owned(),
        ..spec.clone()
    }
}

fn admit_trigger(rt: &Runtime, world: &World, script: &str, trigger: &str) -> String {
    let spec = world.spec(rt, script).expect("approve");
    rt.admit(&with_trigger(&spec, trigger))
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned()
}

#[test]
fn run_rate_limit_and_auto_disable_after_k_saturated_windows() {
    let world = World::new("limits-autodisable");
    let (rt, clock) = rt_with(&world, |c| c.rate = rate(2, 1_000, 2));
    let spec = world.spec(&rt, "return 1;").expect("approve");
    let admit = |t: &str| rt.admit(&with_trigger(&spec, t)).expect("admit");
    assert!(matches!(admit("a1"), Admission::Admitted { .. }));
    assert!(matches!(admit("a2"), Admission::Admitted { .. }));
    assert_eq!(admit("a3"), Admission::RateLimited);
    assert!(rt.flow("flow-test").expect("flow").expect("exists").enabled);
    assert!(rt.outbox().expect("outbox").is_empty());

    // The next window saturates too: two in a row disable the flow.
    clock.advance(60_000);
    assert!(matches!(admit("b1"), Admission::Admitted { .. }));
    assert!(matches!(admit("b2"), Admission::Admitted { .. }));
    assert_eq!(admit("b3"), Admission::RateLimited);
    let flow = rt.flow("flow-test").expect("flow").expect("exists");
    assert!(!flow.enabled);
    assert_eq!(flow.disabled_by.as_deref(), Some("runtime"));
    let outbox = rt.outbox().expect("outbox");
    assert_eq!(outbox.len(), 1, "{outbox:?}");
    assert_eq!(outbox[0].0, "flow.auto_disabled");
    assert_eq!(outbox[0].2.as_deref(), Some("ALF"), "the owner is told");

    // Disabled: nothing is admitted, in this window or any later one.
    clock.advance(60_000);
    assert_eq!(admit("c1"), Admission::Disabled);
    // A trigger admitted before still deduplicates to its run.
    assert!(matches!(admit("a1"), Admission::Duplicate { .. }));
}

#[test]
fn saturated_windows_that_are_not_consecutive_do_not_disable() {
    let world = World::new("limits-streak");
    let (rt, clock) = rt_with(&world, |c| c.rate = rate(1, 1_000, 2));
    let spec = world.spec(&rt, "return 1;").expect("approve");
    let admit = |t: &str| rt.admit(&with_trigger(&spec, t)).expect("admit");
    assert!(matches!(admit("a1"), Admission::Admitted { .. }));
    assert_eq!(admit("a2"), Admission::RateLimited);
    // A window that admits within its limit breaks the streak.
    clock.advance(60_000);
    assert!(matches!(admit("b1"), Admission::Admitted { .. }));
    clock.advance(60_000);
    assert!(matches!(admit("c1"), Admission::Admitted { .. }));
    assert_eq!(admit("c2"), Admission::RateLimited);
    assert!(rt.flow("flow-test").expect("flow").expect("exists").enabled);
    // Two in a row do.
    clock.advance(60_000);
    assert!(matches!(admit("d1"), Admission::Admitted { .. }));
    assert_eq!(admit("d2"), Admission::RateLimited);
    assert!(!rt.flow("flow-test").expect("flow").expect("exists").enabled);
}

#[test]
fn dispatch_budget_refuses_beyond_the_window() {
    let world = World::new("limits-dispatch");
    let (rt, _clock) = rt_with(&world, |c| c.rate = rate(100, 2, 3));
    let run_id = admit_trigger(
        &rt,
        &world,
        r#"
        const out = [];
        for (let i = 0; i < 3; i++) {
            try { await ops.call('mock', 'echo', { i }); out.push('ok'); }
            catch (e) { out.push(e.data.code); }
        }
        return out;
        "#,
        "t1",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(
        result(&run),
        json!(["ok", "ok", "dispatch_budget"]),
        "{run:#?}"
    );
    assert_eq!(world.mock.total_sends(), 2);
}

/// A run suspended on a call in flight; the flow is disabled; the call
/// completes, which settles its obligation and resumes the run; the run's
/// new calls, local ones included, are refused, and nothing new reaches
/// the host.
#[test]
fn disable_fences_a_resumed_run_while_in_flight_calls_settle() {
    let world = World::new("limits-disable");
    let (rt, _clock) = rt_with(&world, |_| {});
    let run_id = admit_trigger(
        &rt,
        &world,
        r#"
        const first = await ops.call('mock', 'long', {});
        const codes = [];
        try { await ops.call('mock', 'echo', {}); codes.push('sent'); } catch (e) { codes.push(e.data.code); }
        try { await kv.set('k', 1); codes.push('written'); } catch (e) { codes.push(e.data.code); }
        return { first: first.done, codes, clock: Date.now() > 0 };
        "#,
        "t1",
    );
    assert!(matches!(
        rt.resume(&run_id).expect("activation"),
        ActivationEnd::Suspended { .. }
    ));
    assert!(
        rt.disable_flow("flow-test", &Actor::Operator("ufuk".into()), "testing")
            .expect("disable")
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert_eq!(
        result(&run),
        json!({"first": true, "codes": ["flow_disabled", "flow_disabled"], "clock": true})
    );
    // Only the call sent before the disable reached the host, and it
    // settled.
    assert_eq!(world.mock.total_sends(), 1);
    assert!(rt.calls(&run_id).expect("calls")[0].outcome.is_some());
    let spec = world.spec(&rt, "return 1;");
    assert!(matches!(
        spec.map(|s| rt.admit(&with_trigger(&s, "t2"))),
        Ok(Ok(Admission::Disabled)) | Err(_)
    ));
    let outbox = rt.outbox().expect("outbox");
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].0, "flow.disabled");
}

#[test]
fn only_the_operator_or_the_owning_agent_disables() {
    let world = World::new("limits-owner");
    let (rt, _clock) = rt_with(&world, |_| {});
    world.spec(&rt, "return 1;").expect("approve");
    assert_eq!(
        rt.disable_flow("flow-test", &Actor::Agent("SYNAPSE".into()), "not mine"),
        Err(InstallError::NotOwner {
            flow_id: "flow-test".into(),
            agent: "SYNAPSE".into()
        })
    );
    assert!(rt.flow("flow-test").expect("flow").expect("exists").enabled);
    assert_eq!(
        rt.disable_flow("flow-test", &Actor::Agent("ALF".into()), "mine"),
        Ok(true)
    );
    assert_eq!(
        rt.disable_flow("flow-test", &Actor::Agent("ALF".into()), "again"),
        Ok(false)
    );
    assert_eq!(rt.enable_flow("flow-test"), Ok(true));
    assert_eq!(
        rt.disable_flow("flow-test", &Actor::Operator("ufuk".into()), "operator"),
        Ok(true)
    );
}

fn limited(change: impl FnOnce(&mut RunLimits)) -> impl FnOnce(&mut Config) {
    move |c: &mut Config| change(&mut c.limits)
}

#[test]
fn per_run_call_limit_fails_the_run() {
    let world = World::new("limits-calls");
    let (rt, _clock) = rt_with(&world, limited(|l| l.max_calls = 3));
    let run_id = admit_trigger(
        &rt,
        &world,
        "for (let i = 0; i < 5; i++) { Date.now(); } return 1;",
        "t1",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Failed, "{run:#?}");
    assert_eq!(run.error_kind.as_deref(), Some("limit"));
    assert_eq!(rt.calls(&run_id).expect("calls").len(), 3);
}

#[test]
fn per_run_journal_bytes_limit_fails_the_run() {
    let world = World::new("limits-bytes");
    let (rt, _clock) = rt_with(&world, limited(|l| l.max_journal_bytes = 2_000));
    let run_id = admit_trigger(
        &rt,
        &world,
        "for (let i = 0; i < 5; i++) { await ops.call('mock', 'echo', { pad: 'x'.repeat(300) }); } return 1;",
        "t1",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Failed, "{run:#?}");
    assert_eq!(run.error_kind.as_deref(), Some("limit"));
    // Each call holds its argument and its echoed result: about 620 bytes.
    let calls = rt.calls(&run_id).expect("calls").len();
    assert!((1..5).contains(&calls), "{calls}");
}

#[test]
fn argument_over_the_cap_is_refused_before_it_is_parsed() {
    let world = World::new("limits-args");
    let (rt, _clock) = rt_with(&world, limited(|l| l.max_arg_bytes = 100));
    let run_id = admit_trigger(
        &rt,
        &world,
        r#"
        try { await ops.call('mock', 'echo', { pad: 'x'.repeat(200) }); return 'sent'; }
        catch (e) { return e.data.code; }
        "#,
        "t1",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(result(&run), json!("too_large"), "{run:#?}");
    assert_eq!(world.mock.total_sends(), 0);
}

#[test]
fn result_over_the_cap_is_recorded_as_a_rejection() {
    let world = World::new("limits-results");
    let (rt, _clock) = rt_with(&world, limited(|l| l.max_result_bytes = 100));
    let run_id = admit_trigger(
        &rt,
        &world,
        r#"
        try { await ops.call('mock', 'echo', { pad: 'x'.repeat(150) }); return 'returned'; }
        catch (e) { return e.data.code; }
        "#,
        "t1",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(result(&run), json!("result_too_large"), "{run:#?}");
    assert_eq!(world.mock.total_sends(), 1);
}
