use super::*;
use std::sync::Arc;

use basal_host::{CallClass, CallRequest, CompletionSink, Dispatched, Host, TransportError};
use basal_proto::CallKind;

use crate::codemode::store::tests::{DIGEST, completed, new_run};
use crate::codemode::store::{
    CallStart, Lookup, NewRun, Outcome, commit_terminal, insert_call, insert_run, lookup,
};
use crate::{Clock, Config, InstallGate, NoHooks, Runtime};

/// A host that fails the test if anything is ever sent: neither codemode
/// retention nor any flow path may dispatch a codemode call.
struct NoDispatch;

impl Host for NoDispatch {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> std::result::Result<Dispatched, TransportError> {
        panic!("no codemode row may reach a flow dispatch")
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.5
    }
}

struct Fixture {
    rt: Runtime,
    clock: Clock,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn fixture(now: i64) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "basal-codemode-retention-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let store = Store::open(dir.join("core.db"), crate::Durability { fullfsync: false }).unwrap();
    let clock = Clock::manual(now);
    let rt = Runtime::new(
        Arc::new(store),
        Arc::new(NoDispatch),
        Arc::new(basal_host::catalog::MockCatalog::new()),
        Arc::new(NoHooks),
        None,
        Config {
            clock: clock.clone(),
            install_gate: InstallGate::Off,
            // Flow retention runs once a week here, so the engine's next
            // wake shows the codemode horizon rather than the flow
            // retention interval.
            retention: crate::retention::RetentionConfig {
                interval: std::time::Duration::from_secs(7 * 24 * 3600),
                ..Default::default()
            },
            ..Config::default()
        },
    );
    Fixture { rt, clock, dir }
}

const ENDED_AT: i64 = 5_000;

/// 24 hours, written out rather than read from `RETENTION_MS`, so changing
/// the retention period fails these tests instead of moving them along.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A run admitted at 100 with one call the parent refused before sending,
/// ended at `ENDED_AT`.
fn ended_run(f: &Fixture, run_id: &str) {
    f.rt.store()
        .write(|tx| {
            assert!(insert_run(tx, &new_run(run_id), None)?);
            let refused = CallStart::Settled {
                outcome: Outcome::Refused,
                code: Some("unknown_tool"),
            };
            insert_call(tx, run_id, 0, "find", 2, refused)?;
            commit_terminal(tx, run_id, &completed(), ENDED_AT)
        })
        .unwrap();
}

fn lookup_in(f: &Fixture, run_id: &str) -> Lookup {
    f.rt.store().read(|c| lookup(c, run_id)).unwrap()
}

fn count(f: &Fixture, sql: &str) -> i64 {
    f.rt.store()
        .read(|c| Ok(c.query_row(sql, [], |r| r.get(0))?))
        .unwrap()
}

#[test]
fn a_run_is_pruned_24_hours_and_1_ms_after_it_ends() {
    let f = fixture(ENDED_AT);
    ended_run(&f, "r1");
    f.rt.enforce_deadlines().unwrap();
    assert_eq!(
        f.rt.next_wake_at(None).unwrap(),
        Some(ENDED_AT + DAY_MS + 1)
    );
    assert_eq!(
        f.rt.store().read(next_sweep_at).unwrap(),
        Some(ENDED_AT + DAY_MS + 1)
    );

    // Exactly 24 hours after the end the run is still readable.
    f.clock.set(ENDED_AT + DAY_MS);
    f.rt.enforce_deadlines().unwrap();
    assert!(matches!(lookup_in(&f, "r1"), Lookup::Found(_)));

    // One millisecond later the deadline pass prunes it.
    f.clock.advance(1);
    f.rt.enforce_deadlines().unwrap();
    assert_eq!(lookup_in(&f, "r1"), Lookup::Pruned);
    assert_eq!(count(&f, "SELECT COUNT(*) FROM codemode_runs"), 0);
    assert_eq!(count(&f, "SELECT COUNT(*) FROM codemode_calls"), 0);
    assert_eq!(
        count(
            &f,
            "SELECT pruned_at FROM codemode_tombstones WHERE run_id = 'r1'"
        ),
        ENDED_AT + DAY_MS + 1
    );
    assert_eq!(f.rt.store().read(next_sweep_at).unwrap(), None);

    // The id is never admitted again, whatever the request carries.
    let mut again = new_run("r1");
    again.program = "return 2;";
    assert!(
        !f.rt
            .store()
            .write(|tx| insert_run(tx, &again, None))
            .unwrap()
    );
    assert_eq!(lookup_in(&f, "r1"), Lookup::Pruned);
}

#[test]
fn a_running_run_is_never_pruned() {
    let f = fixture(100);
    f.rt.store()
        .write(|tx| {
            insert_run(tx, &new_run("live"), None)?;
            insert_call(tx, "live", 0, "find", 2, CallStart::Intent { at: 100 })
        })
        .unwrap();
    f.clock.set(100 + 10 * DAY_MS);
    assert_eq!(
        sweep(f.rt.store(), f.clock.now_ms()).unwrap(),
        Vec::<String>::new()
    );
    f.rt.enforce_deadlines().unwrap();
    assert!(matches!(lookup_in(&f, "live"), Lookup::Found(_)));
    assert_eq!(f.rt.store().read(next_sweep_at).unwrap(), None);
}

#[test]
fn the_sweep_prunes_oldest_first_in_bounded_batches() {
    let f = fixture(0);
    let ids: Vec<String> = (0..(BATCH + 3)).map(|i| format!("run-{i:04}")).collect();
    f.rt.store()
        .write(|tx| {
            for (i, id) in ids.iter().enumerate() {
                let run = NewRun {
                    admitted_at: 0,
                    ..new_run(id)
                };
                insert_run(tx, &run, None)?;
                // Later ids end earlier, so age order is not id order.
                commit_terminal(tx, id, &completed(), (ids.len() - i) as i64)?;
            }
            Ok(())
        })
        .unwrap();
    let now = DAY_MS + 1_000;
    let first = sweep(f.rt.store(), now).unwrap();
    assert_eq!(first.len(), BATCH);
    assert_eq!(first[0], ids[ids.len() - 1]);
    // The rest is due at once, so the engine's next pass continues.
    assert!(f.rt.store().read(next_sweep_at).unwrap().unwrap() <= now);
    assert_eq!(sweep(f.rt.store(), now).unwrap().len(), 3);
    assert_eq!(
        count(&f, "SELECT COUNT(*) FROM codemode_tombstones"),
        ids.len() as i64
    );
}

#[test]
fn the_schema_refuses_to_reuse_a_pruned_run_id() {
    let f = fixture(ENDED_AT);
    ended_run(&f, "r1");
    f.clock.set(ENDED_AT + DAY_MS + 1);
    assert_eq!(sweep(f.rt.store(), f.clock.now_ms()).unwrap(), ["r1"]);
    let err =
        f.rt.store()
            .write(|tx| {
                Ok(tx.execute(
                "INSERT INTO codemode_runs (run_id, agent_id, program, catalog, catalog_digest, \
                 limits, scope, deadline_ms, status, admitted_at) \
                 VALUES ('r1', 'agent', 'return 1;', '[]', ?1, '{}', '{}', 0, 'running', 0)",
                [DIGEST],
            )?)
            })
            .unwrap_err();
    assert!(err.to_string().contains("never used again"), "{err}");
}

#[test]
fn the_schema_deletes_a_run_only_behind_its_tombstone() {
    let f = fixture(ENDED_AT);
    ended_run(&f, "r1");
    f.rt.store()
        .write(|tx| insert_run(tx, &new_run("live"), None))
        .unwrap();
    for (run_id, tombstone) in [("r1", false), ("live", true)] {
        let err =
            f.rt.store()
                .write(|tx| {
                    if tombstone {
                        tx.execute(
                            "INSERT INTO codemode_tombstones (run_id, pruned_at) VALUES (?1, 0)",
                            [run_id],
                        )?;
                    }
                    tx.execute("DELETE FROM codemode_calls WHERE run_id = ?1", [run_id])?;
                    Ok(tx.execute("DELETE FROM codemode_runs WHERE run_id = ?1", [run_id])?)
                })
                .unwrap_err();
        assert!(err.to_string().contains("tombstoned"), "{run_id}: {err}");
    }
}

#[test]
fn tombstones_are_permanent() {
    let f = fixture(ENDED_AT);
    ended_run(&f, "r1");
    f.clock.set(ENDED_AT + DAY_MS + 1);
    sweep(f.rt.store(), f.clock.now_ms()).unwrap();
    for sql in [
        "DELETE FROM codemode_tombstones",
        "UPDATE codemode_tombstones SET run_id = 'other'",
    ] {
        let err =
            f.rt.store()
                .write(|tx| Ok(tx.execute(sql, [])?))
                .unwrap_err();
        assert!(err.to_string().contains("permanent"), "{sql}: {err}");
    }
}

#[test]
fn flow_paths_never_see_codemode_rows() {
    let f = fixture(100);
    f.rt.store()
        .write(|tx| {
            insert_run(tx, &new_run("cm"), None)?;
            // An unresolved sent call of a query-classified tool: a flow
            // journal row like it would be resent by call recovery.
            insert_call(tx, "cm", 0, "find", 2, CallStart::Intent { at: 100 })?;
            insert_call(tx, "cm", 1, "find", 2, CallStart::Queued)
        })
        .unwrap();
    let before =
        f.rt.store()
            .read(|c| crate::codemode::store::calls(c, "cm"))
            .unwrap();

    // Restart recovery, the deadline and retention pass, flow pruning and the
    // flow run lookups all leave codemode rows alone; the host panics if
    // anything is dispatched.
    assert!(f.rt.recover().unwrap().is_empty());
    f.clock.set(100 + 10 * DAY_MS);
    assert!(f.rt.enforce_deadlines().unwrap().is_empty());
    f.rt.prune(f.clock.now_ms(), 0).unwrap();
    assert!(f.rt.startable().unwrap().is_empty());
    assert!(f.rt.run("cm").is_err());
    assert!(f.rt.calls("cm").unwrap_or_default().is_empty());
    f.rt.quiesce();

    assert_eq!(
        f.rt.store()
            .read(|c| crate::codemode::store::calls(c, "cm"))
            .unwrap(),
        before
    );
    assert!(matches!(lookup_in(&f, "cm"), Lookup::Found(_)));
    assert_eq!(count(&f, "SELECT COUNT(*) FROM runs"), 0);
    assert_eq!(count(&f, "SELECT COUNT(*) FROM journal"), 0);
}
