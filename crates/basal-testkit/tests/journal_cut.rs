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
use basal_testkit::harness::{CutRun, Point, REPRESENTATIVE, Summary, World, run_with_cuts};
use serde_json::json;

const PARALLEL: usize = 6;

fn uncut() -> CutRun {
    let world = World::new("uncut");
    run_with_cuts(&world, REPRESENTATIVE, &[], &Config::default()).expect("uncut run")
}

fn check_uncut(summary: &Summary) {
    assert_eq!(summary.state, "succeeded", "{summary:#?}");
    assert_eq!(
        summary.result,
        Some(json!({
            "winner": "fast",
            "caught": "denied",
            "a": 1,
            "b": 2,
            "r": true,
            "answer": true,
            "seen": "fast",
            "monotonic": true,
        }))
    );
    // Each keyed mutation (the race loser, the send, the llm call) took
    // effect exactly once; the local write once.
    assert!(
        summary.effects_per_call.values().all(|n| *n == 1),
        "{summary:#?}"
    );
    assert_eq!(summary.effects_per_call.len(), 3, "{summary:#?}");
    assert_eq!(
        summary.local_effects.values().sum::<usize>(),
        1,
        "{summary:#?}"
    );
    assert_eq!(summary.open_obligations, 0);
}

/// Runs every cut list in parallel and returns (cuts, result) pairs.
fn run_all(cases: Vec<Vec<Point>>) -> Vec<(Vec<Point>, Result<CutRun, String>)> {
    let queue = Mutex::new(cases.into_iter().collect::<Vec<_>>());
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..PARALLEL {
            s.spawn(|| {
                loop {
                    let Some(cuts) = queue.lock().ok().and_then(|mut q| q.pop()) else {
                        return;
                    };
                    let world = World::new("cut");
                    let result = run_with_cuts(&world, REPRESENTATIVE, &cuts, &Config::default());
                    if let Ok(mut r) = results.lock() {
                        r.push((cuts, result));
                    }
                }
            });
        }
    });
    results.into_inner().unwrap_or_default()
}

fn assert_equivalent(
    expected: &Summary,
    results: &[(Vec<Point>, Result<CutRun, String>)],
) -> usize {
    let mut fired = 0;
    let mut failures = Vec::new();
    for (cuts, result) in results {
        match result {
            Ok(run) => {
                if run.fired.iter().all(|f| *f) {
                    fired += 1;
                }
                if &run.summary != expected {
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
fn representative_run_completes_uncut() {
    let run = uncut();
    check_uncut(&run.summary);
}

#[test]
fn every_cut_recovers_to_the_uncut_state() {
    let base = uncut();
    check_uncut(&base.summary);
    let points = base.phases[0].clone();
    let results = run_all(points.iter().map(|p| vec![p.clone()]).collect());
    let fired = assert_equivalent(&base.summary, &results);
    // Concurrency moves a few repeated boundaries (how many times the worker
    // reports itself blocked) between runs, so a cut can miss; nearly all
    // must land for the harness to mean anything.
    assert!(
        fired * 10 >= points.len() * 9,
        "only {fired} of {} cuts fired",
        points.len()
    );
    eprintln!("{fired} of {} first-level cuts fired", points.len());
}

#[test]
fn cuts_in_recovery_generated_suffixes_recover_too() {
    let base = uncut();
    check_uncut(&base.summary);
    let exhaustive = std::env::var_os("BASAL_CUT_EXHAUSTIVE").is_some();
    // First cuts whose recovery does distinctive work: re-sending a query
    // and a keyed mutation, releasing an outcome that arrived during the
    // gap, re-asking for a synchronous value, a local effect behind an
    // unresolved call, and a cut inside the resumed activation.
    let chosen = [
        "CallCommitted { position: 0 }#1",
        "OrderCommitted { position: 1, order: 0 }#1",
        "SyncCommitted { position: 2 }#1",
        "LocalCommitted { position: 4 }#1",
        "CallCommitted { position: 5 }#1",
        "HostAnswered { position: 5 }#1",
        "Claimed { generation: 2 }#1",
    ];
    let firsts: Vec<Point> = base.phases[0]
        .iter()
        .filter(|p| exhaustive || chosen.contains(&p.render().as_str()))
        .cloned()
        .collect();
    assert!(exhaustive || firsts.len() == chosen.len(), "{firsts:?}");
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
    assert!(
        fired * 10 >= total * 8,
        "only {fired} of {total} second cuts fired"
    );
    eprintln!("{fired} of {total} second-level cuts fired");
}
