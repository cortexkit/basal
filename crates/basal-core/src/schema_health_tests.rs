use super::*;
use rusqlite::{Connection, params, types::Value};

fn snapshot(conn: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
        .unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

#[test]
fn additive_health_indexes_preserve_populated_tables_in_every_state() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for m in MIGRATIONS.iter().filter(|m| m.version <= 10) {
        conn.execute_batch(m.statements).unwrap();
    }
    conn.execute_batch(r#"
INSERT INTO meta VALUES ('store_id','existing');
INSERT INTO flows(flow_id,owner,created_at) VALUES ('f','operator',1);
INSERT INTO flows(flow_id,owner,state,disabled_by,created_at,package,removed) VALUES ('instance','ALF','disabled','runtime',1,'p',1);
INSERT INTO package_versions VALUES ('p',1,'return 1','{}',zeroblob(32),1);
INSERT INTO instance_generations VALUES ('p','ALF',1,'ensure',1,'{}');
INSERT INTO instance_generations VALUES ('p','BASAL',2,'remove',NULL,'{}');
INSERT INTO instance_revocations VALUES ('instance',1,'revoked',1);
INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at) VALUES ('r','f','t',1,'{}','return 1','{}',zeroblob(32),'pending',1);
INSERT INTO trigger_inbox VALUES ('f','t',1,'r',1);
INSERT INTO tombstones VALUES ('trigger','old','old-run',1);
INSERT INTO journal(run_id,position,kind_code,args,args_digest,idempotency_key,class,dispatch,issued_generation) VALUES ('r',0,1,'{}',zeroblob(32),'k','query','sent',1);
INSERT INTO mailbox(run_id,position,settlement,value,payload_hash,source,arrived_generation) VALUES ('r',0,'fulfilled','1',zeroblob(32),'host',1);
INSERT INTO quarantine(run_id,position,settlement,value,payload_hash,reason,at) VALUES ('r',0,'fulfilled','2',zeroblob(32),'contradicts_outcome',1);
INSERT INTO activations VALUES ('r',1,'parent',1);
INSERT INTO audit(at,actor,action,detail) VALUES (1,'operator','test','{}');
INSERT INTO schedules VALUES ('f',1,'{}','active',1,2,NULL,1);
INSERT INTO schedules VALUES ('instance',1,'{}','disabled',1,2,NULL,1);
INSERT INTO schedule_fires(flow_id,trigger_id,version,due_ms,payload,planned_at) VALUES ('f','due',1,2,'{}',1);
INSERT INTO schedule_dropped(flow_id,trigger_id,due_ms,payload,reason,at) VALUES ('f','dropped',2,'{}','disabled',1);
INSERT INTO broca_calls VALUES ('k','r',0,'{"acknowledged":false}');
INSERT INTO call_audit VALUES ('r',0,'f','echo',zeroblob(32),'fulfilled',1);
INSERT INTO kv VALUES ('f','key','1',1,1);
INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,reserved_at) VALUES ('k','f','r',0,60000,0,10,'reserved',1);
INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,reserved_at,settled_at) VALUES ('settled','f','r',1,60000,0,10,'settled',1,2);
INSERT INTO token_windows(flow_id,window_ms,window_start,reserved) VALUES ('f',60000,0,10);
INSERT INTO rate_windows(flow_id,window_ms,window_start,runs) VALUES ('f',60000,0,1);
INSERT INTO outbox(at,kind,flow_id,recipient,body) VALUES (1,'flow.disabled','f','operator','{}');
"#).unwrap();
    for state in [
        "pending",
        "running",
        "suspended",
        "succeeded",
        "failed",
        "needs_reconcile",
        "engine_mismatch",
        "cancelled",
    ] {
        conn.execute("INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,owner,admitted_at) VALUES (?1,'f',?1,1,'null','return 1','{}',zeroblob(32),?1,?2,1)", params![state, (state=="running").then_some("parent")]).unwrap();
    }
    for (p, state) in [
        "none",
        "sent",
        "accepted",
        "unknown",
        "not_applied",
        "deferred",
    ]
    .into_iter()
    .enumerate()
    {
        conn.execute("INSERT INTO journal(run_id,position,kind_code,args,args_digest,idempotency_key,class,dispatch,issued_generation,unknown_reason,refusal,retry_not_before,refusal_detail) VALUES ('r',?1,1,'{}',zeroblob(32),?2,'query',?3,1,?4,?5,?6,?7)",params![p as i64+1,format!("dispatch:{state}"),state,(state=="unknown").then_some("connection_lost"),(state=="deferred").then_some("scope_ended"),(state=="deferred").then_some(1),(state=="deferred").then_some(r#"{"reason":"scope_ended","provider":"mock","action":"send"}"#)]).unwrap();
    }
    for (version, state) in ["validated", "approved", "superseded", "validated"]
        .into_iter()
        .enumerate()
    {
        conn.execute("INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,approval_ref,installed_at) VALUES ('f',?1,zeroblob(32),'{}','return 1','operator',?2,?3,1)",params![version as i64+1,state,(state!="validated").then_some("approval")]).unwrap();
    }
    for (version, state) in ["pending", "approved", "rejected", "stale"]
        .into_iter()
        .enumerate()
    {
        conn.execute("INSERT INTO install_cards(card_id,flow_id,version,code_hash,author,card,state,decided_by,created_at) VALUES (?1,'f',?2,zeroblob(32),'operator','{}',?1,?3,1)",params![state,version as i64+1,(state!="pending").then_some("operator")]).unwrap();
    }
    for (instance, state) in ["open", "applied", "declined", "expired", "stale"]
        .into_iter()
        .enumerate()
    {
        conn.execute("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at,answered_at) VALUES (?1,'reenable','f',1,?2,'{}',?1,1,?3)",params![state,instance as i64+1,(state!="open").then_some(2)]).unwrap();
    }
    let tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    let before: Vec<_> = tables
        .iter()
        .map(|table| {
            let rows = snapshot(&conn, table);
            assert!(!rows.is_empty(), "missing fixture for {table}");
            (table, rows)
        })
        .collect();
    let migration = MIGRATIONS
        .iter()
        .find(|m| m.version == 11)
        .expect("health query indexes migration");
    conn.execute_batch(migration.statements).unwrap();
    for (table, rows) in before {
        assert_eq!(snapshot(&conn, table), rows, "{table}");
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
    for name in [
        "runs_flow_finished",
        "runs_flow_state_admission",
        "journal_deferred",
        "decision_cards_kind_flow_instance",
        "decision_cards_open",
        "decision_cards_subject",
        "decision_cards_elicitation",
        "broca_calls_pending",
    ] {
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                [name],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "{name}"
        );
    }
    for (query, index) in [
        (
            "SELECT run_id FROM runs WHERE flow_id='f' AND state IN ('succeeded', 'failed', 'engine_mismatch', 'cancelled') ORDER BY admit_seq DESC, run_id DESC",
            "runs_flow_finished",
        ),
        (
            "SELECT position FROM journal WHERE dispatch = 'deferred' AND run_id='r' ORDER BY position",
            "journal_deferred",
        ),
        (
            "SELECT seq FROM decision_cards WHERE state = 'open' AND (raised_revision IS NULL OR raised_revision < revision) ORDER BY seq",
            "decision_cards_open",
        ),
        (
            "SELECT send_id FROM broca_calls WHERE json_extract(snapshot, '$.acknowledged') = 0 AND COALESCE(json_extract(snapshot, '$.deferred'), 0) = 0 ORDER BY send_id",
            "broca_calls_pending",
        ),
    ] {
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map([], |r| r.get(3))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().any(|step| step.contains(index)),
            "{index}: {plan:?}"
        );
    }
}
