//! The journal cut harness over the representative run.
//!
//! Every boundary the uncut run passes (after each commit and after each
//! frame either way) is cut once: the store is cut, the worker killed, and a
//! new runtime recovers and finishes the run. The final state, result and
//! effect counts must equal the uncut run's. Recovery-generated suffixes are
//! cut too: for a set of first cuts, every boundary the recovery passes is
//! cut in turn (set `BASAL_CUT_EXHAUSTIVE=1` to do this for every first
//! cut).

use std::sync::Mutex;

use basal_core::Config;
use basal_testkit::harness::{
    CUT_PARALLEL, CutRun, Point, REPRESENTATIVE, Summary, World, is_repeat_prone, run_with_cuts,
};
use serde_json::json;

fn uncut() -> CutRun {
    let world = World::new("uncut");
    run_with_cuts(
        &world,
        REPRESENTATIVE,
        &[],
        &Config {
            install_gate: basal_core::InstallGate::Off,
            ..Config::default()
        },
    )
    .expect("uncut run")
}

fn check_uncut(summary: &Summary) {
    assert_eq!(summary.state, "succeeded", "{summary:#?}");
    assert_eq!(
        summary.result,
        Some(json!({
            "winner": "fast",
            "caught": "denied",
            "denied": "denied",
            "a": 1,
            "b": 2,
            "r": true,
            "answer": true,
            "seen": "fast",
            "monotonic": true,
        }))
    );
    // Each keyed mutation (the race loser, the send, the llm call) took
    // effect exactly once; the local write once (its first revision).
    assert!(
        summary.effects_per_call.values().all(|n| *n == 1),
        "{summary:#?}"
    );
    assert_eq!(summary.effects_per_call.len(), 3, "{summary:#?}");
    assert_eq!(
        summary.kv.get("seen").map(String::as_str),
        Some("\"fast\"@1"),
        "{summary:#?}"
    );
    assert_eq!(summary.open_obligations, 0);
    // One audit row per journaled call, the refused call's saying so.
    assert!(summary.audit_complete(), "{summary:#?}");
    assert_eq!(
        summary.audit.values().filter(|o| *o == "denied").count(),
        1,
        "{summary:#?}"
    );
    // The llm call's reservation was settled by its reported usage, and
    // every send under its send id carried the same bytes.
    assert_eq!(summary.tokens.get("open"), Some(&0), "{summary:#?}");
    assert!(
        summary.tokens.get("input").is_some_and(|n| *n > 0),
        "{summary:#?}"
    );
    assert_eq!(summary.tokens.get("unreported"), Some(&0), "{summary:#?}");
    assert_eq!(summary.broca_reuse, 0, "{summary:#?}");
}

/// Runs every cut list in parallel and returns (cuts, result) pairs.
fn run_all(cases: Vec<Vec<Point>>) -> Vec<(Vec<Point>, Result<CutRun, String>)> {
    let queue = Mutex::new(cases.into_iter().collect::<Vec<_>>());
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..CUT_PARALLEL {
            s.spawn(|| {
                loop {
                    let Some(cuts) = queue.lock().ok().and_then(|mut q| q.pop()) else {
                        return;
                    };
                    let world = World::new("cut");
                    let result = run_with_cuts(
                        &world,
                        REPRESENTATIVE,
                        &cuts,
                        &Config {
                            install_gate: basal_core::InstallGate::Off,
                            ..Config::default()
                        },
                    );
                    if let Ok(mut r) = results.lock() {
                        r.push((cuts, result));
                    }
                }
            });
        }
    });
    results.into_inner().unwrap_or_default()
}

/// Asserts every run ended equal to the uncut run, and returns the cut lists
/// whose cuts all landed.
fn assert_equivalent(
    expected: &Summary,
    results: &[(Vec<Point>, Result<CutRun, String>)],
) -> Vec<Vec<Point>> {
    let mut fired = Vec::new();
    let mut failures = Vec::new();
    for (cuts, result) in results {
        match result {
            Ok(run) => {
                if run.fired.iter().all(|f| *f) {
                    fired.push(cuts.clone());
                }
                // Equality covers the audit rows (one per call, the same
                // outcomes), kv revisions, token usage and Broca sends;
                // completeness is checked again so a cut run cannot pass by
                // matching an incomplete uncut run.
                if &run.summary != expected || !run.summary.audit_complete() {
                    failures.push(format!(
                        "cuts {:?} (fired {:?}): {:#?}",
                        cuts.iter().map(Point::render).collect::<Vec<_>>(),
                        run.fired,
                        run.summary
                    ));
                }
            }
            Err(e) => failures.push(format!(
                "cuts {:?}: {e}",
                cuts.iter().map(Point::render).collect::<Vec<_>>()
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cut runs differ from the uncut run {expected:#?}:\n{}",
        failures.len(),
        results.len(),
        failures.join("\n")
    );
    fired
}

#[test]
fn every_cut_recovers_to_the_uncut_state() {
    let base = uncut();
    check_uncut(&base.summary);
    let points = base.phases[0].clone();
    let results = run_all(points.iter().map(|p| vec![p.clone()]).collect());
    let fired = assert_equivalent(&base.summary, &results);
    // Every boundary must have been cut, except the few whose number of
    // occurrences depends on when outcomes arrive.
    let missed: Vec<String> = points
        .iter()
        .filter(|p| !is_repeat_prone(p) && !fired.contains(&vec![(*p).clone()]))
        .map(Point::render)
        .collect();
    assert!(missed.is_empty(), "cuts that did not land: {missed:?}");
    eprintln!("{} of {} first-level cuts fired", fired.len(), points.len());
}

#[test]
fn cuts_in_recovery_generated_suffixes_recover_too() {
    let base = uncut();
    check_uncut(&base.summary);
    let exhaustive = std::env::var_os("BASAL_CUT_EXHAUSTIVE").is_some();
    // First cuts whose recovery does distinctive work: re-sending a query
    // and a keyed mutation, releasing an outcome that arrived during the
    // gap, re-asking for a synchronous value, a refused call journaled
    // behind an unresolved one, a local effect behind an unresolved call,
    // and a cut inside the resumed activation.
    let chosen = [
        "DriverSynced { position: 0 }#1",
        "DriverReleased { position: 1 }#1",
        "DriverReleased { position: 2 }#1",
        "DriverReleased { position: 4 }#1",
        "DriverReleased { position: 5 }#1",
        "DriverSynced { position: 6 }#1",
        "DriverDispatched { position: 6 }#1",
        "DriverActivated { generation: 2 }#1",
    ];
    let firsts: Vec<Point> = base.phases[0]
        .iter()
        .filter(|p| exhaustive || chosen.contains(&p.render().as_str()))
        .cloned()
        .collect();
    assert!(exhaustive || firsts.len() == chosen.len(), "{firsts:?}");
    assert_eq!(
        firsts
            .iter()
            .filter(|point| point.boundary == "DriverActivated { generation: 2 }")
            .count(),
        1
    );
    // Learn each recovery's boundaries from a run with only the first cut.
    let learned = run_all(firsts.iter().map(|p| vec![p.clone()]).collect());
    let mut cases = Vec::new();
    for (cuts, result) in &learned {
        let run = result.as_ref().expect("first cut runs");
        // A repeated boundary can move between runs; in the exhaustive
        // sweep a first cut that missed has no recovery to cut.
        if exhaustive && !run.fired[0] {
            continue;
        }
        assert!(run.fired[0], "{cuts:?} did not fire");
        let recovery = run.phases.get(1).cloned().unwrap_or_default();
        for p in recovery {
            cases.push(vec![cuts[0].clone(), p]);
        }
    }
    let total = cases.len();
    let results = run_all(cases);
    let fired = assert_equivalent(&base.summary, &results);
    // What a recovery does depends on which of the calls in flight at the
    // first cut had their outcomes committed, so its later boundaries vary
    // between runs. The activation boundary never does: for every first cut,
    // the cut at recovery activation must have landed, which proves the
    // second cuts reach recovery at all.
    let mut missed = Vec::new();
    for (cuts, result) in &learned {
        let Ok(run) = result else { continue };
        if !run.fired[0] {
            continue;
        }
        let claim = run
            .phases
            .get(1)
            .and_then(|r| r.iter().find(|p| p.boundary.starts_with("DriverActivated")))
            .cloned();
        let landed = claim
            .as_ref()
            .is_some_and(|c| fired.contains(&vec![cuts[0].clone(), c.clone()]));
        if !landed {
            missed.push((cuts[0].render(), claim.map(|c| c.render())));
        }
    }
    assert!(
        missed.is_empty(),
        "recovery activations not cut: {missed:?}"
    );
    eprintln!("{} of {total} second-level cuts fired", fired.len());
}

#[test]
fn quiesce_waits_for_dispatch_threads_after_an_activation_cut() {
    use basal_core::{ActivationEnd, Boundary, Runtime, Step, Store};
    use basal_testkit::harness::{FnHooks, wait_until};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    let world = World::new("quiesce-dispatch-cut");
    let store = Arc::new(Store::open(world.store_path(), world.durability).unwrap());
    let weak = Arc::downgrade(&store);
    let (trace_tx, trace_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let entered_rx = Mutex::new(entered_rx);
    let release_rx = Mutex::new(release_rx);
    let hooks = Arc::new(FnHooks(
        move |_: &str, boundary: &Boundary| match boundary {
            Boundary::DriverDispatched { position: 0 } => {
                trace_tx
                    .send(format!(
                        "thread {:?}:\n{}",
                        std::thread::current().id(),
                        std::backtrace::Backtrace::force_capture()
                    ))
                    .unwrap();
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
                Step::Continue
            }
            Boundary::DriverSynced { position: 1 } => {
                entered_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                Step::Crash
            }
            _ => Step::Continue,
        },
    ));
    let rt = Runtime::new(
        store,
        Arc::new(world.mock.clone()),
        Arc::new(world.catalog.clone()),
        hooks,
        Some(world.source.clone()),
        Config {
            install_gate: basal_core::InstallGate::Off,
            auto_resume: false,
            selector: world.selector.clone(),
            ..Config::default()
        },
    )
    .unwrap();
    let spec = world.spec(&rt, "return await Promise.all([ops.call('mock','echo',{n:0}),ops.call('mock','echo',{n:1})]);").unwrap();
    let run = rt.admit(&spec).unwrap().run_id().unwrap().to_owned();
    assert_eq!(rt.resume(&run).unwrap(), ActivationEnd::Crashed);
    let trace = trace_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let quiescer = {
        let rt = rt.clone();
        std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            rt.quiesce();
            drop(rt);
            done_tx.send(()).unwrap();
        })
    };
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let escaped = done_rx.recv_timeout(Duration::from_millis(200)).is_ok();
    world.mock.detach();
    drop(rt);
    let holders = weak.strong_count();
    let refusal = Store::open(world.store_path(), world.durability)
        .err()
        .map(|e| e.to_string());
    release_tx.send(()).unwrap();
    if !escaped {
        done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    quiescer.join().unwrap();
    wait_until(
        Instant::now() + Duration::from_secs(10),
        "dispatch store references to drop",
        || weak.strong_count() == 0,
    )
    .unwrap();
    Store::open(world.store_path(), world.durability).unwrap();
    assert!(
        !escaped,
        "quiesce returned with {holders} store references; reopen refused: {refusal:?}; held dispatch {trace}"
    );
    // The held dispatch owns the lease until it drains, even after a sibling
    // crash has ended the activation. Quiesce must join it before returning.
    assert!(refusal.is_some());
}
