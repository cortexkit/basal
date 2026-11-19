use super::*;
use basal_core::codemode::store::{self, NewRun, Status, Terminal};

#[test]
fn new_late_result_commits_one_wake_per_custodian_and_never_duplicates_it() {
    let root = std::env::temp_dir().join(format!("basal-late-wake-{}", random_id().unwrap()));
    let store = Store::open(root.join("store.db"), basal_core::Durability::throwaway()).unwrap();
    store.write(|tx| {
        store::insert_run(tx,&NewRun{run_id:"r",agent_id:"agent",program:"return 1;",catalog:"[]",catalog_digest:&"a".repeat(64),description:None,limits:"{}",scope:"{}",deadline_ms:1000,admitted_at:100},None)?;
        store::commit_terminal(tx,"r",&Terminal{status:Status::Completed,value:Some("1".into()),value_head:None,error:None,output:String::new(),warnings:"[]".into()},110)?;
        for module in ["broca","prefrontal-core"] {
            let principal=format!("reserved:{module}");
            let scope=serde_json::to_string(&cortexkit_role_tool_provider::scope::ScopeIdentity{owner:principal.clone(),scope_ref:"scope".into(),scope_epoch:1}).unwrap();
            tx.execute("INSERT INTO codemode_tool_calls(carrier,call_key,run_id,scope,custodian) VALUES(?1,'key','r',?2,?1)",rusqlite::params![principal,scope])?;
        }
        Ok(())
    }).unwrap();
    store.write(|tx| {
        tx.execute_batch("CREATE TRIGGER refuse_wake BEFORE INSERT ON outbox WHEN NEW.kind='codemode.late_result_ready' BEGIN SELECT RAISE(ABORT,'wake unavailable'); END;")?;
        Ok(())
    }).unwrap();
    assert!(publish_result(&store, "r", json!({"status":"completed","value":1})).is_err());
    assert_eq!(
        store
            .read(|c| Ok(c
                .query_row("SELECT COUNT(*) FROM codemode_late_results", [], |r| r
                    .get::<_, i64>(0))?))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .read(|c| Ok(c.query_row(
                "SELECT SUM(published) FROM codemode_tool_calls",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .unwrap(),
        0
    );
    store
        .write(|tx| {
            tx.execute_batch("DROP TRIGGER refuse_wake")?;
            Ok(())
        })
        .unwrap();
    publish_result(&store, "r", json!({"status":"completed","value":1})).unwrap();
    // Re-enter the producer as after a duplicate completion/recovery pass.
    store
        .write(|tx| {
            tx.execute("UPDATE codemode_tool_calls SET published=0", [])?;
            Ok(())
        })
        .unwrap();
    publish_result(&store, "r", json!({"status":"completed","value":1})).unwrap();
    let rows=store.read(|c| {
        let mut q=c.prepare("SELECT l.event_id,l.custodian,o.recipient,o.body FROM codemode_late_results l JOIN outbox o ON o.kind='codemode.late_result_ready' AND json_extract(o.body,'$.event_id')=l.event_id ORDER BY l.seq")?;
        Ok(q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }).unwrap();
    assert_eq!(rows.len(), 2);
    for (event_id, custodian, module, body) in rows {
        assert_eq!(custodian, format!("reserved:{module}"));
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"call_key":"key","outcome":"ok","event_id":event_id})
        );
    }
    assert_eq!(
        store
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))?))
            .unwrap(),
        2
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn late_result_wake_outcomes_match_the_settled_program_reply() {
    let root = std::env::temp_dir().join(format!("basal-late-outcomes-{}", random_id().unwrap()));
    let store = Store::open(root.join("store.db"), basal_core::Durability::throwaway()).unwrap();
    for (id, result, expected) in [
        ("ok", json!({"status":"completed","value":1}), "ok"),
        (
            "failed",
            json!({"status":"failed","error":{"code":"tool_denied","message":"refused"}}),
            "tool_denied",
        ),
        (
            "budget",
            json!({"status":"failed","error":{"code":"budget_exhausted:wall","message":"deadline"}}),
            "budget_exhausted",
        ),
        (
            "cut",
            json!({"status":"failed","error":{"code":"result_too_large"},"value_head":"partial return"}),
            "ok",
        ),
        ("unknown", json!({}), "unknown"),
    ] {
        store.write(|tx| {
            store::insert_run(tx,&NewRun{run_id:id,agent_id:"agent",program:"return 1;",catalog:"[]",catalog_digest:&"a".repeat(64),description:None,limits:"{}",scope:"{}",deadline_ms:1000,admitted_at:100},None)?;
            store::commit_terminal(tx,id,&Terminal{status:Status::Completed,value:Some("1".into()),value_head:None,error:None,output:String::new(),warnings:"[]".into()},110)?;
            let scope=serde_json::to_string(&cortexkit_role_tool_provider::scope::ScopeIdentity{owner:"reserved:reader".into(),scope_ref:"scope".into(),scope_epoch:1}).unwrap();
            tx.execute("INSERT INTO codemode_tool_calls(carrier,call_key,run_id,scope,custodian) VALUES('reserved:reader',?1,?1,?2,'reserved:reader')",rusqlite::params![id,scope])?;
            Ok(())
        }).unwrap();
        publish_result(&store, id, result).unwrap();
        let (entry, body): (String, String) = store.read(|c| {
            Ok(c.query_row("SELECT l.entry,o.body FROM codemode_late_results l JOIN outbox o ON json_extract(o.body,'$.event_id')=l.event_id WHERE json_extract(l.entry,'$.call_key')=?1", [id], |r| Ok((r.get(0)?,r.get(1)?)))?)
        }).unwrap();
        let entry: Value = serde_json::from_str(&entry).unwrap();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["call_key"], id);
        assert_eq!(body["outcome"], expected, "{id}");
        assert_eq!(body["event_id"], entry["event_id"]);
        if expected != "ok" && expected != "unknown" {
            assert_eq!(entry["result"]["errorCode"], expected);
        }
    }
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn restart_publishes_a_held_call_that_finished_while_basal_was_down() {
    let root = std::env::temp_dir().join(format!("basal-late-recovery-{}", random_id().unwrap()));
    let store =
        Arc::new(Store::open(root.join("store.db"), basal_core::Durability::throwaway()).unwrap());
    store.write(|tx| {
        store::insert_run(tx,&NewRun{run_id:"recovered",agent_id:"agent",program:"return 1;",catalog:"[]",catalog_digest:&"a".repeat(64),description:None,limits:"{}",scope:"{}",deadline_ms:1000,admitted_at:100},None)?;
        let scope=serde_json::to_string(&cortexkit_role_tool_provider::scope::ScopeIdentity{owner:"reserved:broca".into(),scope_ref:"scope".into(),scope_epoch:1}).unwrap();
        tx.execute("INSERT INTO codemode_tool_calls(carrier,call_key,run_id,scope,custodian) VALUES('reserved:broca','held-recovery','recovered',?1,'reserved:broca')",[scope])?;
        // A held answer leaves published=0. Committing before Codemode exists
        // models the run reaching its terminal state while basal was down.
        store::commit_terminal(tx,"recovered",&Terminal{status:Status::Completed,value:Some("1".into()),value_head:None,error:None,output:String::new(),warnings:"[]".into()},110)?;
        Ok(())
    }).unwrap();

    let clock = Clock::manual(110);
    let config = basal_core::Config {
        clock: clock.clone(),
        ..Default::default()
    };
    let mut pool = crate::pool::PoolConfig::new("missing", crate::process::WorkerLaunch::Plain);
    pool.warm_spares = 0;
    let spawner = Arc::new(crate::pool::ProcessSpawner::codemode(&pool));
    let codemode = Codemode::new(
        store.clone(),
        &crate::module::Hosts {
            host: Arc::new(basal_host::mock::MockHost::new()),
            transport: Arc::new(crate::unconfigured::UnconfiguredTransport),
            catalog: Arc::new(basal_host::MockCatalog::new()),
            consent: Arc::new(basal_host::MockConsent::new()),
            hooks: Arc::new(basal_core::NoHooks),
        },
        Pool::new(pool, spawner, clock, Arc::default()),
        &config,
    )
    .unwrap();

    let (results, wakes, published) = store
        .read(|conn| {
            Ok((
                conn.query_row("SELECT COUNT(*) FROM codemode_late_results", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                conn.query_row(
                    "SELECT COUNT(*) FROM outbox WHERE kind='codemode.late_result_ready'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                conn.query_row(
                    "SELECT published FROM codemode_tool_calls WHERE call_key='held-recovery'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!((results, wakes, published), (1, 1, 1));
    drop((codemode, store));
    std::fs::remove_dir_all(root).unwrap();
}
