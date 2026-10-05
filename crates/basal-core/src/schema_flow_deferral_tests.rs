use super::*;
use rusqlite::{Connection, types::Value};

fn old_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for migration in MIGRATIONS.iter().filter(|m| m.version < 9) {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO flows (flow_id,created_at) VALUES ('f',1); INSERT INTO runs (run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at) VALUES ('r','f','t',1,'{}','return 1','{}',zeroblob(32),'pending',1)").unwrap();
    for (pos, dispatch) in ["none", "sent", "accepted", "unknown", "not_applied"]
        .iter()
        .enumerate()
    {
        conn.execute("INSERT INTO journal (run_id,position,kind_code,args,args_digest,idempotency_key,class,dispatch,issued_generation,unknown_reason) VALUES ('r',?1,1,'{}',zeroblob(32),?2,'query',?3,1,?4)",rusqlite::params![pos as i64,format!("key:{pos}"),dispatch,if *dispatch=="unknown" {Some("connection_lost")} else {None}]).unwrap();
    }
    conn.execute_batch("INSERT INTO mailbox (seq,run_id,position,settlement,value,payload_hash,source,arrived_generation) VALUES (9,'r',1,'fulfilled','7',zeroblob(32),'host',1); INSERT INTO broca_calls VALUES ('key:2','r',2,'{\"snapshot\":true}')").unwrap();
    conn
}
fn rows(conn: &Connection, table: &str, columns: usize) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
        .unwrap();
    stmt.query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
#[test]
fn migration_preserves_all_existing_dispatch_states_and_foreign_key_children() {
    let conn = old_store();
    let count = conn
        .prepare("SELECT * FROM journal")
        .unwrap()
        .column_count();
    let before = rows(&conn, "journal", count);
    let mailbox = rows(&conn, "mailbox", 9);
    let broca = rows(&conn, "broca_calls", 4);
    let flows = rows(&conn, "flows", 8);
    conn.execute_batch(FLOW_DEFERRALS).unwrap();
    assert_eq!(rows(&conn, "journal", count), before);
    assert_eq!(rows(&conn, "mailbox", 9), mailbox);
    assert_eq!(rows(&conn, "broca_calls", 4), broca);
    assert_eq!(rows(&conn, "flows", 8), flows);
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
#[test]
fn deferred_state_requires_closed_typed_refusal_and_retry_metadata() {
    let conn = old_store();
    conn.execute_batch(FLOW_DEFERRALS).unwrap();
    conn.execute_batch("UPDATE journal SET dispatch='deferred',refusal='scope_ended',retry_not_before=250,refusal_detail='{\"reason\":\"scope_ended\",\"provider\":\"mock\",\"action\":\"send\"}' WHERE position=0").unwrap();
    for mutation in [
        "refusal=NULL",
        "retry_not_before=NULL",
        "refusal_detail=NULL",
        "refusal='new_reason',refusal_detail='{\"reason\":\"new_reason\",\"provider\":\"mock\",\"action\":\"send\"}'",
        "retry_not_before='tomorrow'",
        "dispatch='other',refusal=NULL,retry_not_before=NULL,refusal_detail=NULL",
        "refusal_detail='null'",
        "refusal_detail='{}'",
        "dispatch='sent'",
        "refusal='no_flow_scope',refusal_detail='{\"reason\":\"no_flow_scope\",\"provider\":\"mock\",\"action\":\"send\"}'",
        "refusal='target_flow_unsupported',refusal_detail='{\"reason\":\"target_flow_unsupported\",\"provider\":\"mock\",\"action\":\"send\"}'",
    ] {
        assert!(
            conn.execute_batch(&format!("UPDATE journal SET {mutation} WHERE position=0"))
                .is_err(),
            "accepted {mutation}"
        );
    }
    conn.execute_batch("UPDATE journal SET dispatch='sent',refusal=NULL,retry_not_before=NULL,refusal_detail=NULL WHERE position=0").unwrap();
}
