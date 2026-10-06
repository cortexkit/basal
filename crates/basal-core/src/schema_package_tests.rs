use super::*;
use rusqlite::{Connection, types::Value};

fn rows(conn: &Connection, table: &str, columns: usize) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
        .unwrap();
    stmt.query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn additive_package_migration_preserves_rows_in_every_schema_nine_table() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for m in MIGRATIONS.iter().filter(|m| m.version <= 9) {
        conn.execute_batch(m.statements).unwrap();
    }
    conn.execute_batch(r#"
INSERT INTO meta VALUES ('store_id','existing');
INSERT INTO flows(flow_id,owner,created_at) VALUES ('f','operator',1);
INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,installed_at) VALUES ('f',1,zeroblob(32),'{}','return 1','operator','validated',1);
INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at) VALUES ('r','f','t',1,'{}','return 1','{}',zeroblob(32),'pending',1);
INSERT INTO trigger_inbox VALUES ('f','t',1,'r',1);
INSERT INTO tombstones VALUES ('trigger','old','old-run',1);
INSERT INTO journal(run_id,position,kind_code,args,args_digest,idempotency_key,class,dispatch,issued_generation) VALUES ('r',0,1,'{}',zeroblob(32),'k','query','sent',1);
INSERT INTO mailbox(run_id,position,settlement,value,payload_hash,source,arrived_generation) VALUES ('r',0,'fulfilled','1',zeroblob(32),'host',1);
INSERT INTO quarantine(run_id,position,settlement,value,payload_hash,reason,at) VALUES ('r',0,'fulfilled','2',zeroblob(32),'contradicts_outcome',1);
INSERT INTO activations VALUES ('r',1,'parent',1);
INSERT INTO audit(at,actor,action,detail) VALUES (1,'operator','test','{}');
INSERT INTO schedules VALUES ('f',1,'{}','active',1,2,NULL,1);
INSERT INTO schedule_fires(flow_id,trigger_id,version,due_ms,payload,planned_at) VALUES ('f','due',1,2,'{}',1);
INSERT INTO schedule_dropped(flow_id,trigger_id,due_ms,payload,reason,at) VALUES ('f','dropped',2,'{}','disabled',1);
INSERT INTO install_cards(card_id,flow_id,version,code_hash,author,card,state,created_at) VALUES ('c','f',1,zeroblob(32),'operator','{}','pending',1);
INSERT INTO broca_calls VALUES ('k','r',0,'{}');
INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at) VALUES ('d','reenable','f',1,1,'{}','open',1);
INSERT INTO call_audit VALUES ('r',0,'f','echo',zeroblob(32),'fulfilled',1);
INSERT INTO kv VALUES ('f','key','1',1,1);
INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,reserved_at) VALUES ('k','f','r',0,60000,0,10,'reserved',1);
INSERT INTO token_windows(flow_id,window_ms,window_start,reserved) VALUES ('f',60000,0,10);
INSERT INTO rate_windows(flow_id,window_ms,window_start,runs) VALUES ('f',60000,0,1);
INSERT INTO outbox(at,kind,flow_id,recipient,body) VALUES (1,'flow.disabled','f','operator','{}');
"#).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let before: Vec<_> = tables
        .iter()
        .map(|table| {
            let columns = conn
                .prepare(&format!("SELECT * FROM {table}"))
                .unwrap()
                .column_count();
            let snapshot = rows(&conn, table, columns);
            assert!(!snapshot.is_empty(), "{table} has no fixture row");
            (table, columns, snapshot)
        })
        .collect();
    assert_eq!(
        tables.len(),
        24,
        "every schema-nine table, including SQLite sequence metadata"
    );
    let tx = conn.transaction().unwrap();
    tx.execute_batch(PACKAGES).unwrap();
    tx.commit().unwrap();
    for (table, columns, expected) in before {
        assert_eq!(rows(&conn, table, columns), expected, "{table}");
    }
    assert!(
        conn.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM package_versions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
