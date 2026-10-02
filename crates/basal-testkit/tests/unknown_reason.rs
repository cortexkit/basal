//! The journal's unknown reason is required by the schema: a call marked
//! unknown without one is refused, and the migration that adds the column
//! refuses a store that already holds an unknown call, whose reason can no
//! longer be known.

use basal_core::schema::MIGRATIONS;
use rusqlite::{Connection, params};

/// A connection with the schema migrated up to, not including, `upto`.
fn migrated(upto: u32) -> Connection {
    let conn = Connection::open_in_memory().expect("memory db");
    for m in MIGRATIONS.iter().filter(|m| m.version < upto) {
        conn.execute_batch(m.statements)
            .unwrap_or_else(|e| panic!("migration {}: {e}", m.version));
    }
    conn
}

/// A run with one sent call of `mock.post`, its dispatch state `dispatch`.
fn call(conn: &Connection, dispatch: &str) {
    conn.execute(
        "INSERT INTO runs (run_id, flow_id, trigger_id, attempt, trigger, script, manifest, \
         code_hash, state, admitted_at) VALUES ('r', 'f', 't', 1, 'null', '', '{}', ?1, \
         'needs_reconcile', 0)",
        params![vec![0u8; 32]],
    )
    .expect("run");
    conn.execute(
        "INSERT INTO journal (run_id, position, kind_code, module, op, args, args_digest, \
         idempotency_key, class, dispatch, attempts, issued_generation) \
         VALUES ('r', 0, 0, 'mock', 'post', '{}', ?1, 'k', 'mutation', ?2, 1, 1)",
        params![vec![0u8; 32], dispatch],
    )
    .expect("call");
}

#[test]
fn the_migration_refuses_a_store_that_already_holds_an_unknown_call() {
    let version = MIGRATIONS.last().expect("migrations").version;
    let conn = migrated(version);
    call(&conn, "unknown");
    let last = MIGRATIONS.last().unwrap();
    let error = conn
        .execute_batch(last.statements)
        .expect_err("the migration refuses")
        .to_string();
    assert!(
        error.contains("this_store_holds_unknown_calls_recorded_without_a_reason"),
        "{error}"
    );
    // A store without an unknown call migrates.
    let conn = migrated(version);
    call(&conn, "sent");
    conn.execute_batch(last.statements).expect("migrates");
}

#[test]
fn a_call_marked_unknown_needs_a_reason_from_the_closed_set() {
    let conn = migrated(u32::MAX);
    call(&conn, "sent");
    let set = |dispatch: &str, reason: Option<&str>| {
        conn.execute(
            "UPDATE journal SET dispatch = ?1, unknown_reason = ?2 WHERE run_id = 'r'",
            params![dispatch, reason],
        )
    };
    assert!(set("unknown", None).is_err(), "unknown without a reason");
    assert!(
        set("unknown", Some("lost")).is_err(),
        "a reason outside the set"
    );
    for reason in basal_host::UnknownReason::ALL {
        set("unknown", Some(reason.as_str())).expect(reason.as_str());
    }
    // Sent again after a not-applied reconcile, the call keeps its last
    // reason until a new one is recorded.
    set("sent", Some("basal_restarted")).expect("resent");
}
