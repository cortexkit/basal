use super::*;
use crate::run_records::Body;
use crate::{Durability, NoHooks, run_store_sql as sql};
use basal_proto::{JsonText, Settlement};
use rusqlite::params;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    store: Option<Arc<Store>>,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(format!(
                "cutover-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&root).unwrap();
        let store =
            Arc::new(Store::open(root.join("store.sqlite"), Durability::throwaway()).unwrap());
        // Model the pre-cutover page shape, not the fresh-store initializer.
        store
            .read(|c| {
                c.pragma_update(None, "journal_mode", "DELETE")?;
                c.pragma_update(None, "page_size", 4096)?;
                c.execute_batch("VACUUM")?;
                c.pragma_update(None, "journal_mode", "WAL")?;
                Ok(())
            })
            .unwrap();
        store.write(|tx| {
            let hash=crate::ids::code_hash("return 7;","{}");
            tx.execute("INSERT INTO run_snapshots(snapshot_id,flow_id,flow_version,code_hash,script,manifest) VALUES(1,'flow',1,?1,'return 7;','{}')",[hash.as_slice()])?;
            for (id,state,seq) in [("success","succeeded",1),("failed","failed",2),("live","suspended",3)] {
                tx.execute("INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,snapshot_id,code_hash,state,broken,admitted_at,ended_at,flow_version,admit_seq,deadline_ms,deadline_at,self_input,params_input,grant_guards,template_incarnation,end_instance,script_started,generation,awaited,result) VALUES(?1,'flow',?1,0,'{\"kind\":\"manual\"}',1,?2,?3,3,1,?4,1,?5,10000,10001,'{\"self\":1}','{\"param\":2}','[]',99,1,1,7,'[0]',?6)",params![id,hash.as_slice(),state,if id=="live"{None}else{Some(10)},seq,if id=="success"{Some("7")}else{None}])?;
                tx.execute("INSERT INTO activations VALUES(?1,1,'old',1),(?1,7,'old',5)",[id])?;
                tx.execute("INSERT INTO trigger_inbox VALUES('flow',?1,0,?1,1)",[id])?;
            }
            let digest=basal_proto::ArgsDigest::of(&JsonText::new("{}").unwrap());
            let value=JsonText::new("7").unwrap();
            let hash=crate::ids::payload_hash(Settlement::Fulfilled,&value);
            for id in ["success","failed","live"] {
                tx.execute("INSERT INTO journal(run_id,position,kind_code,module,op,args_digest,idempotency_key,class,dispatch,attempts,handle,settlement,payload_hash,delivery_order,issued_generation,args_bytes,request_bytes,value_bytes) VALUES(?1,0,0,'model','call',?2,?3,'keyed_mutation','accepted',1,?4,?5,?6,?7,7,2,2,?8)",params![id,digest.0.as_slice(),format!("key:{id}"),format!("handle:{id}"),if id=="success"{Some("fulfilled")}else{None},if id=="success"{Some(hash.as_slice())}else{None},if id=="success"{Some(0)}else{None},if id=="success"{Some(1)}else{None}])?;
                tx.execute("INSERT INTO journal_payloads VALUES(?1,0,'{}','{}',?2)",params![id,if id=="success"{Some("7")}else{None}])?;
                tx.execute("INSERT INTO call_audit VALUES(?1,0,'flow','model.call',?2,'allowed',2)",params![id,digest.0.as_slice()])?;
                tx.execute("INSERT INTO broca_calls VALUES(?1,?2,0,?3)",params![format!("key:{id}"),id,serde_json::json!({"acknowledged":false,"handle":format!("handle:{id}")}).to_string()])?;
                tx.execute("INSERT INTO broca_waits(send_id,broca_run_id,check_at) VALUES(?1,?2,20)",params![format!("key:{id}"),format!("broca:{id}")])?;
            }
            tx.execute("INSERT INTO rate_windows(flow_id,window_ms,window_start,runs,dispatches) VALUES('flow',60000,0,3,9)",[])?;
            tx.execute("INSERT INTO schedules VALUES('flow',1,'{}','active',0,60000,NULL,0)",[])?;
            tx.execute("INSERT INTO schedule_fires(flow_id,trigger_id,version,due_ms,payload,planned_at) VALUES('flow','queued',1,2,'{}',1)",[])?;
            tx.execute("UPDATE sqlite_sequence SET seq=81 WHERE name='schedule_fires'",[])?;
            tx.execute("INSERT INTO tombstones VALUES('trigger','older','pruned',0)",[])?;
            tx.execute("INSERT INTO broca_wakes(send_id,event_key,kind,digest,run_id,session_ref,seq,received_at) VALUES('key:live','event','ended','digest','broca:live','session','9',20)",[])?;
            tx.execute("INSERT INTO token_windows(flow_id,window_ms,window_start,reserved) VALUES('flow',60000,0,20)",[])?;
            for id in ["failed","live"] {
                tx.execute("INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,reserved_at) VALUES(?1,'flow',?2,0,60000,0,10,'reserved',2)",params![format!("key:{id}"),id])?;
                tx.execute("INSERT INTO classify_costs(kind,flow_id,run_id,position,created_at,reserved,charged,args) VALUES('flow','flow',?1,0,2,0.3,NULL,'{}')",[id])?;
            }
            Ok(())
        }).unwrap();
        Self {
            root,
            store: Some(store),
        }
    }
    fn store(&self) -> Arc<Store> {
        self.store.as_ref().unwrap().clone()
    }
    fn migration(&self, hooks: Arc<dyn Hooks>) -> Migration {
        Migration::new(self.store(), self.root.join("run-log"), hooks)
    }
    fn reopen(&mut self) {
        self.store.take();
        self.store = Some(Arc::new(
            Store::open(self.root.join("store.sqlite"), Durability::throwaway()).unwrap(),
        ));
    }
    fn check(&self) {
        self.store()
            .read(|c| {
                assert_eq!(phase_from_connection(c)?, Phase::Done);
                assert!(!c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM meta WHERE key='migration_audit')",
                    [],
                    |r| r.get::<_, bool>(0)
                )?);
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM run_history", [], |r| r
                        .get::<_, i64>(0))?,
                    2
                );
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM run_calls", [], |r| r.get::<_, i64>(0))?,
                    1
                );
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM live_runs", [], |r| r.get::<_, i64>(0))?,
                    1
                );
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM run_identities", [], |r| r
                        .get::<_, i64>(0))?,
                    3
                );
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM broca_waits", [], |r| r
                        .get::<_, i64>(0))?,
                    3
                );
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM obligations", [], |r| r
                        .get::<_, i64>(0))?,
                    1
                );
                assert_eq!(
                    c.query_row("SELECT value FROM meta WHERE key='next_run'", [], |r| {
                        r.get::<_, String>(0)
                    })?,
                    "82"
                );
                assert_eq!(sql::history(c, "failed")?.unwrap().activation_count, 2);
                assert_eq!(sql::history(c, "failed")?.unwrap().broken, 3);
                let live = sql::live(c, "live")?.unwrap();
                assert_eq!(live.template_incarnation, Some(99));
                assert!(live.end_instance);
                let receipt = sql::obligation(c, "success", 0)?.unwrap();
                assert_eq!(receipt.state, sql::ObligationState::Settled);
                assert_eq!(
                    receipt.receipt,
                    Some(sql::Receipt::Value(crate::ids::payload_hash(
                        Settlement::Fulfilled,
                        &JsonText::new("7").unwrap()
                    )))
                );
                check_broca_ownership(c)?;
                pages::verify(c)?;
                audit::drop_dependencies(c)?;
                Ok(())
            })
            .unwrap();
        let frames = convert::snapshots(&self.store(), &self.root.join("run-log")).unwrap();
        let snapshots: Vec<_> = frames
            .iter()
            .filter_map(|f| {
                if let Body::Snapshot { state, .. } = &f.record.body {
                    Some((f.record.run_id.as_str(), f.lsn.sequence, state))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(snapshots.len(), 1);
        assert_eq!((snapshots[0].0, snapshots[0].1), ("live", 1));
        assert_eq!(snapshots[0].2.activation_count, 2);
        assert_eq!(snapshots[0].2.broken, 3);
        assert!(snapshots[0].2.overlay.is_empty());
        assert!(snapshots[0].2.calls[&0].pre_accounted_attempts > 0);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.store.take();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
struct Cut(Boundary);
impl Hooks for Cut {
    fn at(&self, _: &str, b: &Boundary) -> Step {
        if *b == self.0 {
            Step::Crash
        } else {
            Step::Continue
        }
    }
}

pub(crate) fn migration_case(boundary: Boundary) {
    let mut fixture = Fixture::new();
    let migration = fixture.migration(Arc::new(Cut(boundary.clone())));
    assert!(
        matches!(migration.run(), Err(CoreError::Cut)),
        "{boundary:?}"
    );
    drop(migration);
    fixture.reopen();
    fixture.migration(Arc::new(NoHooks)).run().unwrap();
    fixture.check();
    fixture.reopen();
    fixture.migration(Arc::new(NoHooks)).run().unwrap();
    fixture.check();
}

#[test]
fn every_migration_phase_resumes_with_durable_post_checks() {
    for boundary in Boundary::MIGRATION {
        migration_case(boundary.clone());
    }
}
#[test]
fn resumed_finish_reads_persisted_evidence_and_catches_changed_accounting() {
    let mut fixture = Fixture::new();
    {
        let migration = fixture.migration(Arc::new(Cut(Boundary::MigrationDropCommitted)));
        assert!(matches!(migration.run(), Err(CoreError::Cut)));
    }
    fixture.reopen();
    assert_eq!(phase(&fixture.store()).unwrap(), Phase::PageSize);
    fixture
        .store()
        .write(|tx| {
            tx.execute("UPDATE rate_windows SET dispatches=dispatches+1", [])?;
            Ok(())
        })
        .unwrap();
    let error = fixture
        .migration(Arc::new(NoHooks))
        .run()
        .unwrap_err()
        .to_string();
    assert!(error.contains("post-check rate_windows"), "{error}");
    assert_eq!(phase(&fixture.store()).unwrap(), Phase::PageSize);
    assert!(
        fixture
            .store()
            .read(|c| Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM meta WHERE key='migration_audit')",
                [],
                |r| r.get::<_, bool>(0)
            )?))
            .unwrap()
    );
}

#[test]
fn fresh_store_selects_1024_before_basal_schema_and_bootstraps() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!(
            "fresh-cutover-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(root.join("fresh.db"), Durability::default()).unwrap());
    store.read(pages::verify).unwrap();
    Migration::new(store.clone(), root.join("run-log"), Arc::new(NoHooks))
        .run()
        .unwrap();
    assert_eq!(phase(&store).unwrap(), Phase::Done);
    store.read(pages::verify).unwrap();
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn insufficient_vacuum_space_refuses_without_changing_durability_or_leaving_delete_mode() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let before = store.pragmas().unwrap();
    let error = pages::with_free_space(&store, 0).unwrap_err().to_string();
    assert!(
        error.contains("insufficient free space") && error.contains("2.5x"),
        "{error}"
    );
    let after = store.pragmas().unwrap();
    assert!(after.journal_mode_wal);
    assert_eq!(after.synchronous, before.synchronous);
    assert_eq!(after.fullfsync, before.fullfsync);
    assert_eq!(after.checkpoint_fullfsync, before.checkpoint_fullfsync);
}

#[test]
fn migrated_reservations_settle_in_place_without_recounting_dispatches() {
    use crate::run_records::{Arrival, ArrivalDurability, Usage};
    let fixture = Fixture::new();
    let store = fixture.store();
    fixture.migration(Arc::new(NoHooks)).run().unwrap();
    let frames = convert::snapshots(&store, &fixture.root.join("run-log")).unwrap();
    let mut state = frames
        .into_iter()
        .find_map(|f| {
            if let Body::Snapshot { state, .. } = f.record.body {
                Some(*state)
            } else {
                None
            }
        })
        .unwrap();
    let value = JsonText::new("7").unwrap();
    state.calls.get_mut(&0).unwrap().arrival = Some(Arrival {
        position: 0,
        arrival_seq: 0,
        outcome: "fulfilled".into(),
        settlement: Settlement::Fulfilled,
        value_hash: crate::ids::payload_hash(Settlement::Fulfilled, &value),
        value,
        handle: Some("handle:live".into()),
        usage: Some(Usage {
            input_tokens: 2,
            output_tokens: 3,
        }),
        cost_micros: None,
        kv_effect: None,
        durability: ArrivalDurability::default(),
    });
    let budgets = crate::run_driver::Budgets {
        dispatch_window_ms: 60000,
        token_window_ms: 60000,
        dispatches: 1000,
        tokens: 1000,
        cost_micros: 1_000_000,
        cost_window: None,
    };
    assert!(
        crate::run_driver::accounting::rows(&state, &budgets)
            .unwrap()
            .is_empty()
    );
    for _ in 0..2 {
        store
            .write(|tx| {
                crate::run_driver::accounting::fold(tx, "flow", "live", &state, &budgets, 20)
            })
            .unwrap();
    }
    store.read(|c| {
        assert_eq!(c.query_row("SELECT COUNT(*) FROM token_ledger",[],|r|r.get::<_,i64>(0))?,2);
        let row:(i64,i64,i64)=c.query_row("SELECT reserved,input_tokens,output_tokens FROM token_windows WHERE flow_id='flow'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;assert_eq!(row,(10,2,3));
        assert_eq!(c.query_row("SELECT dispatches FROM rate_windows WHERE flow_id='flow'",[],|r|r.get::<_,i64>(0))?,9);
        assert_eq!(c.query_row("SELECT charged FROM classify_costs WHERE run_id='live'",[],|r|r.get::<_,f64>(0))?,0.3);
        Ok(())
    }).unwrap();
}

#[test]
fn settled_receipts_keep_source_provenance_only_in_the_transient_migration_audit() {
    for cancelled in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.store();
        let value = JsonText::new("{\"code\":\"scope_ended\"}").unwrap();
        let hash = crate::ids::payload_hash(Settlement::Rejected, &value);
        store.write(|tx| {
            if cancelled {
                tx.execute("UPDATE runs SET state='cancelled' WHERE run_id='failed'",[])?;
                tx.execute("INSERT INTO quarantine(run_id,position,handle,settlement,value,payload_hash,reason,at) VALUES('failed',0,'handle:failed','rejected',?1,?2,'run_cancelled',12)",params![value.as_str(),hash.as_slice()])?;
            } else {
                tx.execute("INSERT INTO mailbox(run_id,position,handle,settlement,value,payload_hash,source,arrived_generation) VALUES('failed',0,'handle:failed','rejected',?1,?2,'host',7)",params![value.as_str(),hash.as_slice()])?;
                tx.execute("INSERT INTO outbox(at,kind,flow_id,body) VALUES(8,'refusal_committed','flow','{\"run_id\":\"failed\",\"position\":0,\"reason\":\"scope_ended\"}')",[])?;
            }
            Ok(())
        }).unwrap();
        let migration = fixture.migration(Arc::new(NoHooks));
        while migration.phase().unwrap() != Phase::Finish {
            migration.advance().unwrap();
        }
        store
            .read(|c| {
                let receipt = sql::obligation(c, "failed", 0)?.unwrap();
                assert_eq!(receipt.state, sql::ObligationState::Settled);
                assert_eq!(receipt.receipt, Some(sql::Receipt::Value(hash)));
                let source = if cancelled {
                    "run_cancelled_quarantine_receipts"
                } else {
                    "refusal_outbox_rows"
                };
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&c.query_row::<String, _, _>(
                        "SELECT value FROM meta WHERE key='migration_audit'",
                        [],
                        |r| r.get(0)
                    )?)
                    .unwrap()["late"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|entry| entry["run_id"] == "failed")
                        .unwrap()["sources"][source]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                assert!(
                    receipt.detail.is_null(),
                    "settled provenance must not retain call payloads"
                );
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM obligations", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                check_broca_ownership(c)?;
                Ok(())
            })
            .unwrap();
        migration.run().unwrap();
        assert_eq!(
            store
                .read(|c| Ok(c.query_row(
                    "SELECT COUNT(*) FROM meta WHERE key='migration_audit'",
                    [],
                    |r| r.get::<_, u64>(0)
                )?))
                .unwrap(),
            0
        );
    }
}

#[test]
fn retained_view_and_trigger_references_block_drop_until_removed() {
    for ddl in [
        "CREATE VIEW unexpected_reader AS SELECT * FROM journal",
        "CREATE TRIGGER unexpected_writer AFTER DELETE ON outbox BEGIN DELETE FROM runs; END",
    ] {
        let fixture = Fixture::new();
        let store = fixture.store();
        store
            .write(|tx| {
                tx.execute_batch(ddl)?;
                Ok(())
            })
            .unwrap();
        let error = fixture
            .migration(Arc::new(NoHooks))
            .run()
            .unwrap_err()
            .to_string();
        assert!(error.contains("retained sqlite_schema"), "{error}");
        assert_eq!(phase(&store).unwrap(), Phase::Drop);
        assert!(
            store
                .read(|c| Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='runs')",
                    [],
                    |r| r.get::<_, bool>(0)
                )?))
                .unwrap()
        );
        store.write(|tx|{tx.execute_batch("DROP VIEW IF EXISTS unexpected_reader;DROP TRIGGER IF EXISTS unexpected_writer;")?;Ok(())}).unwrap();
        fixture.migration(Arc::new(NoHooks)).run().unwrap();
        fixture.check();
    }
}

#[test]
fn released_broca_outcome_redelivers_as_duplicate_then_host_records_acknowledgement() {
    use basal_host::broca::{
        BrocaHost, StateStore, StoredOutcome,
        fake::{FakeBroca, MemoryStore},
    };
    use basal_host::selector::{FakeSelector, ModelSelector, SelectionRequest};
    use basal_host::{
        CallRequest, Completion, CompletionAck, CompletionSink, Host, SinkError, UnknownOutcome,
    };
    use basal_proto::{CallKind, Primitive};
    let fixture = Fixture::new();
    let store = fixture.store();
    let memory = Arc::new(MemoryStore::default());
    let selector = Arc::new(FakeSelector::default());
    let selection = selector
        .select(&SelectionRequest {
            caller_class: "flow".into(),
            iq: 1,
            eq: 1,
            flow_id: "flow".into(),
            run_id: "success".into(),
            send_id: "key:success".into(),
        })
        .unwrap();
    let request=CallRequest{flow_version:Some(1),flow_id:"flow".into(),run_id:"success".into(),position:0,kind:CallKind::Primitive(Primitive::Llm),args:JsonText::new(serde_json::json!({"send_id":"key:success","work_class":"flow:flow","session":"basal:flow-flow:success:0","op":"llm","max_output":12,"request":{"prompt":"synthetic"},"selection":selection}).to_string()).unwrap(),idempotency_key:"key:success".into(),attempt:1};
    BrocaHost::new(
        Arc::new(FakeBroca::default()),
        memory.clone(),
        "/".into(),
        "basal".into(),
        selector.clone(),
    )
    .dispatch_model(&request)
    .unwrap();
    let mut saved = memory.get("key:success").unwrap().unwrap();
    saved.handle = Some("handle:success".into());
    saved.outcome = Some(StoredOutcome {
        rejected: false,
        value: "7".into(),
        usage: None,
    });
    saved.state = Some("succeeded".into());
    saved.acknowledged = false;
    store
        .write(|tx| {
            tx.execute("DELETE FROM broca_calls WHERE run_id<>'success'", [])?;
            tx.execute(
                "UPDATE broca_calls SET snapshot=?1 WHERE run_id='success'",
                [serde_json::to_string(&saved).unwrap()],
            )?;
            tx.execute(
                "UPDATE journal SET kind_code=?1,module=NULL,op=NULL WHERE run_id='success'",
                [i64::from(Primitive::Llm.code())],
            )?;
            tx.execute(
                "UPDATE journal_payloads SET request=?1 WHERE run_id='success'",
                [request.args.as_str()],
            )?;
            Ok(())
        })
        .unwrap();
    fixture.migration(Arc::new(NoHooks)).run().unwrap();
    let meta = store.read(sql::meta).unwrap();
    let (writer, frames) = crate::run_log::RunLog::<crate::run_records::Record>::open_restoring(
        crate::run_log::Config {
            directory: fixture.root.join("run-log"),
            identity: crate::run_log::Identity {
                store_id: meta.store_id.as_bytes().try_into().unwrap(),
                log_epoch: meta.log_epoch,
            },
            lease_epoch: store.lease_epoch(),
            target_size: crate::run_log::Config::TARGET_SIZE,
            hooks: Arc::new(NoHooks),
        },
        store.clone(),
    )
    .unwrap();
    let watermark = Arc::new(writer.watermark());
    let writer = Arc::new(writer);
    let hosts = Arc::new(
        crate::run_host::RecoveredHosts::recover(
            store.clone(),
            writer,
            watermark,
            &frames.records,
            crate::run_driver::Budgets {
                dispatch_window_ms: 60000,
                token_window_ms: 60000,
                dispatches: 1000,
                tokens: 1000,
                cost_micros: 1_000_000,
                cost_window: None,
            },
            Arc::new(NoHooks),
            crate::Clock::system(),
        )
        .unwrap(),
    );
    struct Observe {
        hosts: Arc<crate::run_host::RecoveredHosts>,
        acks: std::sync::Mutex<Vec<CompletionAck>>,
    }
    impl CompletionSink for Observe {
        fn complete(&self, c: &Completion) -> std::result::Result<CompletionAck, SinkError> {
            let ack = self.hosts.complete(c)?;
            self.acks.lock().unwrap().push(ack);
            Ok(ack)
        }
        fn unknown(&self, c: &UnknownOutcome) -> std::result::Result<CompletionAck, SinkError> {
            self.hosts.unknown(c)
        }
    }
    let adapter = Arc::new(crate::broca::BrocaStore::new_staged(&hosts));
    let observer = Arc::new(Observe {
        hosts,
        acks: std::sync::Mutex::new(Vec::new()),
    });
    let host = BrocaHost::new(
        Arc::new(FakeBroca::default()),
        adapter.clone(),
        "/".into(),
        "basal".into(),
        selector,
    );
    host.attach(observer.clone());
    assert_eq!(*observer.acks.lock().unwrap(), [CompletionAck::Duplicate]);
    assert!(adapter.get("key:success").unwrap().unwrap().acknowledged);
    store
        .read(|c| {
            let receipt = sql::obligation(c, "success", 0)?.unwrap();
            assert_eq!(
                receipt.receipt,
                Some(sql::Receipt::Value(crate::ids::payload_hash(
                    Settlement::Fulfilled,
                    &JsonText::new("7").unwrap()
                )))
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM obligations WHERE run_id='success'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM quarantine", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            check_broca_ownership(c)?;
            Ok(())
        })
        .unwrap();
}

#[test]
fn migrated_broca_deletion_cleans_waits_and_wakes_after_preserving_them_during_rebuild() {
    let fixture = Fixture::new();
    fixture.migration(Arc::new(NoHooks)).run().unwrap();
    let store = fixture.store();
    let counts = || {
        store
            .read(|c| {
                Ok((
                    c.query_row(
                        "SELECT COUNT(*) FROM broca_waits WHERE send_id='key:live'",
                        [],
                        |r| r.get::<_, u64>(0),
                    )?,
                    c.query_row(
                        "SELECT COUNT(*) FROM broca_wakes WHERE send_id='key:live'",
                        [],
                        |r| r.get::<_, u64>(0),
                    )?,
                ))
            })
            .unwrap()
    };
    assert_eq!(counts(), (1, 1));
    store
        .write(|tx| {
            tx.execute("DELETE FROM broca_calls WHERE send_id='key:live'", [])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(counts(), (0, 0));
}

#[test]
fn drop_removes_and_audits_all_orphaned_journal_counters() {
    let f = Fixture::new();
    let store = f.store();
    store.write(|tx|{tx.execute_batch("INSERT INTO meta VALUES('journal_calls:success','10'),('journal_bytes:success','500'),('journal_calls:orphan','22'),('journal_calls_not_a_counter:kept','1');")?;Ok(())}).unwrap();
    let migration = f.migration(Arc::new(NoHooks));
    while migration.phase().unwrap() != Phase::Drop {
        migration.advance().unwrap();
    }
    let audit: serde_json::Value = store
        .read(|c| {
            Ok(serde_json::from_str(&c.query_row::<String, _, _>(
                "SELECT value FROM meta WHERE key='migration_audit'",
                [],
                |r| r.get(0),
            )?)
            .unwrap())
        })
        .unwrap();
    assert_eq!(audit["journal_meta_keys_before"], 3);
    migration.advance().unwrap();
    assert_eq!(store.read(audit::journal_meta_keys).unwrap(), 0);
    while migration.phase().unwrap() != Phase::Finish {
        migration.advance().unwrap();
    }
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO meta VALUES('journal_bytes:reintroduced','4')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(
        migration
            .run()
            .unwrap_err()
            .to_string()
            .contains("legacy journal meta keys")
    );
    store
        .write(|tx| {
            tx.execute(
                "DELETE FROM meta WHERE key='journal_bytes:reintroduced'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    migration.run().unwrap();
    assert_eq!(
        store
            .read(|c| Ok(c.query_row(
                "SELECT value FROM meta WHERE key='journal_calls_not_a_counter:kept'",
                [],
                |r| r.get::<_, String>(0)
            )?))
            .unwrap(),
        "1"
    );
}

#[test]
fn migrated_success_receipts_exclude_legacy_call_values() {
    let f = Fixture::new();
    let store = f.store();
    let value = JsonText::new(
        serde_json::json!({"private":"MIGRATED-PRIVATE-VALUE".repeat(4096)}).to_string(),
    )
    .unwrap();
    let hash = crate::ids::payload_hash(Settlement::Fulfilled, &value);
    store.write(|tx|{
        tx.execute("UPDATE journal_payloads SET value=?1 WHERE run_id='success' AND position=0",[value.as_str()])?;
        tx.execute("UPDATE journal SET payload_hash=?1,value_bytes=?2 WHERE run_id='success' AND position=0",params![hash.as_slice(),value.len() as u64])?;
        Ok(())
    }).unwrap();
    f.migration(Arc::new(NoHooks)).run().unwrap();
    store
        .read(|c| {
            let text: String = c.query_row(
                "SELECT receipts FROM run_history WHERE run_id='success'",
                [],
                |r| r.get(0),
            )?;
            assert!(!text.contains("MIGRATED-PRIVATE-VALUE"));
            assert!(!text.contains("detail"));
            assert!(text.len() < 512);
            let receipt = sql::obligation(c, "success", 0)?.unwrap();
            assert_eq!(receipt.receipt, Some(sql::Receipt::Value(hash)));
            assert!(receipt.detail.is_null());
            Ok(())
        })
        .unwrap();
}
