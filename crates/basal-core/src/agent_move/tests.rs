use std::collections::BTreeMap;

use rusqlite::Connection;
use serde_json::{Value, json};

use super::*;
use crate::schema::MIGRATIONS;

pub(super) const AGENT: &str = "agent_0123456789abcdef";
const CUT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BACK: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A store at `version`, with the run-log tables the one-time cutover adds
/// to every live store.
fn migrated_to(version: u32) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for migration in MIGRATIONS.iter().filter(|m| m.version <= version) {
        conn.execute_batch(migration.statements).unwrap();
    }
    let tx = conn.transaction().unwrap();
    tx.execute_batch(crate::run_store_sql::DDL).unwrap();
    crate::run_cutover::initialize_broca_fixture(&tx).unwrap();
    tx.commit().unwrap();
    conn
}

pub(super) fn migrated() -> Connection {
    migrated_to(u32::MAX)
}

fn hash(script: &str, manifest: &str) -> Vec<u8> {
    crate::ids::code_hash(script, manifest).to_vec()
}

/// Two flows of the agent (one enabled with a schedule, `kv`, history and a
/// queued event; one disabled by the operator), a template instance it
/// owns, and another agent's flow.
pub(super) fn populate(conn: &Connection) {
    let insert_install = |flow: &str, version: i64, state: &str, author: &str| {
        let script = format!("// {flow} v{version}\n");
        let manifest = format!(r#"{{"id":"{flow}","version":{version}}}"#);
        let approval = (state != "validated").then_some("core:card");
        conn.execute(
            "INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,approval_ref,installed_at,approved_at,approval_kind) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,10,?9,?10)",
            rusqlite::params![flow, version, hash(&script, &manifest), manifest, script, author, state, approval, approval.map(|_| 20), approval.map(|_| "card")],
        )
        .unwrap();
    };
    conn.execute_batch(&format!(
        "INSERT INTO flows(flow_id,owner,state,approved_version,created_at,first_approved_at) VALUES ('f-alpha','{AGENT}','enabled',1,5,20);
         INSERT INTO flows(flow_id,owner,state,approved_version,disabled_by,disabled_reason,disabled_at,created_at) VALUES ('f-beta','{AGENT}','disabled',1,'operator:operator','paused',30,6);
         INSERT INTO flows(flow_id,owner,state,approved_version,created_at) VALUES ('f-other','agent_other','enabled',1,7);
         INSERT INTO flows(flow_id,owner,state,approved_version,created_at,package,first_approved_at) VALUES ('remind-at_0011223344556677','{AGENT}','enabled',1,8,'remind-at',8);"
    ))
    .unwrap();
    insert_install("f-alpha", 1, "approved", AGENT);
    insert_install("f-alpha", 2, "validated", AGENT);
    insert_install("f-beta", 1, "approved", AGENT);
    insert_install("f-other", 1, "approved", "agent_other");
    let template_script = "// remind\n";
    let template_manifest = r#"{"id":"remind-at","version":1}"#;
    conn.execute(
        "INSERT INTO package_versions(package,version,script,manifest,code_hash,registered_at) VALUES ('remind-at',1,?1,?2,?3,1)",
        rusqlite::params![template_script, template_manifest, hash(template_script, template_manifest)],
    )
    .unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO install_tool_policies(flow_id,version,module,op,policy) VALUES ('f-alpha',1,'plexus','search','ordinary');
         INSERT INTO schedules(flow_id,version,spec,state,anchor_ms,next_due_ms,last_fired_ms,updated_at) VALUES ('f-alpha',1,'{{\"interval\":\"1m\"}}','active',0,120000,60000,60000);
         INSERT INTO template_instances(flow_id,params,grant_guards,expires_at,incarnation,seq) VALUES ('remind-at_0011223344556677','{{\"at\":\"x\"}}','[]',999999,1,2);
         INSERT INTO instance_generations(package,agent_id,generation,operation,version,reply) VALUES ('remind-at','{AGENT}',3,'ensure',1,'{{}}');
         INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES ('f-alpha','count','4',1,4),('f-other','count','9',1,1);
         INSERT INTO event_receipts(flow_id,subject,event_key,notice,state,received_at) VALUES ('f-alpha','m.ev','k1','{{}}','backlog',50);"
    ))
    .unwrap();
    let script = "// f-alpha v1\n";
    let manifest = r#"{"id":"f-alpha","version":1}"#;
    conn.execute(
        "INSERT INTO run_snapshots(snapshot_id,flow_id,flow_version,code_hash,script,manifest) VALUES (41,'f-alpha',1,?1,?2,?3)",
        rusqlite::params![hash(script, manifest), script, manifest],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO run_history(run_id,flow_id,flow_version,snapshot_id,trigger_id,trigger_kind,trigger,admitted_at,admit_seq,ended_at,state,result_size,call_counts,writes_sent,tokens_charged,cost_microusd,broken,activation_count,script_started,phase,deadline_exempt,history_lost,commit_seq,error_kind)
           VALUES ('run-src-1','f-alpha',1,41,'t1','schedule','{}',60,1,61,'failed',0,'{}',0,0,0,0,1,1,'done',0,0,1,'script_error');
         INSERT INTO run_calls(run_id,position,kind,class,module,op,args_digest,outcome,issued_at,detail)
           VALUES ('run-src-1',0,0,'query','plexus','search',zeroblob(32),'rejected',60,'{}');",
    )
    .unwrap();
}

fn tx(conn: &mut Connection) -> rusqlite::Transaction<'_> {
    conn.transaction().unwrap()
}

fn rows(conn: &Connection, sql: &str) -> Vec<String> {
    conn.prepare(sql)
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn migration_53_keeps_a_populated_v52_store() {
    let conn = migrated_to(52);
    populate(&conn);
    let tables = [
        "flows",
        "installs",
        "schedules",
        "kv",
        "template_instances",
        "run_history",
        "run_calls",
        "event_receipts",
    ];
    let dump = |conn: &Connection| -> Vec<Vec<String>> {
        tables
            .iter()
            .map(|t| {
                let mut stmt = conn.prepare(&format!("SELECT * FROM {t}")).unwrap();
                let width = stmt.column_count();
                let mut rows: Vec<String> = stmt
                    .query_map([], |r| {
                        Ok((0..width)
                            .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                            .collect::<Vec<_>>()
                            .join("|"))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                rows.sort();
                rows
            })
            .collect()
    };
    let before = dump(&conn);
    conn.execute_batch(
        MIGRATIONS
            .iter()
            .find(|m| m.version == 53)
            .unwrap()
            .statements,
    )
    .unwrap();
    assert_eq!(dump(&conn), before);
    assert_eq!(
        rows(
            &conn,
            "SELECT flow_id||'='||agent_id FROM agent_move_flow_agents ORDER BY flow_id"
        ),
        [
            format!("f-alpha={AGENT}"),
            format!("f-beta={AGENT}"),
            "f-other=agent_other".to_owned(),
            format!("remind-at_0011223344556677={AGENT}"),
        ]
    );
    assert!(rows(&conn, "SELECT flow_id FROM agent_move_held_flows").is_empty());
    assert_eq!(rows(&conn, "PRAGMA integrity_check"), ["ok"]);
    assert!(rows(&conn, "PRAGMA foreign_key_check").is_empty());
}

#[test]
fn an_unapproved_flow_belongs_to_its_single_author() {
    let conn = migrated();
    conn.execute_batch(&format!(
        "INSERT INTO flows(flow_id,created_at) VALUES ('draft',1),('shared',1);
         INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,installed_at)
           VALUES ('draft',1,zeroblob(32),'m','s','{AGENT}','validated',1),
                  ('shared',1,zeroblob(32),'m','s','{AGENT}','validated',1),
                  ('shared',2,zeroblob(32),'m','s','agent_other','validated',1);"
    ))
    .unwrap();
    assert_eq!(
        rows(&conn, "SELECT flow_id FROM agent_move_flow_agents"),
        ["draft"]
    );
}

#[test]
fn busy_reports_live_runs_and_open_decisions() {
    let mut conn = migrated();
    populate(&conn);
    assert_eq!(busy(&conn, AGENT).unwrap(), None);
    conn.execute_batch(
        "INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at) VALUES ('d','reenable','f-beta',1,0,'{}','open',1)",
    )
    .unwrap();
    assert_eq!(busy(&conn, AGENT).unwrap().unwrap().code, "reconcile_open");
    assert_eq!(busy(&conn, "agent_other").unwrap(), None);
    let t = tx(&mut conn);
    assert_eq!(
        freeze(&t, AGENT, CUT, 1).unwrap().unwrap_err().code,
        "reconcile_open"
    );
    assert_eq!(hold(&t, AGENT).unwrap(), None);
}

#[test]
fn a_freeze_holds_every_flow_of_the_agent_and_abort_lifts_it() {
    let mut conn = migrated();
    populate(&conn);
    let t = tx(&mut conn);
    freeze(&t, AGENT, CUT, 1).unwrap().unwrap();
    assert!(flow_held(&t, "f-alpha").unwrap());
    assert!(flow_held(&t, "remind-at_0011223344556677").unwrap());
    assert!(!flow_held(&t, "f-other").unwrap());
    // A frozen schedule isn't due and doesn't wake the engine.
    let due = crate::schedule::tick::due_flows(
        &t,
        jiff::Timestamp::from_millisecond(10_000_000).unwrap(),
    )
    .unwrap();
    assert!(due.is_empty(), "{due:?}");
    lift(&t, AGENT, "cccccccccccccccccccccccccccccccc").unwrap();
    assert!(
        flow_held(&t, "f-alpha").unwrap(),
        "another cut's abort lifts nothing"
    );
    lift(&t, AGENT, CUT).unwrap();
    assert!(!flow_held(&t, "f-alpha").unwrap());
    let due = crate::schedule::tick::due_flows(
        &t,
        jiff::Timestamp::from_millisecond(10_000_000).unwrap(),
    )
    .unwrap();
    assert_eq!(due, ["f-alpha"]);
}

fn staged_from(conn: &Connection, cut: &str) -> Staged {
    let bytes = encode(conn, AGENT, cut).unwrap().unwrap();
    stream::decode(&bytes).unwrap()
}

fn receiver() -> Connection {
    let conn = migrated();
    let template_script = "// remind\n";
    let template_manifest = r#"{"id":"remind-at","version":1}"#;
    conn.execute(
        "INSERT INTO package_versions(package,version,script,manifest,code_hash,registered_at) VALUES ('remind-at',1,?1,?2,?3,1)",
        rusqlite::params![template_script, template_manifest, hash(template_script, template_manifest)],
    )
    .unwrap();
    conn
}

#[test]
fn a_two_flow_agent_round_trips_with_kv_an_instance_and_history() {
    let source = migrated();
    populate(&source);
    let staged = staged_from(&source, CUT);
    assert_eq!(staged.agent, AGENT);
    assert_eq!(staged.cut_id, CUT);
    assert_eq!(
        staged.flow_ids(),
        ["f-alpha", "f-beta", "remind-at_0011223344556677"]
    );
    assert_eq!(
        staged.dropped(),
        Some(BTreeMap::from([("queued_event".to_owned(), 1)]))
    );
    assert!(staged.flow_ids().iter().all(|f| f != "f-other"));

    let mut receiver = receiver();
    let t = tx(&mut receiver);
    let written = activate(&t, AGENT, CUT, &staged, 1000).unwrap().unwrap();
    t.commit().unwrap();
    assert_eq!(
        written,
        fingerprint(&source, AGENT).unwrap(),
        "the receiver holds what the cut held"
    );
    assert_eq!(
        rows(
            &receiver,
            "SELECT flow_id||':'||state||':'||COALESCE(disabled_by,'-')||':'||COALESCE(approved_version,'-') FROM flows ORDER BY flow_id"
        ),
        [
            "f-alpha:disabled:move:-",
            "f-beta:disabled:move:-",
            "remind-at_0011223344556677:disabled:move:1",
        ]
    );
    // Approval doesn't move: the approved version waits as validated.
    assert_eq!(
        rows(
            &receiver,
            "SELECT flow_id||':'||version||':'||state||':'||COALESCE(approval_ref,'-') FROM installs ORDER BY flow_id,version"
        ),
        [
            "f-alpha:1:validated:-",
            "f-alpha:2:validated:-",
            "f-beta:1:validated:-"
        ]
    );
    assert_eq!(
        rows(&receiver, "SELECT key||'='||value FROM kv"),
        ["count=4"]
    );
    assert_eq!(
        rows(
            &receiver,
            "SELECT module||'.'||op||'='||policy FROM install_tool_policies"
        ),
        ["plexus.search=ordinary"]
    );
    assert_eq!(
        rows(&receiver, "SELECT state||':'||next_due_ms FROM schedules"),
        ["disabled:120000"]
    );
    assert_eq!(
        rows(&receiver, "SELECT params||':'||seq FROM template_instances"),
        [r#"{"at":"x"}:2"#]
    );
    assert_eq!(
        rows(
            &receiver,
            "SELECT package||':'||generation FROM instance_generations"
        ),
        ["remind-at:3"]
    );
    assert_eq!(
        rows(
            &receiver,
            "SELECT run_id||':'||ended_at||':'||error_kind FROM run_history"
        ),
        ["run-src-1:61:script_error"]
    );
    assert_eq!(
        rows(
            &receiver,
            "SELECT run_id||':'||module||'.'||op FROM run_calls"
        ),
        ["run-src-1:plexus.search"]
    );
    assert_eq!(
        rows(
            &receiver,
            "SELECT flow_id||':'||version FROM agent_move_pending ORDER BY flow_id"
        ),
        ["f-alpha:1", "f-beta:1", "remind-at_0011223344556677:1"]
    );
    assert!(
        rows(&receiver, "SELECT * FROM event_receipts").is_empty(),
        "queued events stay behind"
    );
    assert_eq!(
        rows(&receiver, "PRAGMA foreign_key_check"),
        Vec::<String>::new()
    );

    // Approval here puts each flow back in its source's state; the schedule
    // keeps its moved due time, so `missed` covers the move.
    let t = tx(&mut receiver);
    assert!(lift_move(&t, "f-alpha", 2000).unwrap());
    assert!(lift_move(&t, "f-beta", 2000).unwrap());
    assert!(!lift_move(&t, "f-beta", 2000).unwrap());
    t.commit().unwrap();
    assert_eq!(
        rows(
            &receiver,
            "SELECT flow_id||':'||state||':'||COALESCE(disabled_by,'-') FROM flows WHERE package IS NULL ORDER BY flow_id"
        ),
        ["f-alpha:enabled:-", "f-beta:disabled:operator:operator"]
    );
    assert_eq!(
        rows(&receiver, "SELECT state||':'||next_due_ms FROM schedules"),
        ["active:120000"]
    );
}

#[test]
fn release_tombstones_the_agent_and_a_move_back_replaces_it() {
    let mut source = migrated();
    populate(&source);
    source.execute_batch(
        "INSERT INTO install_cards(card_id,flow_id,version,code_hash,author,card,state,created_at) SELECT 'c1','f-alpha',2,code_hash,author,'{}','pending',1 FROM installs WHERE flow_id='f-alpha' AND version=2",
    )
    .unwrap();
    let t = tx(&mut source);
    freeze(&t, AGENT, CUT, 1).unwrap().unwrap();
    let staged = staged_from(&t, CUT);
    let released = release(&t, AGENT, CUT, "holder-b", 100).unwrap();
    t.commit().unwrap();
    assert_eq!(
        released,
        ["f-alpha", "f-beta", "remind-at_0011223344556677"]
    );
    assert_eq!(
        hold(&source, AGENT).unwrap(),
        Some(Hold::Moved {
            cut_id: CUT.into(),
            holder: "holder-b".into()
        })
    );
    assert_eq!(
        rows(
            &source,
            "SELECT flow_id||':'||removed_by||':'||state FROM flows WHERE owner=(SELECT agent_id FROM agent_move_holds) ORDER BY flow_id"
        ),
        [
            "f-alpha:moved:holder-b:disabled",
            "f-beta:moved:holder-b:disabled",
            "remind-at_0011223344556677:moved:holder-b:disabled",
        ]
    );
    assert_eq!(
        rows(&source, "SELECT state FROM event_receipts"),
        ["refused"]
    );
    assert_eq!(rows(&source, "SELECT state FROM install_cards"), ["stale"]);
    assert_eq!(rows(&source, "SELECT state FROM schedules"), ["disabled"]);
    assert_eq!(
        local(&source, AGENT).unwrap(),
        Local::AtCut,
        "a tombstone holds nothing live"
    );

    // The agent runs on the other computer, gains a kv write, and moves back.
    let mut other = receiver();
    let t = tx(&mut other);
    activate(&t, AGENT, CUT, &staged, 200).unwrap().unwrap();
    t.execute("UPDATE kv SET value='5',revision=5 WHERE key='count'", [])
        .unwrap();
    t.commit().unwrap();
    let back = staged_from(&other, BACK);
    let t = tx(&mut source);
    activate(&t, AGENT, BACK, &back, 300).unwrap().unwrap();
    t.commit().unwrap();
    assert_eq!(
        hold(&source, AGENT).unwrap(),
        None,
        "the tombstone is replaced"
    );
    assert_eq!(
        rows(
            &source,
            "SELECT flow_id||':'||value FROM kv ORDER BY flow_id"
        ),
        ["f-alpha:5", "f-other:9"]
    );
    assert_eq!(
        rows(
            &source,
            "SELECT flow_id||':'||COALESCE(removed_by,'-')||':'||disabled_by FROM flows WHERE flow_id IN ('f-alpha','f-beta') ORDER BY flow_id"
        ),
        ["f-alpha:-:move", "f-beta:-:move"]
    );
    assert_eq!(
        rows(&source, "SELECT CAST(COUNT(*) AS TEXT) FROM install_cards"),
        ["0"]
    );
    assert_eq!(
        rows(&source, "SELECT run_id FROM run_history"),
        ["run-src-1"]
    );
    assert_eq!(
        rows(&source, "SELECT CAST(COUNT(*) AS TEXT) FROM run_snapshots"),
        ["1"]
    );
    assert_eq!(
        rows(&source, "PRAGMA foreign_key_check"),
        Vec::<String>::new()
    );
}

#[test]
fn a_flow_owned_by_someone_else_here_is_a_collision() {
    let source = migrated();
    populate(&source);
    let staged = staged_from(&source, CUT);
    let mut receiver = receiver();
    receiver
        .execute_batch(
            "INSERT INTO flows(flow_id,owner,created_at) VALUES ('f-beta','agent_other',1)",
        )
        .unwrap();
    assert_eq!(
        collision(&receiver, AGENT, &staged).unwrap(),
        Some(Collision {
            flow_id: "f-beta".into(),
            owner: Some("agent_other".into())
        })
    );
    let t = tx(&mut receiver);
    let refused = activate(&t, AGENT, CUT, &staged, 1).unwrap().unwrap_err();
    t.commit().unwrap();
    assert!(matches!(refused, ActivateRefusal::Collision(_)));
    assert_eq!(
        rows(&receiver, "SELECT flow_id FROM flows"),
        ["f-beta"],
        "nothing written"
    );
}

#[test]
fn a_live_local_copy_with_new_data_is_a_fork() {
    let source = migrated();
    populate(&source);
    let staged = staged_from(&source, CUT);
    let mut receiver = receiver();
    let t = tx(&mut receiver);
    activate(&t, AGENT, CUT, &staged, 1).unwrap().unwrap();
    // The receiver records the imported cut's fingerprint, as the module does.
    save_cut(
        &t,
        &Cut {
            cut_id: CUT.into(),
            agent: Some(AGENT.into()),
            side: "receiver".into(),
            step: "activated".into(),
            refs: json!([]),
            session_ref: None,
            end_offset: None,
            digest: None,
            fingerprint: Some(fingerprint(&t, AGENT).unwrap()),
            dropped: None,
        },
        1,
    )
    .unwrap();
    t.commit().unwrap();
    assert_eq!(
        local(&receiver, AGENT).unwrap(),
        Local::AtCut,
        "matching the last cut"
    );
    receiver
        .execute_batch(
            "INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES ('f-alpha','new','1',1,1)",
        )
        .unwrap();
    assert_eq!(local(&receiver, AGENT).unwrap(), Local::Advanced);
    let before = rows(&receiver, "SELECT key FROM kv ORDER BY key");
    let t = tx(&mut receiver);
    assert_eq!(
        activate(&t, AGENT, BACK, &staged, 2).unwrap(),
        Err(ActivateRefusal::Advanced)
    );
    t.commit().unwrap();
    assert_eq!(rows(&receiver, "SELECT key FROM kv ORDER BY key"), before);
}

#[test]
fn a_refused_write_rolls_back_everything_it_wrote() {
    let source = migrated();
    populate(&source);
    let mut staged = staged_from(&source, CUT);
    // A history row whose snapshot the cut doesn't carry is refused after
    // the flows were already written.
    staged.records.retain(|r| r["kind"] != "snapshot");
    let mut receiver = receiver();
    let t = tx(&mut receiver);
    assert!(matches!(
        activate(&t, AGENT, CUT, &staged, 1).unwrap(),
        Err(ActivateRefusal::Rejected(_))
    ));
    t.commit().unwrap();
    assert_eq!(
        rows(&receiver, "SELECT CAST(COUNT(*) AS TEXT) FROM flows"),
        ["0"]
    );
    assert_eq!(
        rows(
            &receiver,
            "SELECT CAST(COUNT(*) AS TEXT) FROM agent_move_pending"
        ),
        ["0"]
    );
}

#[test]
fn cuts_and_replies_survive_as_rows() {
    let mut conn = migrated();
    let t = tx(&mut conn);
    let cut = Cut {
        cut_id: CUT.into(),
        agent: Some(AGENT.into()),
        side: "source".into(),
        step: "prepared".into(),
        refs: json!([{"ref_kind":"agent","ref":AGENT}]),
        session_ref: Some("0".repeat(32)),
        end_offset: Some(7),
        digest: Some("d".into()),
        fingerprint: None,
        dropped: Some(json!({"queued_event": 1})),
    };
    save_cut(&t, &cut, 1).unwrap();
    record_reply(&t, CUT, "prepare", &json!({"a":1}), &json!({"items":[]})).unwrap();
    record_reply(&t, CUT, "prepare", &json!({"a":2}), &json!({"other":true})).unwrap();
    t.commit().unwrap();
    assert_eq!(super::cut(&conn, CUT).unwrap(), Some(cut));
    assert_eq!(
        recorded_reply(&conn, CUT, "prepare").unwrap(),
        Some((json!({"a":1}), json!({"items":[]}))),
        "the first reply stands"
    );
    assert_eq!(
        recorded_reply(&conn, CUT, "export").unwrap(),
        None::<(Value, Value)>
    );
}

#[test]
fn a_second_cut_of_a_frozen_agent_is_busy() {
    let mut conn = migrated();
    populate(&conn);
    let t = tx(&mut conn);
    freeze(&t, AGENT, CUT, 1).unwrap().unwrap();
    freeze(&t, AGENT, CUT, 2).unwrap().unwrap();
    assert_eq!(
        freeze(&t, AGENT, BACK, 3).unwrap().unwrap_err().code,
        "move_in_progress"
    );
    assert_eq!(
        hold(&t, AGENT).unwrap(),
        Some(Hold::Frozen { cut_id: CUT.into() })
    );
}
