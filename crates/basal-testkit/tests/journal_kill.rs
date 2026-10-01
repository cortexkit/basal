//! Real `kill -9`: of the test parent process at every boundary of the
//! representative run, and separately of the worker. Every killed run must
//! recover to the uncut run's final state and effect counts.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use basal_core::Config;
use basal_testkit::harness::{Point, Probe, REPRESENTATIVE, World, drive, scratch, summarize};
use basal_testkit::worker_binary;
use serde_json::Value;

const PARENT: &str = env!("CARGO_BIN_EXE_basal-test-parent");
const PARALLEL: usize = 6;

/// Runs the test parent once. Returns (killed by SIGKILL, printed JSON).
fn parent(dir: &Path, kill_at: Option<&Point>) -> (bool, Option<Value>, String) {
    let worker = worker_binary();
    let mut cmd = Command::new(PARENT);
    cmd.arg("--dir").arg(dir).arg("--worker").arg(&worker);
    if let Some(p) = kill_at {
        cmd.arg("--kill-at").arg(p.render());
    }
    let out = cmd.output().expect("run the test parent");
    let killed = out.status.signal() == Some(libc::SIGKILL);
    let json = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .and_then(|l| serde_json::from_str(l).ok());
    (
        killed,
        json,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn killing_the_parent_at_every_boundary_recovers_to_the_uncut_state() {
    let dir = scratch("kill-uncut");
    let (killed, out, err) = parent(&dir, None);
    assert!(!killed);
    let out = out.unwrap_or_else(|| panic!("no output: {err}"));
    let expected = out["summary"].clone();
    assert_eq!(expected["state"], "succeeded", "{out}");
    let points: Vec<Point> = out["points"]
        .as_array()
        .expect("points")
        .iter()
        .filter_map(|p| p.as_str().and_then(Point::parse))
        .collect();
    assert!(points.len() > 40, "{points:?}");
    let _ = std::fs::remove_dir_all(&dir);

    let queue = Mutex::new(points.clone());
    let failures = Mutex::new(Vec::new());
    let fired = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..PARALLEL {
            s.spawn(|| {
                while let Some(point) = queue.lock().ok().and_then(|mut q| q.pop()) {
                    let dir = scratch("kill");
                    let (killed, out, err) = parent(&dir, Some(&point));
                    if killed {
                        fired.fetch_add(1, Ordering::SeqCst);
                    }
                    // The restarted parent recovers and finishes the run.
                    let (_, after, err2) = parent(&dir, None);
                    let summary = after.as_ref().map(|a| a["summary"].clone());
                    if summary.as_ref() != Some(&expected) {
                        if let Ok(mut f) = failures.lock() {
                            f.push(format!(
                                "{} (killed {killed}): {summary:?}\nfirst: {out:?} {err}\nsecond: {err2}",
                                point.render()
                            ));
                        }
                    }
                    let _ = std::fs::remove_dir_all(&dir);
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap_or_default();
    assert!(
        failures.is_empty(),
        "{} of {} kills differ from {expected}:\n{}",
        failures.len(),
        points.len(),
        failures.join("\n")
    );
    let fired = fired.load(Ordering::SeqCst);
    assert!(
        fired * 10 >= points.len() * 9,
        "only {fired} of {} kills landed",
        points.len()
    );
    eprintln!("{fired} of {} parent kills landed", points.len());
}

#[test]
fn killing_the_worker_at_every_boundary_recovers_to_the_uncut_state() {
    let base_world = World::new("worker-kill-uncut");
    let base =
        basal_testkit::harness::run_with_cuts(&base_world, REPRESENTATIVE, &[], &Config::default())
            .expect("uncut");
    let expected = base.summary.clone();
    let points = base.phases[0].clone();
    let queue = Mutex::new(points.clone());
    let failures = Mutex::new(Vec::new());
    let fired = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..PARALLEL {
            s.spawn(|| {
                while let Some(point) = queue.lock().ok().and_then(|mut q| q.pop()) {
                    let world = World::new("worker-kill");
                    let source = world.source.clone();
                    let probe = Arc::new(Probe::act_at(point.clone(), move |_, _| {
                        let pid = source.counts.live_pid.load(Ordering::SeqCst);
                        if pid != 0 {
                            // SAFETY: kill(2) has no memory-safety
                            // preconditions; the pid is a worker this test
                            // spawned and has not reaped.
                            unsafe {
                                libc::kill(pid as i32, libc::SIGKILL);
                            }
                        }
                    }));
                    let rt = world
                        .runtime(probe.clone(), Config::default())
                        .expect("runtime");
                    let run_id = rt
                        .admit(&world.spec(REPRESENTATIVE))
                        .expect("admit")
                        .run_id()
                        .unwrap_or_default()
                        .to_owned();
                    let outcome = drive(
                        &rt,
                        &world.mock,
                        &run_id,
                        std::time::Duration::from_secs(120),
                    );
                    rt.quiesce();
                    if probe.fired() {
                        fired.fetch_add(1, Ordering::SeqCst);
                    }
                    let summary = outcome.and_then(|_| summarize(&rt, &world.mock, &run_id));
                    if summary.as_ref().ok() != Some(&expected) {
                        if let Ok(mut f) = failures.lock() {
                            f.push(format!("{}: {summary:#?}", point.render()));
                        }
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap_or_default();
    assert!(
        failures.is_empty(),
        "{} of {} worker kills differ:\n{}",
        failures.len(),
        points.len(),
        failures.join("\n")
    );
    let fired = fired.load(Ordering::SeqCst);
    assert!(
        fired * 10 >= points.len() * 9,
        "only {fired} of {} kills landed",
        points.len()
    );
    eprintln!("{fired} of {} worker kills landed", points.len());
}
