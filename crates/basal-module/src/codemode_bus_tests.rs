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
        let mut q=c.prepare("SELECT l.seq,l.custodian,o.recipient,o.body FROM codemode_late_results l JOIN outbox o ON o.kind='codemode.late_result_ready' AND json_extract(o.body,'$.seq')=l.seq ORDER BY l.seq")?;
        Ok(q.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }).unwrap();
    assert_eq!(rows.len(), 2);
    for (seq, custodian, module, body) in rows {
        assert_eq!(custodian, format!("reserved:{module}"));
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"seq":seq})
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
