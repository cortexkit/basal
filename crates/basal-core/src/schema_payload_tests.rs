use super::MIGRATIONS;
use crate::{journal, model, runs};
use basal_proto::{ArgsDigest, JsonText, Settlement};
use rusqlite::{Connection, params};

#[test]
fn migration_47_preserves_populated_history_and_in_progress_replay() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for m in MIGRATIONS.iter().filter(|m| m.version < 47) {
        conn.execute_batch(m.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO flows(flow_id,created_at) VALUES ('f',1);
        INSERT INTO runs(run_id,flow_id,flow_version,trigger_id,attempt,trigger,script,manifest,code_hash,state,owner,generation,admitted_at,ended_at)
        VALUES ('active','f',1,'t',1,'{}','return 1','{}',zeroblob(32),'running','owner',1,1,NULL),
               ('finished','f',1,'t2',1,'{}','return 1','{}',zeroblob(32),'succeeded',NULL,0,1,2),
               ('other','f',2,'t3',1,'{}','return 2','{}',zeroblob(32),'pending',NULL,0,1,NULL);").unwrap();
    let args = JsonText::new("{\"input\":\"é\"}").unwrap();
    let value = JsonText::new("{\"reply\":123}").unwrap();
    let digest = ArgsDigest::of(&args);
    let hash = crate::ids::payload_hash(Settlement::Fulfilled, &value);
    conn.execute("INSERT INTO journal(run_id,position,kind_code,args,args_digest,idempotency_key,class,dispatch,settlement,value,payload_hash,delivery_order,issued_generation,request)
        VALUES ('active',0,1,?1,?2,'settled','sync','none','fulfilled',?3,?4,0,1,'{\"prepared\":true}')",
        params![args.as_str(),digest.0.as_slice(),value.as_str(),hash.as_slice()]).unwrap();
    conn.execute("INSERT INTO journal(run_id,position,kind_code,args,args_digest,idempotency_key,class,dispatch,issued_generation,request)
        VALUES ('active',1,21,?1,?2,'pending','query','sent',1,'{\"frozen\":true}')",params![args.as_str(),digest.0.as_slice()]).unwrap();
    conn.execute("INSERT INTO mailbox(seq,run_id,position,settlement,value,payload_hash,source,arrived_generation)
        VALUES (500,'active',1,'fulfilled',?1,?2,'host',1)",params![value.as_str(),hash.as_slice()]).unwrap();
    conn.execute_batch("INSERT INTO broca_calls VALUES ('pending','active',1,'{\"acknowledged\":false}');
        INSERT INTO broca_waits VALUES ('pending','broca-run','session',123);
        INSERT INTO broca_wakes VALUES ('pending','event','ended','digest',NULL,NULL,NULL,1);
        INSERT INTO event_body_journal(run_id,body,digest,last_provider_refusal) VALUES ('active',X'000102','body-hash','scope_ended');").unwrap();
    let before_run = conn
        .query_row(
            &format!(
                "SELECT {} FROM runs WHERE run_id='active'",
                model::RUN_COLUMNS
            ),
            [],
            model::Run::from_row,
        )
        .unwrap()
        .unwrap();
    let old_columns = model::CALL_COLUMNS.replace(", payload_pruned", ", 0 AS payload_pruned");
    let before_calls = conn
        .prepare(&format!(
            "SELECT {old_columns} FROM journal ORDER BY position"
        ))
        .unwrap()
        .query_map([], model::CallRow::from_row)
        .unwrap()
        .map(|r| r.unwrap().unwrap())
        .collect::<Vec<_>>();
    let indexes = |c: &Connection| {
        c.prepare("SELECT name FROM sqlite_schema WHERE type='index' AND tbl_name IN ('runs','journal','mailbox','broca_calls') ORDER BY name").unwrap()
            .query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
    };
    let before_indexes = indexes(&conn);
    let tx = conn.transaction().unwrap();
    tx.execute_batch(
        MIGRATIONS
            .iter()
            .find(|m| m.version == 47)
            .unwrap()
            .statements,
    )
    .unwrap();
    assert_eq!(
        tx.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(
        tx.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
    tx.commit().unwrap();
    assert_eq!(runs::load(&conn, "active").unwrap(), before_run);
    assert_eq!(journal::rows(&conn, "active").unwrap(), before_calls);
    assert_eq!(
        conn.query_row(
            "SELECT payload_hash FROM journal WHERE run_id='active' AND position=0",
            [],
            |r| r.get::<_, Vec<u8>>(0)
        )
        .unwrap(),
        hash
    );
    assert_eq!(
        journal::prefix(&conn, "active").unwrap(),
        before_calls
            .iter()
            .map(model::CallRow::recorded)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        journal::run_size(&conn, "active").unwrap(),
        (
            2,
            (2 * args.len()
                + 2 * value.len()
                + "{\"prepared\":true}".len()
                + "{\"frozen\":true}".len()) as u64
        )
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM journal_payloads", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM run_snapshots", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    for name in before_indexes {
        assert!(indexes(&conn).contains(&name), "lost {name}");
    }
    assert_eq!(
        conn.query_row(
            "SELECT body FROM event_body_journal WHERE run_id='active'",
            [],
            |r| r.get::<_, Vec<u8>>(0)
        )
        .unwrap(),
        vec![0, 1, 2]
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM broca_wakes", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let lease = runs::Lease {
        run_id: "active".into(),
        owner: "owner".into(),
        generation: 1,
    };
    let tx = conn.transaction().unwrap();
    assert_eq!(journal::authorize_resend(&tx, &lease, 1).unwrap(), 1);
    assert_eq!(
        journal::row(&tx, "active", 1)
            .unwrap()
            .unwrap()
            .dispatch_args()
            .as_str(),
        "{\"frozen\":true}"
    );
    let (released, _) = journal::release_next(&tx, &lease, &[1]).unwrap();
    assert_eq!(released.unwrap().value, value);
    assert_eq!(
        journal::prefix(&tx, "active").unwrap()[1]
            .outcome
            .as_ref()
            .unwrap()
            .delivery_order,
        1
    );
    tx.execute(
        "DELETE FROM journal WHERE run_id='active' AND position=1",
        [],
    )
    .unwrap();
    assert_eq!(
        tx.query_row("SELECT count(*) FROM broca_waits", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        tx.query_row("SELECT count(*) FROM broca_wakes", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        tx.query_row("SELECT count(*) FROM journal_payloads", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        tx.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn migration_47_preserves_deleted_mailbox_sequence_and_refuses_conflicting_code() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for m in MIGRATIONS.iter().filter(|m| m.version < 47) {
        conn.execute_batch(m.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at) VALUES ('one','f','t',1,'{}','one','{}',zeroblob(32),'pending',1),('two','f','u',1,'{}','two','{}',zeroblob(32),'pending',1); INSERT INTO journal(run_id,position,kind_code,args,args_digest,idempotency_key,class,dispatch,issued_generation) VALUES ('one',0,1,'null',zeroblob(32),'key','query','sent',1); INSERT INTO mailbox(seq,run_id,position,settlement,value,payload_hash,source,arrived_generation) VALUES (900,'one',0,'fulfilled','null',zeroblob(32),'host',1); DELETE FROM mailbox;").unwrap();
    let sql = MIGRATIONS
        .iter()
        .find(|m| m.version == 47)
        .unwrap()
        .statements;
    {
        let tx = conn.transaction().unwrap();
        assert!(
            tx.execute_batch(sql).is_err(),
            "conflicting identity must not discard code"
        );
    }
    assert_eq!(
        conn.query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    conn.execute("UPDATE runs SET script='one' WHERE run_id='two'", [])
        .unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute_batch(sql).unwrap();
    assert_eq!(
        tx.query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='mailbox'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        900
    );
    assert_eq!(
        tx.query_row(
            "SELECT payload_hash FROM journal WHERE run_id='one'",
            [],
            |r| r.get::<_, Option<Vec<u8>>>(0)
        )
        .unwrap(),
        None
    );
    tx.commit().unwrap();
}
