//! Release order, the replay barrier, suspension and concurrency, driven
//! through basal-core's runtime (store, driver and dispatch) on a real
//! worker.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use basal_core::{ActivationEnd, Boundary, RunState, Step};
use basal_host::HostOutcome;
use basal_proto::JsonText;
use basal_testkit::harness::{FnHooks, Point, Probe, World, run_with_cuts};
use common::{admit, config, finish, query_i64, result, runtime, runtime_with, sql};
use serde_json::json;

fn done_value() -> HostOutcome {
    HostOutcome::fulfilled(JsonText::new("{\"done\":true}").expect("small"))
}

/// The race cut that shows why the replay barrier exists:
/// positions 0 and 1 race, 1 wins and is
/// released, and the process dies before the winner's continuation issues
/// position 2. Position 0 completes during recovery. Shipping it in the
/// prefix with an order above 1's would make the worker see position 2
/// issued while position 0's release is still queued (`UnreleasedOutcome`);
/// it must wait in the mailbox until the worker reports itself blocked.
#[test]
fn race_cut_late_outcome_waits_for_the_barrier() {
    let world = World::new("race-cut");
    let script = r#"
        const w = await Promise.race([
            ops.call('mock', 'long', { tag: 'slow' }),
            ops.call('mock', 'echo', { tag: 'fast' }),
        ]);
        const next = await ops.call('mock', 'echo', { after: w.tag });
        return { winner: w.tag, next: next.after };
    "#;
    let cut = Point::parse("OrderCommitted { position: 1 }#1").expect("point");
    let probe = Arc::new(Probe::crash_at(cut));
    let rt = runtime_with(&world, probe.clone());
    let run_id = admit(&rt, &world, script);
    let _ = rt.run_to_rest(&run_id, Duration::from_secs(30));
    rt.quiesce();
    assert!(probe.fired(), "the cut was not reached");
    drop(rt);
    world.mock.detach();

    // Recovery: position 0 completes as soon as the new activation claims
    // the run, before its prefix is built.
    let mock = world.mock.clone();
    let completed = Arc::new(AtomicBool::new(false));
    let flag = completed.clone();
    let hooks = FnHooks(move |run_id: &str, b: &Boundary| {
        if matches!(b, Boundary::Claimed { .. }) && !flag.swap(true, Ordering::SeqCst) {
            let handle = mock.handle_for(run_id, 0).expect("position 0 was accepted");
            assert!(mock.complete(
                &handle,
                HostOutcome::fulfilled(JsonText::new("{\"tag\":\"slow\"}").expect("small"))
            ));
        }
        Step::Continue
    });
    let rt = runtime_with(&world, Arc::new(hooks));
    let calls = rt.calls(&run_id).expect("calls");
    assert_eq!(calls.len(), 2, "the cut left positions 0 and 1 only");
    assert!(calls[0].outcome.is_none());
    assert_eq!(calls[1].outcome.as_ref().map(|o| o.delivery_order), Some(0));

    let run = finish(&rt, &world, &run_id);
    assert!(completed.load(Ordering::SeqCst));
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert_eq!(result(&run), json!({"winner": "fast", "next": "fast"}));
    let calls = rt.calls(&run_id).expect("calls");
    let orders: Vec<_> = calls
        .iter()
        .map(|c| c.outcome.as_ref().map(|o| o.delivery_order))
        .collect();
    // Position 0's outcome was released after the replay, above 1's order.
    assert_eq!(orders[1], Some(0));
    assert!(orders[0] > Some(0), "{orders:?}");
}

/// A completion lands after the worker reported `Suspended` (and dropped
/// its VM) but before the run left `running`. The suspend transition
/// re-reads the mailbox and the readiness sequence, so the run becomes
/// runnable instead of sleeping on an outcome it already has.
#[test]
fn completion_after_suspended_report_is_not_lost() {
    let world = World::new("lost-wakeup");
    let mock = world.mock.clone();
    let once = Arc::new(AtomicBool::new(false));
    let flag = once.clone();
    let hooks = FnHooks(move |run_id: &str, b: &Boundary| {
        if matches!(b, Boundary::EndingReceived) && !flag.swap(true, Ordering::SeqCst) {
            let handle = mock.handle_for(run_id, 0).expect("accepted");
            assert!(mock.complete(&handle, done_value()));
        }
        Step::Continue
    });
    let rt = runtime_with(&world, Arc::new(hooks));
    let run_id = admit(
        &rt,
        &world,
        "const x = await llm({ prompt: 'p' }); return x.done;",
    );
    let end = rt.resume(&run_id).expect("activation");
    assert!(
        once.load(Ordering::SeqCst),
        "the worker never reported an ending"
    );
    assert_eq!(end, ActivationEnd::Requeued);
    assert_eq!(rt.run(&run_id).expect("run").state, RunState::Pending);
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(result(&run), json!(true));
}

/// Two long calls are outstanding. One completes and makes the run
/// runnable; the other completes while the new activation's worker is
/// replaying. It waits in the mailbox and is released after the replay.
#[test]
fn completion_during_replay_is_released_after_the_prefix() {
    let world = World::new("during-replay");
    let script = r#"
        const a = ops.call('mock', 'long', { tag: 'a' });
        const b = ops.call('mock', 'long', { tag: 'b' });
        const xs = [];
        for (let i = 0; i < 20; i++) {
            xs.push((await ops.call('mock', 'echo', { i })).i);
        }
        const vb = await b;
        const va = await a;
        return { n: xs.length, a: va.done, b: vb.done };
    "#;
    let mock = world.mock.clone();
    let activations = Arc::new(AtomicUsize::new(0));
    let count = activations.clone();
    let hooks = FnHooks(move |run_id: &str, b: &Boundary| {
        if matches!(b, Boundary::ActivateSent { .. }) && count.fetch_add(1, Ordering::SeqCst) == 1 {
            let handle = mock.handle_for(run_id, 0).expect("a accepted");
            assert!(mock.complete(&handle, done_value()));
        }
        Step::Continue
    });
    let rt = runtime_with(&world, Arc::new(hooks));
    let run_id = admit(&rt, &world, script);
    let first = rt.resume(&run_id).expect("first activation");
    assert!(
        matches!(first, ActivationEnd::Suspended { .. }),
        "{first:?}"
    );
    let b = world.mock.handle_for(&run_id, 1).expect("b accepted");
    assert!(world.mock.complete(&b, done_value()));
    let second = rt.resume(&run_id).expect("second activation");
    assert_eq!(activations.load(Ordering::SeqCst), 2);
    assert!(
        matches!(second, ActivationEnd::Succeeded { .. }),
        "{second:?} {:#?}",
        rt.run(&run_id)
    );
    let run = rt.run(&run_id).expect("run");
    assert_eq!(result(&run), json!({"n": 20, "a": true, "b": true}));
    rt.quiesce();
}

fn replays_to(script: &str, expected: serde_json::Value) {
    let world = World::new("rejections");
    let rt = runtime(&world);
    let run_id = admit(&rt, &world, script);
    let first = rt.resume(&run_id).expect("first activation");
    assert!(
        matches!(first, ActivationEnd::Suspended { .. }),
        "{first:?}"
    );
    // The resumed activation replays every rejection from the journal.
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert_eq!(result(&run), expected);
    assert!(rt.activation_count(&run_id).expect("count") >= 2);
}

#[test]
fn caught_rejection_through_the_parent() {
    replays_to(
        r#"
        let code = null;
        try { await ops.call('mock', 'fail', { message: 'no' }); } catch (e) { code = e.data.code; }
        const after = await ops.call('mock', 'echo', { code });
        await llm({ prompt: 'p' });
        return after.code;
        "#,
        json!("denied"),
    );
}

#[test]
fn all_rejected_any_through_the_parent() {
    replays_to(
        r#"
        let messages = null;
        try {
            await Promise.any([
                ops.call('mock', 'fail', { message: 'first', delay_ms: 30 }),
                ops.call('mock', 'fail', { message: 'second' }),
            ]);
        } catch (e) {
            messages = e.errors.map((x) => x.data.message);
        }
        await llm({ prompt: 'p' });
        return messages;
        "#,
        json!(["first", "second"]),
    );
}

#[test]
fn early_all_rejection_through_the_parent() {
    replays_to(
        r#"
        let seen = null;
        try {
            await Promise.all([
                ops.call('mock', 'long', { slow: true }),
                ops.call('mock', 'fail', { message: 'early' }),
            ]);
        } catch (e) {
            seen = e.data.message;
        }
        await llm({ prompt: 'p' });
        return seen;
        "#,
        json!("early"),
    );
}

/// Several calls of one run are in flight at once. The four calls meet at a
/// rendezvous in the mock host: each waits until all four are inside the
/// host together. Dispatched one after another, each would wait alone until
/// the rendezvous timed out, and the peak seen together would be 1.
#[test]
fn calls_of_one_run_are_dispatched_concurrently() {
    let world = World::new("concurrent");
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        r#"
        const meet = { name: 'four', count: 4 };
        const xs = await Promise.all([1, 2, 3, 4].map((n) => ops.call('mock', 'echo', { n, rendezvous: meet })));
        return xs.map((x) => x.n);
        "#,
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(world.mock.peak_concurrency("four"), 4, "{run:#?}");
    assert_eq!(result(&run), json!([1, 2, 3, 4]));
}

/// A local effect committed behind an earlier unresolved call: the crash
/// lands after the `kv.set` committed its effect and outcome, while the
/// long call before it is still outstanding. Recovery must not apply the
/// effect again.
#[test]
fn local_effect_behind_unresolved_call_applies_once() {
    let world = World::new("local-behind");
    let script = r#"
        const l = ops.call('mock', 'long', {});
        await kv.set('k', 7);
        const v = await kv.get('k');
        const lv = await l;
        return { v, lv: lv.done };
    "#;
    let cut = Point::parse("LocalCommitted { position: 1 }#1").expect("point");
    let run = run_with_cuts(&world, script, &[cut], &config()).expect("runs");
    assert_eq!(run.fired, vec![true]);
    assert_eq!(run.summary.state, "succeeded", "{run:#?}");
    assert_eq!(run.summary.result, Some(json!({"v": 7, "lv": true})));
    // Written once: a second application would show as revision 2.
    assert_eq!(
        run.summary.kv.get("k").map(String::as_str),
        Some("7@1"),
        "{run:#?}"
    );
    assert!(run.summary.audit_complete(), "{run:#?}");
}

/// A synchronous call journaled without its value (its row committed, its
/// value not) is asked for again by the worker at the same position, and
/// the parent answers it then, committing the value before the reply.
#[test]
fn sync_call_journaled_without_value_is_answered_again() {
    let world = World::new("sync-reask");
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        "const t = Date.now(); await llm({ prompt: 'p' }); return { t: typeof t, later: Date.now() >= t };",
    );
    let first = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(first, ActivationEnd::Suspended { .. }),
        "{first:?}"
    );
    sql(
        &rt,
        &format!(
            "UPDATE journal SET settlement = NULL, value = NULL, payload_hash = NULL, \
             delivery_order = NULL, clock_ms = NULL WHERE run_id = '{run_id}' AND position = 0"
        ),
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert_eq!(result(&run), json!({"t": "number", "later": true}));
    let has_value = query_i64(
        &rt,
        &format!(
            "SELECT COUNT(*) FROM journal WHERE run_id = '{run_id}' AND position = 0 AND settlement IS NOT NULL"
        ),
    );
    assert_eq!(has_value, 1);
}

/// The parent keeps clock reads monotonic within a run, from the last
/// recorded value, even when the host's clock goes backwards across a
/// suspension.
#[test]
fn clock_stays_monotonic_across_activations() {
    let world = World::new("clock");
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        "const t1 = Date.now(); await llm({ prompt: 'p' }); const t2 = Date.now(); return t2 - t1;",
    );
    let first = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(first, ActivationEnd::Suspended { .. }),
        "{first:?}"
    );
    world.mock.set_clock(1_000.0);
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    let delta = result(&run).as_f64().expect("number");
    assert!(delta >= 0.0, "the clock went backwards by {delta}");
}

/// Two resume notifications racing for the same run start one activation.
#[test]
fn concurrent_resumes_start_one_activation() {
    let world = World::new("two-resumes");
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        "const x = await ops.call('mock', 'echo', { n: 1, gate: 'resume' }); return x.n;",
    );
    let barrier = std::sync::Barrier::new(2);
    let ends: OnceLock<Vec<ActivationEnd>> = OnceLock::new();
    let collected = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| {
                barrier.wait();
                let end = rt.resume(&run_id).expect("resume");
                if let Ok(mut c) = collected.lock() {
                    c.push(end);
                }
            });
        }
        // The activation that claims the run stays running until its call's
        // gate opens, so the other resume always meets a running run. The
        // gate opens once one resume has returned (with one claim, that is
        // the one refused), or after a bounded wait if both claimed.
        s.spawn(|| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while collected.lock().map(|c| c.is_empty()).unwrap_or(false)
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            world.mock.open_gate("resume");
        });
    });
    let _ = ends.set(collected.into_inner().unwrap_or_default());
    let ends = ends.get().expect("ends");
    let started = ends
        .iter()
        .filter(|e| !matches!(e, ActivationEnd::NotRunnable { .. }))
        .count();
    assert_eq!(started, 1, "{ends:?}");
    assert_eq!(rt.activation_count(&run_id).expect("count"), 1);
    assert_eq!(world.source.counts.activate.load(Ordering::SeqCst), 1);
    rt.quiesce();
}
