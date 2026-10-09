use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::store::{Durability, Store};

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct TempStore {
    pub(crate) store: Store,
    dir: PathBuf,
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn temp_store() -> TempStore {
    let dir = std::env::temp_dir().join(format!(
        "basal-codemode-store-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let store = Store::open(dir.join("core.db"), Durability { fullfsync: false }).unwrap();
    TempStore { store, dir }
}

pub(crate) const DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

pub(crate) fn new_run(run_id: &str) -> NewRun<'_> {
    NewRun {
        run_id,
        agent_id: "agent",
        program: "return 1;",
        catalog: "[]",
        catalog_digest: DIGEST,
        description: Some("count the notes"),
        limits: "{}",
        scope: r#"{"owner":"reserved:core","ref":"r","epoch":1}"#,
        deadline_ms: 1_000_000,
        admitted_at: 100,
    }
}

pub(crate) fn admit(store: &Store, run: &NewRun) {
    assert!(store.write(|tx| insert_run(tx, run, None)).unwrap());
}

fn found(store: &Store, run_id: &str) -> RunRecord {
    match store.read(|c| lookup(c, run_id)).unwrap() {
        Lookup::Found(run) => *run,
        other => panic!("expected a stored run, got {other:?}"),
    }
}

fn call(store: &Store, run_id: &str, position: u64) -> CallRecord {
    store
        .read(|c| calls(c, run_id))
        .unwrap()
        .into_iter()
        .find(|c| c.position == position)
        .expect("a stored call")
}

pub(crate) fn completed() -> Terminal {
    Terminal {
        status: Status::Completed,
        value: Some("42".into()),
        error: None,
        output: "a 1\n".into(),
        warnings: "[]".into(),
    }
}

fn cancelled() -> Terminal {
    Terminal {
        status: Status::Cancelled,
        value: None,
        error: None,
        output: String::new(),
        warnings: "[]".into(),
    }
}

#[test]
fn the_migration_adds_codemode_tables_apart_from_the_flow_tables() {
    let t = temp_store();
    let columns = |table: &str| -> Vec<String> {
        t.store
            .read(|c| {
                let mut stmt =
                    c.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
                Ok(stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?)
            })
            .unwrap()
    };
    let runs = columns("codemode_runs");
    for column in [
        "run_id",
        "program",
        "catalog",
        "catalog_digest",
        "description",
        "output",
        "status",
        "admitted_at",
        "ended_at",
    ] {
        assert!(runs.iter().any(|c| c == column), "codemode_runs.{column}");
    }
    let calls = columns("codemode_calls");
    for column in [
        "run_id",
        "position",
        "tool",
        "idempotency_key",
        "outcome",
        "code",
    ] {
        assert!(calls.iter().any(|c| c == column), "codemode_calls.{column}");
    }
    assert_eq!(columns("codemode_tombstones"), ["run_id", "pruned_at"]);
    // Nothing codemode lives in, or points at, a flow table.
    let flow_references: i64 = t
        .store
        .read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM sqlite_master m, pragma_foreign_key_list(m.name) f \
                 WHERE m.type = 'table' AND m.name LIKE 'codemode_%' AND f.\"table\" NOT LIKE 'codemode_%'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(flow_references, 0);
}

#[test]
fn an_admitted_run_reads_back_as_admitted() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    let run = found(&t.store, "r1");
    assert_eq!(run.status, Status::Running);
    assert_eq!(run.program, "return 1;");
    assert_eq!(run.catalog_digest, DIGEST);
    assert_eq!(run.description.as_deref(), Some("count the notes"));
    assert_eq!(run.output, "");
    assert_eq!(run.warnings, "[]");
    assert_eq!(
        (run.admitted_at, run.ended_at, run.duration_ms),
        (100, None, None)
    );
    assert_eq!(
        t.store.read(|c| lookup(c, "never")).unwrap(),
        Lookup::Unknown
    );
}

#[test]
fn a_known_run_id_keeps_its_first_admission() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    let mut again = new_run("r1");
    again.program = "return 2;";
    again.description = None;
    assert!(!t.store.write(|tx| insert_run(tx, &again, None)).unwrap());
    let run = found(&t.store, "r1");
    assert_eq!(run.program, "return 1;");
    assert_eq!(run.description.as_deref(), Some("count the notes"));
}

#[test]
fn a_run_admitted_at_its_deadline_is_stored_already_ended() {
    let t = temp_store();
    let ended = Terminal {
        status: Status::BudgetExhausted(Budget::Wall),
        value: None,
        error: Some(RunError {
            code: "budget_exhausted:wall".into(),
            message: "the run was admitted at its deadline".into(),
        }),
        output: String::new(),
        warnings: "[]".into(),
    };
    assert!(
        t.store
            .write(|tx| insert_run(tx, &new_run("late"), Some(&ended)))
            .unwrap()
    );
    let run = found(&t.store, "late");
    assert_eq!(run.status, Status::BudgetExhausted(Budget::Wall));
    assert_eq!(run.error, ended.error);
    assert_eq!((run.ended_at, run.duration_ms), (Some(100), Some(0)));
    assert!(
        !t.store
            .write(|tx| insert_call(tx, "late", 0, "find", 2, CallStart::Intent { at: 100 }))
            .unwrap()
    );
}

#[test]
fn call_keys_are_the_codemode_idempotency_keys() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    for position in [0, 7] {
        assert!(
            t.store
                .write(|tx| insert_call(tx, "r1", position, "find", 2, CallStart::Queued))
                .unwrap()
        );
        let key = idempotency_key("codemode", "r1", position);
        assert_eq!(call_key("r1", position), key);
        assert_eq!(call(&t.store, "r1", position).idempotency_key, key);
    }
}

#[test]
fn a_recorded_outcome_never_changes() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    t.store
        .write(|tx| insert_call(tx, "r1", 0, "find", 2, CallStart::Intent { at: 200 }))
        .unwrap();
    assert!(
        t.store
            .write(|tx| record_outcome(tx, "r1", 0, Outcome::Ok, None, 205))
            .unwrap()
    );
    assert!(
        !t.store
            .write(|tx| record_outcome(tx, "r1", 0, Outcome::Error, Some("late"), 300))
            .unwrap()
    );
    let c = call(&t.store, "r1", 0);
    assert_eq!(
        (c.outcome, c.code, c.duration_ms),
        (Outcome::Ok, None, Some(5))
    );
}

#[test]
fn a_refusal_before_send_has_no_intent_and_no_duration() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    let start = CallStart::Settled {
        outcome: Outcome::Refused,
        code: Some("unknown_tool"),
    };
    assert!(
        t.store
            .write(|tx| insert_call(tx, "r1", 0, "nope", 2, start))
            .unwrap()
    );
    let c = call(&t.store, "r1", 0);
    assert_eq!(c.outcome, Outcome::Refused);
    assert_eq!(c.code.as_deref(), Some("unknown_tool"));
    assert_eq!((c.intent_at, c.duration_ms), (None, None));
}

#[test]
fn a_queued_call_is_sent_only_while_it_is_still_queued() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    t.store
        .write(|tx| insert_call(tx, "r1", 0, "find", 2, CallStart::Queued))
        .unwrap();
    assert!(t.store.write(|tx| begin_queued(tx, "r1", 0, 300)).unwrap());
    assert!(!t.store.write(|tx| begin_queued(tx, "r1", 0, 400)).unwrap());
    assert_eq!(call(&t.store, "r1", 0).intent_at, Some(300));
}

#[test]
fn a_terminal_write_never_overwrites_another() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    assert!(
        t.store
            .write(|tx| commit_terminal(tx, "r1", &completed(), 150))
            .unwrap()
    );
    assert!(
        !t.store
            .write(|tx| commit_terminal(tx, "r1", &cancelled(), 160))
            .unwrap()
    );
    let run = found(&t.store, "r1");
    assert_eq!(run.status, Status::Completed);
    assert_eq!(run.value.as_deref(), Some("42"));
    assert_eq!(run.output, "a 1\n");
    assert_eq!((run.ended_at, run.duration_ms), (Some(150), Some(50)));
}

#[test]
fn termination_cancels_queued_calls_and_marks_sent_calls_unknown() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    t.store
        .write(|tx| {
            // A sent call refused for scope loss, recorded before termination.
            insert_call(tx, "r1", 0, "find", 2, CallStart::Intent { at: 110 })?;
            record_outcome(tx, "r1", 0, Outcome::Refused, Some("scope_ended"), 112)?;
            // A sent call still awaiting its answer, and a queued one.
            insert_call(tx, "r1", 1, "find", 2, CallStart::Intent { at: 120 })?;
            insert_call(tx, "r1", 2, "find", 2, CallStart::Queued)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        t.store
            .write(|tx| settle_pending_calls(tx, "r1", 130))
            .unwrap(),
        (1, 1)
    );
    let kept = call(&t.store, "r1", 0);
    assert_eq!(
        (kept.outcome, kept.code.as_deref(), kept.duration_ms),
        (Outcome::Refused, Some("scope_ended"), Some(2))
    );
    let sent = call(&t.store, "r1", 1);
    assert_eq!(
        (sent.outcome, sent.code.as_deref(), sent.duration_ms),
        (Outcome::OutcomeUnknown, Some("no_outcome"), Some(10))
    );
    let queued = call(&t.store, "r1", 2);
    assert_eq!(
        (queued.outcome, queued.code, queued.duration_ms),
        (Outcome::Cancelled, None, None)
    );
    // An answer released after termination began is not recorded.
    assert!(
        !t.store
            .write(|tx| record_outcome(tx, "r1", 1, Outcome::Ok, None, 140))
            .unwrap()
    );
    assert_eq!(call(&t.store, "r1", 1).outcome, Outcome::OutcomeUnknown);
}

#[test]
fn committing_a_terminal_status_settles_pending_calls_in_the_same_transaction() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    t.store
        .write(|tx| {
            insert_call(tx, "r1", 0, "find", 2, CallStart::Intent { at: 120 })?;
            insert_call(tx, "r1", 1, "find", 2, CallStart::Queued)?;
            Ok(())
        })
        .unwrap();
    assert!(
        t.store
            .write(|tx| commit_terminal(tx, "r1", &cancelled(), 125))
            .unwrap()
    );
    let outcomes: Vec<_> = t
        .store
        .read(|c| calls(c, "r1"))
        .unwrap()
        .into_iter()
        .map(|c| (c.outcome, c.code))
        .collect();
    assert_eq!(
        outcomes,
        [
            (Outcome::OutcomeUnknown, Some("no_outcome".to_owned())),
            (Outcome::Cancelled, None)
        ]
    );
    assert!(
        !t.store
            .write(|tx| insert_call(tx, "r1", 2, "find", 2, CallStart::Queued))
            .unwrap()
    );
}

#[test]
fn running_runs_are_counted_per_agent_and_in_total() {
    let t = temp_store();
    admit(&t.store, &new_run("a1"));
    admit(&t.store, &new_run("a2"));
    let mut other = new_run("b1");
    other.agent_id = "other";
    admit(&t.store, &other);
    t.store
        .write(|tx| commit_terminal(tx, "a2", &completed(), 150))
        .unwrap();
    assert_eq!(
        t.store.read(|c| running_counts(c, "agent")).unwrap(),
        (1, 2)
    );
    assert_eq!(
        t.store.read(|c| running_counts(c, "other")).unwrap(),
        (1, 2)
    );
    assert_eq!(t.store.read(|c| running_counts(c, "none")).unwrap(), (0, 2));
    assert_eq!(t.store.read(running_runs).unwrap(), ["a1", "b1"]);
}

// The schema's own rules, written in SQL so a writer that bypasses this
// module still cannot break them.

fn raw(store: &Store, sql: &str) -> Result<usize> {
    store.write(|tx| Ok(tx.execute(sql, [])?))
}

#[test]
fn the_schema_refuses_to_change_a_recorded_outcome() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    t.store
        .write(|tx| {
            insert_call(tx, "r1", 0, "find", 2, CallStart::Intent { at: 110 })?;
            record_outcome(tx, "r1", 0, Outcome::Ok, None, 111)
        })
        .unwrap();
    let err = raw(
        &t.store,
        "UPDATE codemode_calls SET outcome = 'pending', duration_ms = NULL",
    )
    .unwrap_err();
    assert!(err.to_string().contains("never changes"), "{err}");
}

#[test]
fn the_schema_refuses_to_change_a_terminal_run() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    t.store
        .write(|tx| commit_terminal(tx, "r1", &completed(), 150))
        .unwrap();
    let err = raw(&t.store, "UPDATE codemode_runs SET output = 'changed'").unwrap_err();
    assert!(err.to_string().contains("never changes"), "{err}");
}

#[test]
fn the_schema_refuses_to_end_a_run_with_a_pending_call() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    t.store
        .write(|tx| insert_call(tx, "r1", 0, "find", 2, CallStart::Queued))
        .unwrap();
    let err = raw(
        &t.store,
        "UPDATE codemode_runs SET status = 'cancelled', ended_at = 150, duration_ms = 50",
    )
    .unwrap_err();
    assert!(err.to_string().contains("pending"), "{err}");
}

#[test]
fn the_schema_refuses_a_call_for_a_run_that_is_not_running() {
    let t = temp_store();
    admit(&t.store, &new_run("r1"));
    t.store
        .write(|tx| commit_terminal(tx, "r1", &completed(), 150))
        .unwrap();
    let err = raw(
        &t.store,
        "INSERT INTO codemode_calls (run_id, position, tool, idempotency_key, input_bytes, outcome) \
         VALUES ('r1', 0, 'find', 'k', 2, 'pending')",
    )
    .unwrap_err();
    assert!(err.to_string().contains("only while"), "{err}");
}

#[test]
fn the_schema_bounds_the_description() {
    let t = temp_store();
    let mut run = new_run("long");
    let exact = "d".repeat(1024);
    run.description = Some(&exact);
    assert!(t.store.write(|tx| insert_run(tx, &run, None)).unwrap());
    assert_eq!(
        found(&t.store, "long").description.as_deref(),
        Some(exact.as_str())
    );
    let over = "\u{e9}".repeat(513);
    for bad in [over.as_str(), "a\nb", "a\rb", "a\u{2028}b", "a\u{2029}b"] {
        let mut run = new_run("bad");
        run.description = Some(bad);
        assert!(
            t.store.write(|tx| insert_run(tx, &run, None)).is_err(),
            "{bad:?}"
        );
    }
}
