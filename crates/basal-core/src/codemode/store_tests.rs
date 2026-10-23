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
    let store = Store::open(dir.join("core.db"), Durability::throwaway()).unwrap();
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

#[test]
fn withdrawn_requires_a_sent_entered_held_call() {
    let t = temp_store();
    admit(&t.store, &new_run("withdrawal"));
    // A provider can hold a call without a person (reason `running`), and
    // withdrawing that call is as valid as withdrawing a person-held one.
    for (position, intent, entered, held, person, allowed) in [
        (0, None, None, 1, 1, false),
        (1, Some(100), None, 1, 1, false),
        (2, Some(100), Some(100), 0, 0, false),
        (3, Some(100), Some(100), 1, 1, true),
        (4, Some(100), Some(100), 1, 0, true),
    ] {
        t.store.write(|tx| {
            tx.execute("INSERT INTO codemode_calls(run_id,position,tool,idempotency_key,input_bytes,intent_at,entered_at,held,waited_on_person,outcome) VALUES('withdrawal',?1,'browser_use',?2,0,?3,?4,?5,?6,'pending')", params![position, format!("withdraw-{position}"),intent,entered,held,person])?;
            let update = record_outcome(tx,"withdrawal",position,Outcome::Withdrawn,Some("withdrawn"),200);
            if allowed { assert!(update?); } else { assert!(update.is_err(), "withdrawn requires dispatch and a hold at position {position}"); }
            Ok(())
        }).unwrap();
    }
    t.store.write(|tx| {
        assert!(
            tx.execute("INSERT INTO codemode_calls(run_id,position,tool,idempotency_key,input_bytes,intent_at,entered_at,held,waited_on_person,outcome) VALUES('withdrawal',5,'browser_use','withdraw-5',0,100,100,0,1,'pending')", []).is_err(),
            "a person wait is always a hold"
        );
        Ok(())
    }).unwrap();
    let withdrawn = call(&t.store, "withdrawal", 3);
    assert_eq!(withdrawn.outcome, Outcome::Withdrawn);
    assert_eq!(withdrawn.intent_at, Some(100));
    assert_eq!(withdrawn.entered_at, Some(100));
    assert_eq!(withdrawn.duration_ms, Some(100));
    assert_eq!(Outcome::parse("withdrawn"), Some(Outcome::Withdrawn));
    t.store.write(|tx| {
        assert!(tx.execute("UPDATE codemode_calls SET outcome='cancelled',code=NULL,duration_ms=100 WHERE run_id='withdrawal' AND position=2",[]).is_err(), "cancelled still means never sent");
        assert!(tx.execute("UPDATE codemode_calls SET outcome='outcome_unknown',code='changed' WHERE run_id='withdrawal' AND position=3",[]).is_err(), "withdrawal is final");
        Ok(())
    }).unwrap();
}

#[test]
fn withdrawn_migration_preserves_calls_model_children_and_finality() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    for migration in crate::schema::MIGRATIONS.iter().filter(|m| m.version < 35) {
        conn.execute_batch(migration.statements).unwrap();
    }
    let tx = conn.transaction().unwrap();
    insert_run(&tx, &new_run("old"), None).unwrap();
    insert_call(&tx, "old", 0, "model", 0, CallStart::Intent { at: 100 }).unwrap();
    record_outcome(&tx, "old", 0, Outcome::Ok, None, 110).unwrap();
    tx.execute(
        "INSERT INTO codemode_models VALUES('old',0,17,11,'{\"model\":true}')",
        [],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO codemode_broca_calls VALUES('send-1','old',0,'{\"pending\":true}')",
        [],
    )
    .unwrap();
    tx.commit().unwrap();
    conn.execute_batch(
        crate::schema::MIGRATIONS
            .iter()
            .find(|m| m.version == 35)
            .unwrap()
            .statements,
    )
    .unwrap();
    let call = calls(&conn, "old").unwrap().remove(0);
    assert_eq!(call.outcome, Outcome::Ok);
    assert_eq!(call.intent_at, Some(100));
    assert_eq!(call.entered_at, Some(100));
    assert_eq!(call.duration_ms, Some(10));
    assert_eq!(
        conn.query_row(
            "SELECT reserved,charged,envelope FROM codemode_models",
            [],
            |r| Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?
            ))
        )
        .unwrap(),
        (17, 11, "{\"model\":true}".into())
    );
    assert_eq!(
        conn.query_row(
            "SELECT snapshot FROM codemode_broca_calls WHERE send_id='send-1'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "{\"pending\":true}"
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
    assert!(conn.execute("UPDATE codemode_calls SET outcome='error',code='changed' WHERE run_id='old' AND position=0",[]).is_err());
}

/// Migration 38 adds `held` to codemode_calls by rebuilding the table. The
/// existing columns, the rows that reference calls, and every trigger must
/// come through unchanged, and earlier holds, all of which were counted as
/// person waits, become holds.
#[test]
fn held_migration_backfills_person_waits_and_preserves_calls_children_and_triggers() {
    const OLD_COLUMNS: &str = "SELECT run_id,position,tool,idempotency_key,input_bytes,intent_at,outcome,code,duration_ms,entered_at,waited_on_person FROM codemode_calls ORDER BY position";
    fn rows(conn: &rusqlite::Connection, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let mut statement = conn.prepare(sql).unwrap();
        let width = statement.column_count();
        statement
            .query_map([], |row| {
                (0..width)
                    .map(|i| row.get::<_, rusqlite::types::Value>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }
    const TRIGGERS: &str =
        "SELECT name, tbl_name, sql FROM sqlite_master WHERE type='trigger' ORDER BY name";
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    for migration in crate::schema::MIGRATIONS.iter().filter(|m| m.version < 38) {
        conn.execute_batch(migration.statements).unwrap();
    }
    let tx = conn.transaction().unwrap();
    insert_run(&tx, &new_run("old"), None).unwrap();
    for (position, tool, intent, outcome, code, duration, entered, person) in [
        (0, "model", Some(100), "ok", None, Some(10), Some(100), 0),
        (
            1,
            "browser_use",
            Some(100),
            "withdrawn",
            Some("withdrawn"),
            Some(20),
            Some(100),
            1,
        ),
        (2, "read", Some(100), "pending", None, None, Some(100), 0),
        (3, "read", Some(100), "pending", None, None, Some(100), 1),
        (4, "read", None, "cancelled", None, None, None, 0),
    ] {
        tx.execute(
            "INSERT INTO codemode_calls(run_id,position,tool,idempotency_key,input_bytes,intent_at,outcome,code,duration_ms,entered_at,waited_on_person) VALUES('old',?1,?2,?3,7,?4,?5,?6,?7,?8,?9)",
            params![position, tool, format!("old-{position}"), intent, outcome, code, duration, entered, person],
        )
        .unwrap();
    }
    tx.execute(
        "INSERT INTO codemode_models VALUES('old',0,17,11,'{\"model\":true}')",
        [],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO codemode_broca_calls VALUES('send-1','old',0,'{\"pending\":true}')",
        [],
    )
    .unwrap();
    tx.execute("INSERT INTO broca_waits(send_id) VALUES('send-1')", [])
        .unwrap();
    tx.commit().unwrap();
    let calls_before = rows(&conn, OLD_COLUMNS);
    let triggers_before = rows(&conn, TRIGGERS);
    conn.execute_batch(
        crate::schema::MIGRATIONS
            .iter()
            .find(|m| m.version == 38)
            .unwrap()
            .statements,
    )
    .unwrap();
    assert_eq!(rows(&conn, OLD_COLUMNS), calls_before);
    assert_eq!(rows(&conn, TRIGGERS), triggers_before);
    let held: Vec<i64> = conn
        .prepare("SELECT held FROM codemode_calls ORDER BY position")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(held, [0, 1, 0, 1, 0], "held starts as waited_on_person");
    assert_eq!(
        rows(&conn, "SELECT * FROM codemode_models"),
        rows(&conn, "SELECT 'old',0,17,11,'{\"model\":true}'")
    );
    assert_eq!(
        rows(&conn, "SELECT * FROM codemode_broca_calls"),
        rows(&conn, "SELECT 'send-1','old',0,'{\"pending\":true}'")
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
    // The checks the rebuild adds: a person wait is a hold, and withdrawal
    // needs a hold but not a person.
    assert!(
        conn.execute(
            "UPDATE codemode_calls SET waited_on_person=1 WHERE run_id='old' AND position=2",
            []
        )
        .is_err()
    );
    assert!(conn.execute("UPDATE codemode_calls SET outcome='withdrawn',code='withdrawn',duration_ms=5 WHERE run_id='old' AND position=2", []).is_err());
    conn.execute(
        "UPDATE codemode_calls SET held=1 WHERE run_id='old' AND position=2",
        [],
    )
    .unwrap();
    conn.execute("UPDATE codemode_calls SET outcome='withdrawn',code='withdrawn',duration_ms=5 WHERE run_id='old' AND position=2", [])
        .unwrap();
    // The recreated triggers still act.
    assert!(conn.execute("UPDATE codemode_calls SET outcome='error',code='changed' WHERE run_id='old' AND position=0", []).is_err());
    conn.execute(
        "DELETE FROM codemode_broca_calls WHERE send_id='send-1'",
        [],
    )
    .unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM broca_waits", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0,
        "deleting a codemode Broca call still forgets its wait"
    );
}
