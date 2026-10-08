use super::MIGRATIONS;
use crate::retention::{
    BATCH, CANDIDATES_SQL, HISTORY_BATCH_SQL, HISTORY_PREDICATES, OBLIGATIONS,
    RATE_WINDOWS_PREDICATE,
};
use rusqlite::{Connection, types::Value};
use std::collections::BTreeMap;

// Written out here rather than read from the migration, so removing or
// changing a migration statement makes these tests fail instead of changing
// what they expect.
const EXPECTED_INDEXES: &[(&str, &str)] = &[
    (
        "runs_retention",
        "CREATE INDEX runs_retention ON runs(state,ended_at,run_id) WHERE state IN ('succeeded','failed','engine_mismatch','cancelled')",
    ),
    (
        "quarantine_obligation",
        "CREATE INDEX quarantine_obligation ON quarantine(run_id,position,reason)",
    ),
    (
        "outbox_refusal_run",
        "CREATE INDEX outbox_refusal_run ON outbox(CAST(json_extract(body,'$.run_id') AS TEXT)) WHERE kind='refusal_committed'",
    ),
    (
        "call_audit_refund_window",
        "CREATE INDEX call_audit_refund_window ON call_audit(flow_id,at,run_id)",
    ),
    (
        "token_ledger_retention_window",
        "CREATE INDEX token_ledger_retention_window ON token_ledger(flow_id,window_ms,window_start,run_id,state)",
    ),
    (
        "quarantine_retention",
        "CREATE INDEX quarantine_retention ON quarantine(at)",
    ),
    (
        "call_audit_retention",
        "CREATE INDEX call_audit_retention ON call_audit(at)",
    ),
    (
        "audit_retention",
        "CREATE INDEX audit_retention ON audit(at)",
    ),
    (
        "outbox_retention",
        "CREATE INDEX outbox_retention ON outbox(delivered_at) WHERE delivered_at IS NOT NULL AND kind<>'refusal_committed'",
    ),
    (
        "schedule_dropped_retention",
        "CREATE INDEX schedule_dropped_retention ON schedule_dropped(at)",
    ),
    (
        "token_ledger_retention",
        "CREATE INDEX token_ledger_retention ON token_ledger(settled_at) WHERE state='settled'",
    ),
    (
        "token_windows_retention",
        "CREATE INDEX token_windows_retention ON token_windows(window_start+window_ms)",
    ),
    (
        "rate_windows_retention",
        "CREATE INDEX rate_windows_retention ON rate_windows(window_start+window_ms)",
    ),
    (
        "decision_cards_retention",
        "CREATE INDEX decision_cards_retention ON decision_cards(answered_at) WHERE state<>'open'",
    ),
];

fn populated_schema_11() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for migration in MIGRATIONS.iter().filter(|m| m.version <= 11) {
        conn.execute_batch(migration.statements).unwrap();
    }
    // A small expired suffix amid a larger retained population, with both live
    // and deleted runs, every run/card state, unsent notifications, refusals,
    // and reserved/settled ledgers. IDs and rowids do not sort by age.
    conn.execute_batch(r#"
CREATE TEMP TABLE fixture(i INTEGER PRIMARY KEY);
WITH RECURSIVE numbers(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM numbers WHERE i<2047)
INSERT INTO fixture SELECT i FROM numbers;
INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,owner,admitted_at,ended_at,admit_seq)
SELECT 'run-'||i,'flow-'||(i%16),'trigger-'||i,1,'{}','return 1','{}',zeroblob(32),
       CASE i%8 WHEN 0 THEN 'pending' WHEN 1 THEN 'running' WHEN 2 THEN 'suspended'
       WHEN 3 THEN 'succeeded' WHEN 4 THEN 'failed' WHEN 5 THEN 'needs_reconcile'
       WHEN 6 THEN 'engine_mismatch' ELSE 'cancelled' END,
       CASE WHEN i%8=1 THEN 'parent' END,1,
       CASE WHEN i%8 IN (3,4,6,7) THEN 2048-i END,i FROM fixture;
INSERT INTO trigger_inbox SELECT flow_id,trigger_id,attempt,run_id,admitted_at FROM runs;
INSERT INTO journal(run_id,position,kind_code,args,args_digest,idempotency_key,class,dispatch,issued_generation)
SELECT 'run-'||i,0,1,'{}',zeroblob(32),'call-'||i,'query','sent',1 FROM fixture WHERE i%4=0;
INSERT INTO quarantine(run_id,position,settlement,value,payload_hash,reason,at)
SELECT CASE WHEN i%2=0 THEN 'run-'||i ELSE 'deleted-'||i END,0,'fulfilled','1',zeroblob(32),
       CASE i%4 WHEN 0 THEN 'contradicts_outcome' WHEN 1 THEN 'unknown_call'
       WHEN 2 THEN 'handle_mismatch' ELSE 'run_cancelled' END,2048-i FROM fixture;
INSERT INTO quarantine(run_id,position,settlement,value,payload_hash,reason,at)
SELECT run_id,position,settlement,value,payload_hash,'run_cancelled',at FROM quarantine WHERE reason='contradicts_outcome';
INSERT INTO call_audit
SELECT CASE WHEN i%2=0 THEN 'run-'||i ELSE 'deleted-'||i END,0,'flow-'||(i%16),'echo',zeroblob(32),'fulfilled',2048-i FROM fixture;
INSERT INTO audit(at,actor,action,run_id,detail,elicitation_id)
SELECT 2048-i,'operator','test',CASE WHEN i%2=0 THEN 'run-'||i ELSE 'deleted-'||i END,'{}','card-'||i FROM fixture;
INSERT INTO outbox(at,kind,flow_id,body,delivered_at)
SELECT 2048-i,CASE WHEN i%3=0 THEN 'refusal_committed' ELSE 'flow.disabled' END,
       'flow-'||(i%16),json_object('run_id','run-'||i),CASE WHEN i%3<>1 THEN 2048-i END FROM fixture;
INSERT INTO schedule_dropped(flow_id,trigger_id,due_ms,payload,reason,at)
SELECT 'flow-'||(i%16),'dropped-'||i,1,'{}',
       CASE i%3 WHEN 0 THEN 'disabled' WHEN 1 THEN 'not_approved' ELSE 'rate_limited' END,2048-i FROM fixture;
INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,reserved_at,settled_at)
SELECT 'send-'||i,'flow-'||(i%16),CASE WHEN i%2=0 THEN 'run-'||i ELSE 'deleted-'||i END,0,10,2048-i,10,
       CASE WHEN i%3=0 THEN 'reserved' ELSE 'settled' END,1,CASE WHEN i%3<>0 THEN 2048-i END FROM fixture;
INSERT INTO token_windows(flow_id,window_ms,window_start,reserved) SELECT 'flow-'||(i%16),10,2048-i,10 FROM fixture;
INSERT INTO rate_windows(flow_id,window_ms,window_start,runs) SELECT 'flow-'||(i%16),10,2048-i,1 FROM fixture;
INSERT INTO decision_cards(dedup_key,kind,flow_id,version,run_id,position,call_key,instance,card,state,created_at,answered_at,elicitation_id)
SELECT 'decision-'||i,CASE WHEN i%2=0 THEN 'reconcile' ELSE 'reenable' END,'flow-'||(i%16),1,
       CASE WHEN i%2=0 THEN 'run-'||i END,CASE WHEN i%2=0 THEN 0 END,CASE WHEN i%2=0 THEN 'call-'||i END,i,'{}',
       CASE i%5 WHEN 0 THEN 'open' WHEN 1 THEN 'applied' WHEN 2 THEN 'declined' WHEN 3 THEN 'expired' ELSE 'stale' END,
       1,CASE WHEN i%5<>0 THEN 2048-i END,'card-'||i FROM fixture;
DROP TABLE fixture;
"#).unwrap();
    conn
}

fn migrate(conn: &Connection) {
    conn.execute_batch(
        MIGRATIONS
            .iter()
            .find(|m| m.version == 12)
            .expect("retention indexes migration")
            .statements,
    )
    .unwrap();
}

fn snapshot(conn: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn indexes(conn: &Connection) -> BTreeMap<String, Option<String>> {
    conn.prepare("SELECT name,sql FROM sqlite_master WHERE type='index' ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn additive_retention_indexes_preserve_populated_tables_in_every_state() {
    let conn = populated_schema_11();
    let tables = [
        "runs",
        "trigger_inbox",
        "journal",
        "quarantine",
        "call_audit",
        "audit",
        "outbox",
        "schedule_dropped",
        "token_ledger",
        "token_windows",
        "rate_windows",
        "decision_cards",
    ];
    let before: Vec<_> = tables
        .into_iter()
        .map(|table| {
            let rows = snapshot(&conn, table);
            assert!(!rows.is_empty(), "missing fixture for {table}");
            (table, rows)
        })
        .collect();
    let indexes_before = indexes(&conn);
    migrate(&conn);
    for (table, rows) in before {
        assert_eq!(snapshot(&conn, table), rows, "{table}");
    }
    let indexes_after = indexes(&conn);
    for (name, sql) in &indexes_before {
        assert_eq!(indexes_after.get(name), Some(sql), "existing index {name}");
    }
    let added: BTreeMap<_, _> = indexes_after
        .into_iter()
        .filter(|(name, _)| !indexes_before.contains_key(name))
        .collect();
    let expected = EXPECTED_INDEXES
        .iter()
        .map(|(name, sql)| ((*name).to_owned(), Some((*sql).to_owned())))
        .collect();
    assert_eq!(added, expected);
    assert!(
        conn.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}

fn candidates_query(unsettled: bool) -> String {
    CANDIDATES_SQL
        .replace("{predicate}", if unsettled { "" } else { "NOT" })
        .replace("{OBLIGATIONS}", OBLIGATIONS)
        .replace("{BATCH}", &BATCH.to_string())
}

fn history_query(table: &str) -> String {
    let predicate = if table == "rate_windows" {
        RATE_WINDOWS_PREDICATE
    } else {
        HISTORY_PREDICATES
            .iter()
            .find(|(name, _)| *name == table)
            .unwrap()
            .1
    };
    HISTORY_BATCH_SQL
        .replace("{table}", table)
        .replace("{predicate}", predicate)
        .replace("{BATCH}", &BATCH.to_string())
}

fn assert_plan(conn: &Connection, sql: &str, index: &str) {
    for analyzed in [false, true] {
        if analyzed {
            conn.execute_batch("ANALYZE").unwrap();
        }
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([64], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        println!("{index} (analyzed={analyzed}): {plan:?}");
        assert!(
            plan.iter().any(
                |step| step.starts_with("SEARCH ") && step.contains(&format!("INDEX {index} "))
            ),
            "{index}: {plan:?}"
        );
    }
}

fn plan_store() -> Connection {
    let conn = populated_schema_11();
    migrate(&conn);
    conn
}

#[test]
fn runs_retention_plan() {
    for unsettled in [false, true] {
        let conn = plan_store();
        assert_plan(&conn, &candidates_query(unsettled), "runs_retention");
    }
}

#[test]
fn outbox_refusal_run_plan() {
    for unsettled in [false, true] {
        let conn = plan_store();
        assert_plan(&conn, &candidates_query(unsettled), "outbox_refusal_run");
    }
}

#[test]
fn quarantine_obligation_plan() {
    for unsettled in [false, true] {
        let conn = plan_store();
        assert_plan(&conn, &candidates_query(unsettled), "quarantine_obligation");
    }
}

#[test]
fn call_audit_refund_window_plan() {
    assert_plan(
        &plan_store(),
        &history_query("rate_windows"),
        "call_audit_refund_window",
    );
}

#[test]
fn token_ledger_retention_window_plan() {
    assert_plan(
        &plan_store(),
        &history_query("token_windows"),
        "token_ledger_retention_window",
    );
}

macro_rules! history_plan_test {
    ($name:ident, $table:literal, $index:literal) => {
        #[test]
        fn $name() {
            assert_plan(&plan_store(), &history_query($table), $index);
        }
    };
}

history_plan_test!(
    quarantine_retention_plan,
    "quarantine",
    "quarantine_retention"
);
history_plan_test!(
    call_audit_retention_plan,
    "call_audit",
    "call_audit_retention"
);
history_plan_test!(audit_retention_plan, "audit", "audit_retention");
history_plan_test!(outbox_retention_plan, "outbox", "outbox_retention");
history_plan_test!(
    schedule_dropped_retention_plan,
    "schedule_dropped",
    "schedule_dropped_retention"
);
history_plan_test!(
    token_ledger_retention_plan,
    "token_ledger",
    "token_ledger_retention"
);
history_plan_test!(
    token_windows_retention_plan,
    "token_windows",
    "token_windows_retention"
);
history_plan_test!(
    rate_windows_retention_plan,
    "rate_windows",
    "rate_windows_retention"
);
history_plan_test!(
    decision_cards_retention_plan,
    "decision_cards",
    "decision_cards_retention"
);

#[test]
fn indexable_ordering_preserves_candidate_and_history_batches() {
    let conn = plan_store();
    for cutoff in [64, 512, 2048] {
        for unsettled in [false, true] {
            let sql = candidates_query(unsettled);
            let original = sql
                .replace("ORDER BY +r.run_id", "ORDER BY r.run_id")
                .replace(
                    "CAST(json_extract(o.body,'$.run_id') AS TEXT)",
                    "json_extract(o.body,'$.run_id')",
                );
            let read = |query: &str| -> Vec<String> {
                conn.prepare(query)
                    .unwrap()
                    .query_map([cutoff], |r| r.get(0))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            };
            let rows = read(&sql);
            assert_eq!(
                rows,
                read(&original),
                "unsettled={unsettled}, cutoff={cutoff}"
            );
            assert!(rows.len() <= 128);
        }
        for table in HISTORY_PREDICATES
            .iter()
            .map(|(table, _)| *table)
            .chain(["rate_windows"])
        {
            let sql = history_query(table);
            let original = sql.replace("ORDER BY +rowid", "ORDER BY rowid");
            conn.execute_batch("SAVEPOINT batch").unwrap();
            let deleted = conn.execute(&sql, [cutoff]).unwrap();
            assert!(deleted <= 128);
            let rows = snapshot(&conn, table);
            conn.execute_batch("ROLLBACK TO batch").unwrap();
            assert_eq!(
                conn.execute(&original, [cutoff]).unwrap(),
                deleted,
                "{table}"
            );
            assert_eq!(snapshot(&conn, table), rows, "{table}, cutoff={cutoff}");
            conn.execute_batch("ROLLBACK TO batch; RELEASE batch")
                .unwrap();
        }
    }
}

#[test]
fn refusal_expression_preserves_run_id_text_affinity() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE runs(run_id TEXT PRIMARY KEY); INSERT INTO runs VALUES ('1'),('1.5'),('text'),('null'),('01'),('[1]'),('{\"a\":1}');").unwrap();
    // json_extract returns SQL NULL, integer, real or text. The explicit cast
    // must preserve the implicit TEXT affinity of comparison to runs.run_id.
    for body in [
        r#"{"run_id":1}"#,
        r#"{"run_id":true}"#,
        r#"{"run_id":1.5}"#,
        r#"{"run_id":"text"}"#,
        r#"{"run_id":"01"}"#,
        r#"{"run_id":null}"#,
        r#"{}"#,
        r#"{"run_id":[1]}"#,
        r#"{"run_id":{"a":1}}"#,
    ] {
        let read = |query: &str| -> Vec<String> {
            conn.prepare(query)
                .unwrap()
                .query_map([body], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(
            read(
                "SELECT run_id FROM runs WHERE CAST(json_extract(?1,'$.run_id') AS TEXT)=run_id ORDER BY run_id"
            ),
            read(
                "SELECT run_id FROM runs WHERE json_extract(?1,'$.run_id')=run_id ORDER BY run_id"
            ),
            "{body}"
        );
    }
}
