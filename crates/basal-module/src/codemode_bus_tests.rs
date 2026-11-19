use super::*;
use basal_core::codemode::store::{self, NewRun, Status, Terminal};

#[test]
fn new_late_result_commits_one_wake_per_custodian_and_never_duplicates_it() {
    let root = std::env::temp_dir().join(format!("basal-late-wake-{}", random_id().unwrap()));
    let store = Store::open(root.join("store.db"), basal_core::Durability::throwaway()).unwrap();
    store.write(|tx| {
        store::insert_run(tx,&NewRun{run_id:"r",agent_id:"agent",program:"return 1;",catalog:"[]",catalog_digest:&"a".repeat(64),description:None,limits:"{}",scope:"{}",deadline_ms:1000,admitted_at:100},None)?;
        store::commit_terminal(tx,"r",&Terminal{status:Status::Completed,value:Some("1".into()),value_head:None,error:None,output:String::new(),warnings:"[]".into()},110)?;
        for module in ["broca","prefrontal"] {
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
