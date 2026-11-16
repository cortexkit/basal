use super::*;

fn database(version: u32) -> Connection {
    let c = Connection::open_in_memory().unwrap();
    for m in crate::schema::MIGRATIONS
        .iter()
        .filter(|m| m.version <= version)
    {
        c.execute_batch(m.statements).unwrap();
    }
    c
}

#[test]
fn attribution_facts_are_permanent_validated_and_ordered() {
    let mut c = database(52);
    assert_eq!(
        read(&c, 0, 100).unwrap(),
        json!({"facts":[],"next":null,"through":0})
    );
    c.execute_batch("INSERT INTO flows(flow_id,owner,approved_version,created_at,first_approved_at) VALUES('one','agent_owner',1,10,20),('two','operator',1,10,30);").unwrap();
    let tx = c.transaction().unwrap();
    flow_approved(&tx, "one", true, 20).unwrap();
    flow_approved(&tx, "two", true, 30).unwrap();
    flow_session(&tx, "one", "basal:flow-one:run:0", 40).unwrap();
    tx.commit().unwrap();
    let page = read(&c, 0, 2).unwrap();
    assert_eq!(page["through"], 3);
    assert_eq!(page["next"], 2);
    assert_eq!(page["facts"][0]["fact_id"], "basal:flow:one:v1");
    assert_eq!(page["facts"][1]["alfonso"], Value::Null);
    let last = read(&c, 2, 2).unwrap();
    assert_eq!(
        last["facts"][0]["subject"]["session"],
        "basal:flow-one:run:0"
    );
    assert_eq!(last["next"], Value::Null);
    assert_eq!(read(&c, 99, 2).unwrap()["facts"], json!([]));
    for sql in [
        "UPDATE attribution_facts SET recorded_at=recorded_at",
        "DELETE FROM attribution_facts",
    ] {
        assert!(
            c.execute(sql, [])
                .unwrap_err()
                .to_string()
                .contains("permanent")
        );
    }
    c.execute("UPDATE flows SET flow_id='Invalid' WHERE flow_id='two'", [])
        .unwrap();
    let tx = c.transaction().unwrap();
    assert!(flow_session(&tx, "Invalid", "session", 50).is_err());
    let mut oversized = page["facts"][0].clone();
    oversized["task"] = "a".repeat(129).into();
    assert!(insert(&tx, oversized).is_err());
    tx.rollback().unwrap();
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM outbox WHERE kind='attribution.fact'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
}

#[test]
fn attribution_migration_52_backfills_v51_once_and_coexists_with_live_writes() {
    let mut c = database(51);
    c.execute_batch(r#"
INSERT INTO flows(flow_id,owner,approved_version,created_at,first_approved_at,package,removed) VALUES
 ('ordinary','agent_a',1,10,20,NULL,0),('removed','agent_b',2,10,30,NULL,1),
 ('legacy','agent_c',1,10,NULL,NULL,0),('package_i','agent_a',1,40,40,'package',0),
 ('template_i','agent_b',1,50,50,'template',0),('never',NULL,NULL,10,NULL,NULL,0);
INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,installed_at,approved_at,approval_ref)
 VALUES('legacy',1,zeroblob(32),'{}','','agent_c','approved',10,35,'card');
INSERT INTO run_snapshots(snapshot_id,flow_id,flow_version,code_hash,script,manifest)
 VALUES(1,'ordinary',1,zeroblob(32),'','{}'),(2,'removed',2,zeroblob(32),'','{}');
INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,snapshot_id,code_hash,state,admitted_at)
 VALUES('flow-run','ordinary','first',1,'{}',1,zeroblob(32),'pending',25),('old-run','removed','old',1,'{}',2,zeroblob(32),'pending',35);
INSERT INTO journal(run_id,position,kind_code,args_digest,idempotency_key,class,dispatch,issued_generation,args_bytes)
 VALUES('flow-run',0,1,zeroblob(32),'flow-key','mutation','sent',0,2),('old-run',1,1,zeroblob(32),'old-key','mutation','sent',0,2);
INSERT INTO broca_calls(send_id,run_id,position,snapshot) VALUES
 ('flow-key','flow-run',0,'{"route":{"flow_id":"ordinary","session":"basal:flow-ordinary:flow-run:0"}}'),
 ('old-key','old-run',1,'{"route":{"session":"basal:flow-removed:old-run:1"}}');
INSERT INTO classify_costs(kind,run_id,position,flow_id,created_at,reserved,args) VALUES
 ('flow','flow-run',0,'ordinary',25,0,'{"session":"basal:flow-ordinary:flow-run:0"}'),
 ('program','program',0,NULL,60,0,'{"session":"basal:codemode-program:0"}'),
 ('program','worker',0,NULL,70,0,'{"session":"basal:codemode-worker:0"}'),
 ('program','pruned',0,NULL,5,0,'{"session":"basal:codemode-pruned:0"}');
INSERT INTO codemode_runs(run_id,agent_id,program,catalog,catalog_digest,limits,scope,deadline_ms,status,admitted_at,bind_identity)
 VALUES('program','agent_a','','[]',printf('%064d',0),'{}','{}',1000,'running',60,'{"harness":"claude","session":"parent"}'),
 ('worker','','','[]',printf('%064d',0),'{}','{}',1000,'running',70,NULL);
UPDATE codemode_runs SET worker_scope='worker-scope',on_behalf_of_agent='agent_head' WHERE run_id='worker';
"#).unwrap();
    let migration = crate::schema::MIGRATIONS
        .iter()
        .find(|m| m.version == 52)
        .unwrap()
        .statements;
    c.execute_batch(migration).unwrap();
    let page = read(&c, 0, 500).unwrap();
    assert_eq!(page["facts"].as_array().unwrap().len(), 9);
    let get = |id: &str| {
        page["facts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["fact_id"] == id)
            .unwrap()
    };
    assert_eq!(get("basal:flow:legacy:v1")["effective_from_ms"], 35);
    assert_eq!(
        get("basal:flow:template_i:created")["task"],
        "basal.template"
    );
    assert_eq!(
        get("basal:session:basal:flow-removed:old-run:1")["effective_from_ms"],
        30
    );
    assert_eq!(
        get("basal:session:basal:codemode-worker:0")["alfonso"],
        "agent_head"
    );
    assert_eq!(
        get("basal:session:basal:codemode-program:0")["parent"],
        Value::Null
    );
    assert_eq!(
        get("basal:session:basal:codemode-program:0")["subject"]["runner"],
        "broca"
    );
    let excluded: i64 = c.query_row("SELECT COUNT(DISTINCT json_extract(c.args,'$.session')) FROM classify_costs c LEFT JOIN codemode_runs r ON r.run_id=c.run_id WHERE c.kind='program' AND r.run_id IS NULL AND json_valid(c.args) AND json_type(c.args,'$.session')='text'",[],|r|r.get(0)).unwrap();
    assert_eq!(excluded, 1);
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM flows WHERE owner IS NOT NULL AND first_approved_at IS NULL",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    // Retry just the set-based backfill, not the one-time DDL.
    let backfill = migration
        .split("WITH\nstamp")
        .nth(1)
        .unwrap()
        .split("DROP INDEX")
        .next()
        .unwrap();
    c.execute_batch(&format!("WITH\nstamp{backfill}")).unwrap();
    let tx = c.transaction().unwrap();
    flow_approved(&tx, "ordinary", false, 100).unwrap();
    flow_session(&tx, "ordinary", "basal:flow-ordinary:flow-run:0", 100).unwrap();
    program_session(&tx, "program", "basal:codemode-program:0", 100).unwrap();
    tx.commit().unwrap();
    assert_eq!(read(&c, 0, 500).unwrap(), page);
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM outbox WHERE kind='attribution.fact'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        9
    );
    assert_eq!(
        c.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        c.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
    println!(
        "backfill: flows=5 flow_sessions=2 program_sessions=2 outbox=9 missing_first_approval=1 excluded_orphan_program_sessions={excluded}"
    );
}
