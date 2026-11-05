use super::*;
use crate::{Durability, Store};
use std::{path::PathBuf, sync::Arc};

struct Fixture {
    store: Arc<Store>,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = crate::test_support::timestamp_scratch("run-retention", "chunks");
        let store = Arc::new(Store::open(root.join("store.db"), Durability::default()).unwrap());
        store.write(|tx| {
            tx.execute_batch(include_str!("schema_run_log.sql"))?;
            tx.execute("INSERT INTO run_snapshots(snapshot_id,flow_id,flow_version,code_hash,script,manifest) VALUES(1,'flow',1,zeroblob(32),'return 1','{}')", [])?;
            Ok(())
        }).unwrap();
        Self { store, root }
    }
    fn add(&self, id: &str, ended: i64) {
        self.store.write(|tx| add_run(tx, id, ended)).unwrap();
    }
    fn count(&self, table: &str) -> usize {
        self.store
            .read(|c| Ok(c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?))
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn add_run(tx: &Transaction<'_>, id: &str, ended: i64) -> Result<()> {
    tx.execute("INSERT INTO run_history(run_id,flow_id,flow_version,trigger_id,trigger_kind,trigger,admitted_at,admit_seq,ended_at,state,result_size,call_counts,writes_sent,tokens_charged,cost_microusd,broken,activation_count,script_started,phase,deadline_exempt,history_lost,commit_seq,snapshot_id) VALUES(?1,'flow',1,?1,'schedule','{}',0,1,?2,'failed',0,'{}',0,0,0,0,1,1,'script',0,0,1,1)", params![id, ended])?;
    Ok(())
}

#[test]
fn bounded_multi_run_chunks_release_connection_and_cap_each_pass() {
    let f = Fixture::new();
    for n in 0..(BATCH + 5) {
        f.add(&format!("run-{n:03}"), 100);
    }
    let ids = f.store.read(|c| candidates(c, 100, false)).unwrap();
    assert_eq!(ids.len(), 128);
    assert_eq!(RUN_CHUNK, 16);
    let mut sizes = Vec::new();
    let report = chunks(&ids, |chunk| {
        let result = f.store.write(|tx| prune_chunk(tx, chunk, 100, 200))?;
        sizes.push(result.pruned.len());
        // A second connection acquisition after every commit would deadlock if
        // chunk execution retained the store's single connection.
        f.store.read(|c| {
            assert!(c.is_autocommit());
            Ok(())
        })?;
        Ok(result)
    })
    .unwrap();
    assert_eq!(sizes, vec![16; 8]);
    assert_eq!(report.pruned.len(), 128);
    assert_eq!(f.count("run_history"), 5);
    assert_eq!(prune(&f.store, 200, 100).unwrap().pruned.len(), 5);
    assert_eq!(f.count("run_history"), 0);
    assert_eq!(f.count("run_snapshots"), 0);
}

#[test]
fn every_candidate_rechecks_cutoff_and_obligations_in_its_chunk() {
    let f = Fixture::new();
    for n in 0..35 {
        f.add(&format!("run-{n:03}"), 100);
    }
    let ids = f.store.read(|c| candidates(c, 100, false)).unwrap();
    f.store.write(|tx| {
        // Change candidates in both chunks after the advisory scan. A single
        // fence on the first run of a chunk would miss these interior runs.
        for id in ["run-003", "run-019"] {
            tx.execute("UPDATE run_history SET ended_at=101 WHERE run_id=?1", [id])?;
        }
        for id in ["run-007", "run-023"] {
            tx.execute("INSERT INTO obligations(run_id,position,state,audit_at,detail) VALUES(?1,0,'unknown',50,'{}')", [id])?;
        }
        Ok(())
    }).unwrap();
    let report = chunks(&ids, |chunk| {
        f.store.write(|tx| prune_chunk(tx, chunk, 100, 200))
    })
    .unwrap();
    assert_eq!(report.pruned.len(), 31);
    assert_eq!(report.kept_unsettled, ["run-007", "run-023"]);
    assert_eq!(f.count("run_history"), 4);
    assert_eq!(f.count("run_snapshots"), 1);
}

#[test]
fn failed_chunk_rolls_back_all_its_runs_but_not_prior_chunks() {
    let f = Fixture::new();
    for n in 0..35 {
        f.add(&format!("run-{n:03}"), 100);
    }
    f.store.write(|tx| {
        tx.execute_batch("CREATE TRIGGER stop_prune BEFORE DELETE ON run_history WHEN old.run_id='run-018' BEGIN SELECT RAISE(ABORT,'injected retention failure'); END;")?;
        Ok(())
    }).unwrap();
    let error = prune(&f.store, 200, 100).unwrap_err();
    assert!(error.to_string().contains("injected retention failure"));
    // The first 16-run transaction committed. Even the first two successful
    // deletes of the failed second transaction must have been rolled back.
    assert_eq!(f.count("run_history"), 19);
    assert_eq!(f.count("tombstones"), 16);
}

#[test]
fn broca_acknowledgement_is_rechecked_and_wait_cleanup_precedes_identity_deletion() {
    let f = Fixture::new();
    f.add("broca", 100);
    let ids = f.store.read(|c| candidates(c, 100, false)).unwrap();
    f.store.write(|tx| {
        // Point finished Broca calls at run_history(run_id) for this test;
        // the production migration chain remains unchanged.
        tx.execute_batch("DROP TABLE broca_calls;
            CREATE TABLE broca_calls(send_id TEXT PRIMARY KEY,run_id TEXT NOT NULL REFERENCES run_history(run_id),position INTEGER NOT NULL,snapshot TEXT NOT NULL);
            CREATE TRIGGER broca_calls_forget_wait AFTER DELETE ON broca_calls BEGIN
                DELETE FROM broca_waits WHERE send_id=old.send_id;
                DELETE FROM broca_wakes WHERE send_id=old.send_id;
            END;")?;
        tx.execute("INSERT INTO broca_calls VALUES('send','broca',0,'{\"acknowledged\":false}')", [])?;
        tx.execute("INSERT INTO broca_waits(send_id) VALUES('send')", [])?;
        tx.execute("INSERT INTO broca_wakes(send_id,event_key,kind,digest,received_at) VALUES('send','event','ended','digest',1)", [])?;
        Ok(())
    }).unwrap();
    let kept = chunks(&ids, |chunk| {
        f.store.write(|tx| prune_chunk(tx, chunk, 100, 200))
    })
    .unwrap();
    assert!(kept.pruned.is_empty());
    assert_eq!(kept.kept_unsettled, ["broca"]);
    f.store
        .write(|tx| {
            tx.execute(
                "UPDATE broca_calls SET snapshot='{\"acknowledged\":true}'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(prune(&f.store, 200, 100).unwrap().pruned, ["broca"]);
    for table in [
        "broca_calls",
        "broca_waits",
        "broca_wakes",
        "run_identities",
        "run_snapshots",
    ] {
        assert_eq!(f.count(table), 0, "{table}");
    }
}

#[test]
fn receipts_calls_tombstones_and_last_snapshot_follow_history_lifetime() {
    let f = Fixture::new();
    f.add("first", 100);
    f.add("last", 200);
    f.store.write(|tx| {
        tx.execute("INSERT INTO obligations(run_id,position,handle,state,audit_at,detail,value_hash,settled_at) VALUES('first',0,'handle','settled',50,'{}',zeroblob(32),90)", [])?;
        tx.execute("INSERT INTO run_calls(run_id,position,kind,class,module,op,args_digest,outcome,issued_at,detail) VALUES('first',0,0,'keyed_mutation','test','write',zeroblob(32),'fulfilled',50,'{\"intent\":{\"idempotency_key\":\"write-key\"}}')", [])?;
        tx.execute("INSERT INTO tombstones VALUES('idempotency_key','success-key','first',90)", [])?;
        tx.execute("INSERT INTO trigger_inbox VALUES('flow','first',1,'first',0)", [])?;
        Ok(())
    }).unwrap();
    assert!(prune(&f.store, 199, 100).unwrap().pruned.is_empty());
    assert_eq!(f.count("obligations"), 1);
    assert_eq!(prune(&f.store, 200, 100).unwrap().pruned, ["first"]);
    for table in ["obligations", "run_calls", "trigger_inbox"] {
        assert_eq!(f.count(table), 0, "{table}");
    }
    assert_eq!(f.count("run_identities"), 1);
    assert_eq!(f.count("run_snapshots"), 1);
    let keys = f
        .store
        .read(|c| {
            Ok(
                c.prepare("SELECT kind,key FROM tombstones ORDER BY kind,key")?
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            )
        })
        .unwrap();
    assert_eq!(
        keys,
        vec![
            ("idempotency_key".into(), "success-key".into()),
            ("idempotency_key".into(), "write-key".into()),
            ("trigger".into(), trigger_key("flow", "first"))
        ]
    );
    assert_eq!(prune(&f.store, 300, 100).unwrap().pruned, ["last"]);
    assert_eq!(f.count("run_snapshots"), 0);
    assert_eq!(f.count("tombstones"), 4);
}

#[test]
fn live_identity_keeps_the_last_shared_code_snapshot() {
    let f = Fixture::new();
    f.add("closed", 100);
    f.store.write(|tx| {
        tx.execute("INSERT INTO live_runs(run_id,flow_id,flow_version,trigger_id,trigger_kind,trigger,attempt,admitted_at,admit_seq,admit_epoch,admit_segment,admit_offset,deadline_ms,self_input,params_input,grant_guards,end_instance,snapshot_id) VALUES('live','flow',1,'live','manual','{}',1,0,2,1,0,0,100,'{}','{}','[]',0,1)", [])?;
        Ok(())
    }).unwrap();
    assert_eq!(prune(&f.store, 200, 100).unwrap().pruned, ["closed"]);
    assert_eq!(f.count("live_runs"), 1);
    assert_eq!(f.count("run_identities"), 1);
    assert_eq!(f.count("run_snapshots"), 1);
}

#[test]
fn all_obligation_kinds_protect_runs_without_starving_other_candidates() {
    let f = Fixture::new();
    for n in 0..130 {
        let id = format!("open-{n:03}");
        f.add(&id, 100);
        f.store.write(|tx| {
            tx.execute("INSERT INTO obligations(run_id,position,state,audit_at,detail) VALUES(?1,0,'open',50,'{}')", [&id])?;
            Ok(())
        }).unwrap();
    }
    for id in ["reserved", "dollars", "card", "refusal", "free"] {
        f.add(id, 100);
    }
    f.store.write(|tx| {
        tx.execute("INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,reserved_at) VALUES('send','flow','reserved',0,100,0,1,'reserved',50)", [])?;
        tx.execute("INSERT INTO classify_costs(kind,run_id,position,flow_id,created_at,reserved,args) VALUES('flow','dollars',0,'flow',50,1,'{}')", [])?;
        tx.execute("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,run_id,position,call_key,instance,card,state,created_at) VALUES('card','reconcile','flow',1,'card',0,'key',1,'{}','open',50)", [])?;
        tx.execute("INSERT INTO outbox(at,kind,flow_id,body) VALUES(50,'refusal_committed','flow','{\"run_id\":\"refusal\"}')", [])?;
        Ok(())
    }).unwrap();
    assert_eq!(prune(&f.store, 200, 100).unwrap().pruned, ["free"]);
    assert_eq!(f.count("run_history"), 134);
}

#[test]
fn history_predicates_keep_identity_dependencies_and_latest_reenable() {
    let f = Fixture::new();
    f.add("kept", 200);
    f.store.write(|tx| {
        for id in ["kept", "gone"] {
            tx.execute("INSERT INTO quarantine(run_id,position,settlement,value,payload_hash,reason,at) VALUES(?1,0,'fulfilled','1',zeroblob(32),'unknown_call',1)", [id])?;
            tx.execute("INSERT INTO audit(at,actor,action,run_id,detail) VALUES(1,'test','test',?1,'{}')", [id])?;
            tx.execute("INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,reserved_at,settled_at) VALUES(?1,'flow',?1,0,100,0,1,'settled',1,2)", [id])?;
        }
        tx.execute("INSERT INTO token_windows(flow_id,window_ms,window_start,reserved) VALUES('flow',100,0,1)", [])?;
        for n in 1..=2 {
            tx.execute("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at,answered_at) VALUES(?1,'reenable','flow',1,?2,'{}','declined',1,2)", params![format!("reenable-{n}"),n])?;
        }
        Ok(())
    }).unwrap();
    assert!(!prune_history(&f.store, 100, 100, |_| vec![]).unwrap());
    for table in [
        "quarantine",
        "audit",
        "token_ledger",
        "token_windows",
        "decision_cards",
    ] {
        assert_eq!(f.count(table), 1, "{table}");
    }
    prune(&f.store, 300, 100).unwrap();
    prune_history(&f.store, 300, 300, |_| vec![]).unwrap();
    for table in ["quarantine", "audit", "token_ledger", "token_windows"] {
        assert_eq!(f.count(table), 0, "{table}");
    }
    assert_eq!(f.count("decision_cards"), 1);
}

#[test]
fn rate_windows_use_live_snapshot_and_transactional_closed_obligations() {
    let f = Fixture::new();
    f.add("closed", 100);
    f.store.write(|tx| {
        tx.execute("INSERT INTO obligations(run_id,position,state,audit_at,detail) VALUES('closed',0,'unknown',250,'{}')", [])?;
        for start in [0,100,200,300] {
            tx.execute("INSERT INTO rate_windows(flow_id,window_ms,window_start,runs) VALUES('flow',100,?1,1)", [start])?;
        }
        Ok(())
    }).unwrap();
    let calls = std::cell::Cell::new(0);
    prune_history(&f.store, 500, 350, |flow| {
        assert_eq!(flow, "flow");
        // Snapshot and revalidation both happen without the SQLite connection.
        f.store
            .read(|c| {
                assert!(c.is_autocommit());
                Ok(())
            })
            .unwrap();
        calls.set(calls.get() + 1);
        vec![150]
    })
    .unwrap();
    assert_eq!(calls.get(), 2);
    let starts = f
        .store
        .read(|c| {
            Ok(
                c.prepare("SELECT window_start FROM rate_windows ORDER BY window_start")?
                    .query_map([], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            )
        })
        .unwrap();
    assert_eq!(starts, [100, 200, 300]);
    let snapshots = std::cell::Cell::new(0);
    assert!(
        prune_history(&f.store, 500, 350, |_| {
            snapshots.set(snapshots.get() + 1);
            if snapshots.get() == 1 {
                vec![150]
            } else {
                vec![]
            }
        })
        .unwrap(),
        "accounting changes request a prompt maintenance pass"
    );
    prune_history(&f.store, 500, 350, |_| vec![]).unwrap();
    assert_eq!(f.count("rate_windows"), 2);
}

#[test]
fn live_rate_windows_do_not_starve_later_expired_windows() {
    let f = Fixture::new();
    f.store.write(|tx| {
        for start in 0..=128 {
            tx.execute("INSERT INTO rate_windows(flow_id,window_ms,window_start,runs) VALUES('flow',1,?1,1)", [start])?;
        }
        Ok(())
    }).unwrap();
    assert!(!prune_history(&f.store, 200, 200, |_| (0..128).collect()).unwrap());
    assert_eq!(f.count("rate_windows"), 128);
    f.store
        .read(|c| {
            assert_eq!(
                c.query_row("SELECT max(window_start) FROM rate_windows", [], |r| r
                    .get::<_, i64>(0))?,
                127
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn live_accounting_protects_token_windows_before_a_ledger_row_exists() {
    let f = Fixture::new();
    f.store.write(|tx| {
        for start in [0,100,200,300] {
            tx.execute("INSERT INTO token_windows(flow_id,window_ms,window_start,reserved) VALUES('flow',100,?1,0)", [start])?;
        }
        Ok(())
    }).unwrap();
    assert_eq!(f.count("token_ledger"), 0);
    prune_history(&f.store, 350, 350, |_| vec![150]).unwrap();
    let starts = f
        .store
        .read(|c| {
            Ok(
                c.prepare("SELECT window_start FROM token_windows ORDER BY window_start")?
                    .query_map([], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            )
        })
        .unwrap();
    assert_eq!(starts, [100, 300]);
    prune_history(&f.store, 350, 350, |_| vec![]).unwrap();
    assert_eq!(f.count("token_windows"), 1);
}
