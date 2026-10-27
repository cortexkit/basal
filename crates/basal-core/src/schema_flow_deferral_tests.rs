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

#[test]
fn grant_loss_migration_preserves_journal_children_and_backfills_retirement() {
    let conn = old_store();
    for migration in MIGRATIONS.iter().filter(|m| (9..16).contains(&m.version)) {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch("UPDATE journal SET dispatch='deferred',refusal='consent_unavailable',retry_not_before=9223372036854775807,refusal_detail='{\"reason\":\"consent_unavailable\",\"provider\":\"plexus\",\"action\":\"github\"}' WHERE position=0; UPDATE flows SET state='disabled',owner='old-agent',disabled_by='core',disabled_reason='agent_retired',disabled_at=20").unwrap();
    let count = conn
        .prepare("SELECT * FROM journal")
        .unwrap()
        .column_count();
    let journal = rows(&conn, "journal", count);
    let mailbox = rows(&conn, "mailbox", 9);
    let broca = rows(&conn, "broca_calls", 4);
    let indexes = || {
        conn.prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name IN ('journal','mailbox','broca_calls') AND name NOT LIKE 'sqlite_autoindex_%' ORDER BY name")
            .unwrap().query_map([], |r| r.get::<_,String>(0)).unwrap()
            .collect::<Result<Vec<_>,_>>().unwrap()
    };
    let before_indexes = indexes();
    conn.execute_batch(GRANT_LOSSES).unwrap();
    assert_eq!(indexes(), before_indexes);
    assert_eq!(rows(&conn, "journal", count), journal);
    assert_eq!(rows(&conn, "mailbox", 9), mailbox);
    assert_eq!(rows(&conn, "broca_calls", 4), broca);
    assert_eq!(
        conn.query_row(
            "SELECT json_extract(agent_retirement,'$.agent_id') FROM flows WHERE flow_id='f'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "old-agent"
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, u64>(
            0
        ))
        .unwrap(),
        0
    );
    for code in ["module_grant_absent", "agent_grant_absent"] {
        conn.execute(
            "UPDATE journal SET refusal=?1,refusal_detail=?2 WHERE position=0",
            rusqlite::params![
                code,
                serde_json::json!({"reason":code,"provider":"plexus","action":"github"})
                    .to_string()
            ],
        )
        .unwrap();
    }
}

#[test]
fn grant_decision_migration_preserves_existing_cards_and_named_indexes() {
    let conn = old_store();
    for migration in MIGRATIONS.iter().filter(|m| (9..17).contains(&m.version)) {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO decision_cards(seq,dedup_key,kind,flow_id,version,run_id,position,call_key,instance,card,state,created_at) VALUES (7,'reconcile-key','reconcile','f',1,'r',0,'call',1,'{}','open',0); INSERT INTO decision_cards(seq,dedup_key,kind,flow_id,version,instance,card,state,created_at,answered_at) VALUES (9,'reenable-key','reenable','f',1,1,'{}','expired',0,10)").unwrap();
    let before = rows(&conn, "decision_cards", 17);
    let indexes = || {
        conn.prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='decision_cards' AND name NOT LIKE 'sqlite_autoindex_%' ORDER BY name").unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
    };
    let before_indexes = indexes();
    conn.execute_batch(GRANT_DECISIONS).unwrap();
    assert_eq!(rows(&conn, "decision_cards", 17), before);
    assert_eq!(indexes(), before_indexes);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM decision_withdrawals", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}

/// A store opened the normal way, through every migration, carries the
/// journal table the latest migration rebuilt. Its refusal list, not the list an
/// earlier migration created, decides which refusals a deferred call may hold.
#[test]
fn current_store_refuses_a_deferred_call_with_a_refusal_outside_the_closed_set() {
    use crate::store::{Durability, Store};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "basal-current-refusal-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let store = Store::open(dir.join("core.db"), Durability::throwaway()).unwrap();
    store
        .write(|tx| {
            tx.execute_batch("INSERT INTO flows (flow_id,created_at) VALUES ('f',1); INSERT INTO runs (run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at) VALUES ('r','f','t',1,'{}','return 1','{}',zeroblob(32),'pending',1)")?;
            Ok(())
        })
        .unwrap();
    let defer = |position: i64, refusal: &str| {
        store.write(|tx| {
            Ok(tx.execute(
                "INSERT INTO journal (run_id,position,kind_code,args,args_digest,idempotency_key,class,dispatch,issued_generation,refusal,retry_not_before,refusal_detail) VALUES ('r',?1,1,'{}',zeroblob(32),?2,'query','deferred',1,?3,250,json_object('reason',?3,'provider','mock','action','send'))",
                rusqlite::params![position, format!("key:{position}"), refusal],
            ))
        })
        .unwrap()
    };
    // A refusal from the closed set is accepted, so the rejections below
    // come from the refusal list rather than from some other constraint.
    defer(0, "scope_ended").expect("a deferred call with a listed refusal");
    // An unlisted code, and the two flow refusals that settle a call as
    // refused rather than deferring it.
    for refusal in ["new_reason", "no_flow_scope", "target_flow_unsupported"] {
        let error = defer(1, refusal).expect_err(refusal).to_string();
        assert!(
            error.contains("CHECK constraint failed"),
            "{refusal}: {error}"
        );
    }
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn target_scope_migration_preserves_existing_calls_and_children() {
    let conn = old_store();
    for migration in MIGRATIONS.iter().filter(|m| (9..23).contains(&m.version)) {
        conn.execute_batch(migration.statements).unwrap();
    }
    let columns = conn
        .prepare("SELECT * FROM journal")
        .unwrap()
        .column_count();
    let before = rows(&conn, "journal", columns);
    let indexes = || {
        conn.prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name IN ('journal','mailbox','broca_calls') ORDER BY name")
            .unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
    };
    let before_indexes = indexes();
    let mailbox = rows(&conn, "mailbox", 9);
    let broca = rows(&conn, "broca_calls", 4);
    conn.execute_batch(
        MIGRATIONS
            .iter()
            .find(|m| m.version == 23)
            .unwrap()
            .statements,
    )
    .unwrap();
    assert_eq!(rows(&conn, "journal", columns), before);
    assert_eq!(rows(&conn, "mailbox", 9), mailbox);
    assert_eq!(rows(&conn, "broca_calls", 4), broca);
    assert_eq!(indexes(), before_indexes);
    let flags: Vec<(bool, bool)> = conn
        .prepare("SELECT targeted,scope_cut FROM journal ORDER BY position")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(flags, vec![(false, false); 5]);
    assert!(
        conn.execute_batch("UPDATE journal SET targeted=1 WHERE position=0")
            .is_err()
    );
    assert!(
        conn.execute_batch("UPDATE journal SET scope_cut=2 WHERE position=0")
            .is_err()
    );
    conn.execute_batch(
        "UPDATE journal SET kind_code=0,module='mock',op='echo',targeted=1 WHERE position=0",
    )
    .unwrap();
    assert!(matches!(
        crate::journal::row(&conn, "r", 0).unwrap().unwrap().kind,
        basal_proto::CallKind::OpFor { .. }
    ));
    conn.execute_batch("UPDATE journal SET dispatch='deferred',refusal='sink_audience_unavailable',retry_not_before=100,refusal_detail='{\"reason\":\"sink_audience_unavailable\",\"provider\":\"prefrontal-core\",\"action\":\"flow.target_scope\"}' WHERE position=0").unwrap();
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
fn board_deferral_migration_preserves_calls_children_and_indexes() {
    let conn = old_store();
    for migration in MIGRATIONS.iter().filter(|m| (9..33).contains(&m.version)) {
        conn.execute_batch(migration.statements).unwrap();
    }
    let count = conn
        .prepare("SELECT * FROM journal")
        .unwrap()
        .column_count();
    let before = rows(&conn, "journal", count);
    let mailbox = rows(&conn, "mailbox", 9);
    let broca = rows(&conn, "broca_calls", 4);
    let indexes = || {
        conn.prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name IN ('journal','mailbox','broca_calls') ORDER BY name").unwrap()
        .query_map([], |r| r.get::<_, String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
    };
    let before_indexes = indexes();
    conn.execute_batch(BOARD_DEFERRALS).unwrap();
    assert_eq!(rows(&conn, "journal", count), before);
    assert_eq!(rows(&conn, "mailbox", 9), mailbox);
    assert_eq!(rows(&conn, "broca_calls", 4), broca);
    assert_eq!(indexes(), before_indexes);
    assert!(
        conn.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
    for code in ["sink_store_failed", "sink_audience_unavailable"] {
        conn.execute("UPDATE journal SET dispatch='deferred',refusal=?1,retry_not_before=100,refusal_detail=json_object('reason',?1,'provider','prefrontal-core','action','sink.board') WHERE position=0", [code]).unwrap();
    }
}

#[test]
fn review_secret_migration_45_preserves_populated_rows_and_allows_vault_backoff() {
    let conn = old_store();
    for migration in MIGRATIONS.iter().filter(|m| (9..45).contains(&m.version)) {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO broca_waits(send_id,broca_run_id,check_at) VALUES ('key:2','remote-run',10); INSERT INTO broca_wakes(send_id,event_key,kind,digest,received_at) VALUES ('key:2','notice','ended','digest',10); INSERT INTO outbox(at,kind,flow_id,body) VALUES (1,'legacy','f','{}'); INSERT INTO mailbox(seq,run_id,position,settlement,value,payload_hash,source,arrived_generation) VALUES (100,'r',3,'fulfilled','1',zeroblob(32),'host',1); DELETE FROM mailbox WHERE seq=100").unwrap();
    let count = conn
        .prepare("SELECT * FROM journal")
        .unwrap()
        .column_count();
    let journal = rows(&conn, "journal", count);
    let mailbox = rows(&conn, "mailbox", 9);
    let broca = rows(&conn, "broca_calls", 4);
    let waits = rows(&conn, "broca_waits", 4);
    let wakes = rows(&conn, "broca_wakes", 8);
    let outbox_count = conn.prepare("SELECT * FROM outbox").unwrap().column_count();
    let outbox = rows(&conn, "outbox", outbox_count);
    let indexes = |table: &str| {
        conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name=?1 ORDER BY name",
        )
        .unwrap()
        .query_map([table], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let journal_indexes = indexes("journal");
    let broca_indexes = indexes("broca_calls");
    conn.execute_batch(
        MIGRATIONS
            .iter()
            .find(|m| m.version == 45)
            .unwrap()
            .statements,
    )
    .unwrap();
    assert_eq!(rows(&conn, "journal", count), journal);
    assert_eq!(rows(&conn, "mailbox", 9), mailbox);
    assert_eq!(rows(&conn, "broca_calls", 4), broca);
    assert_eq!(rows(&conn, "broca_waits", 4), waits);
    assert_eq!(rows(&conn, "broca_wakes", 8), wakes);
    assert_eq!(rows(&conn, "outbox", outbox_count), outbox);
    assert_eq!(indexes("journal"), journal_indexes);
    assert_eq!(indexes("broca_calls"), broca_indexes);
    assert!(
        conn.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
    conn.execute("UPDATE journal SET dispatch='deferred',refusal='secret_vault_unavailable',retry_not_before=100,refusal_detail=json_object('reason','secret_vault_unavailable','provider','claustrum','action','secret.use') WHERE position=0", []).unwrap();
    assert!(conn.execute("UPDATE journal SET refusal='unknown',refusal_detail=json_object('reason','unknown','provider','claustrum','action','secret.use') WHERE position=0", []).is_err());
    conn.execute("DELETE FROM broca_calls WHERE send_id='key:2'", [])
        .unwrap();
    assert!(rows(&conn, "broca_waits", 4).is_empty());
    assert!(rows(&conn, "broca_wakes", 8).is_empty());
    conn.execute("INSERT INTO mailbox(run_id,position,settlement,value,payload_hash,source,arrived_generation) VALUES ('r',0,'fulfilled','1',zeroblob(32),'host',1)", []).unwrap();
    assert_eq!(conn.last_insert_rowid(), 101);
}

#[test]
fn review_migration45_preserves_mailbox_sequence_with_no_surviving_answers() {
    let conn = old_store();
    for migration in MIGRATIONS.iter().filter(|m| (9..45).contains(&m.version)) {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO mailbox(seq,run_id,position,settlement,value,payload_hash,source,arrived_generation) VALUES (100,'r',3,'fulfilled','1',zeroblob(32),'host',1); DELETE FROM mailbox").unwrap();
    conn.execute_batch(
        MIGRATIONS
            .iter()
            .find(|m| m.version == 45)
            .unwrap()
            .statements,
    )
    .unwrap();
    conn.execute_batch("INSERT INTO mailbox(run_id,position,settlement,value,payload_hash,source,arrived_generation) VALUES ('r',0,'fulfilled','1',zeroblob(32),'host',1)").unwrap();
    assert_eq!(conn.last_insert_rowid(), 101);
}
