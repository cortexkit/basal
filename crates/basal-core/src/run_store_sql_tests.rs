use super::*;
use crate::schema::MIGRATIONS;
use crate::{Durability, Store};
use serde_json::json;

fn initial_meta() -> Meta {
    Meta {
        store_id: "11223344556677889900aabbccddeeff".into(),
        log_epoch: 7,
        log_hwm: EpochLsn {
            log_epoch: 7,
            lsn: Lsn {
                sequence: 3,
                offset: 512,
            },
        },
        commit_seq: 20,
        gc_floor: GcFloor {
            log_epoch: 7,
            first_retained: 3,
            predecessor: Some(SegmentEnd {
                lsn: Lsn {
                    sequence: 2,
                    offset: 8192,
                },
                crc: 1234,
            }),
            synced_lsn: Lsn {
                sequence: 3,
                offset: 512,
            },
            live_starts: BTreeMap::from([(
                "run-live".into(),
                Lsn {
                    sequence: 3,
                    offset: 128,
                },
            )]),
        },
        migration_phase: "create".into(),
    }
}

fn legacy() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    for m in MIGRATIONS {
        conn.execute_batch(m.statements).unwrap();
    }
    conn
}

fn fixture() -> Connection {
    let mut conn = legacy();
    let tx = conn.transaction().unwrap();
    create_test_schema(&tx, &initial_meta()).unwrap();
    tx.execute(
        "INSERT INTO run_snapshots(snapshot_id,flow_id,flow_version,code_hash,script,manifest)
        VALUES (1,'flow',2,?1,'return 1','{}')",
        [[42u8; 32].as_slice()],
    )
    .unwrap();
    tx.commit().unwrap();
    conn
}

fn id(run_id: &str) -> RunIdentity {
    RunIdentity {
        run_id: run_id.into(),
        flow_id: "flow".into(),
        flow_version: 2,
        code_hash: [42; 32],
        snapshot_id: 1,
    }
}

fn admission(run_id: &str) -> LiveRun {
    LiveRun {
        run_id: run_id.into(),
        flow_id: "flow".into(),
        flow_version: 2,
        trigger_id: "fire-9".into(),
        trigger_kind: "schedule".into(),
        trigger: json!({"scheduled_at": 100}),
        attempt: 1,
        admitted_at: 105,
        admit_seq: 9,
        admit_lsn: EpochLsn {
            log_epoch: 7,
            lsn: Lsn {
                sequence: 3,
                offset: 512,
            },
        },
        deadline_ms: 60000,
        self_input: json!({"instance_id":"instance"}),
        params_input: json!({"limit":2}),
        grant_guards: json!([{"grant":"g"}]),
        template_incarnation: Some(5),
        end_instance: true,
    }
}

fn summary(run_id: &str) -> RunHistory {
    RunHistory {
        run_id: run_id.into(),
        flow_id: "flow".into(),
        flow_version: 2,
        trigger_id: "fire-9".into(),
        trigger_kind: "schedule".into(),
        trigger: json!({"scheduled_at":100}),
        admitted_at: 105,
        admit_seq: 9,
        first_claim_at: Some(110),
        ended_at: 200,
        state: RunState::Failed,
        error_kind: Some("worker".into()),
        error_detail: Some("stopped".into()),
        result: Some(json!({"partial":true})),
        result_size: 16,
        call_counts: json!({"class":{"query":2,"mutation":1},"outcome":{"fulfilled":2,"unknown":1}}),
        writes_sent: 1,
        tokens_charged: 100,
        cost_microusd: 2500,
        broken: 2,
        activation_count: 3,
        script_started: true,
        phase: "script".into(),
        deadline_exempt: true,
        history_lost: false,
        commit_seq: 21,
    }
}

fn late(run_id: &str, position: u64) -> Obligation {
    Obligation {
        run_id: run_id.into(),
        position,
        handle: Some("host-handle".into()),
        state: ObligationState::Open,
        audit_at: 130,
        detail: json!({"frozen_request":{"x":1},"reservation":{"tokens":25},
            "window":100,"usage":null,"cost":null}),
        receipt: None,
        settled_at: None,
    }
}

fn detail(run_id: &str) -> RunCall {
    RunCall {
        run_id: run_id.into(),
        position: 4,
        kind: CallKind::Op {
            module: "remote".into(),
            op: "read".into(),
        },
        class: StoredClass::Query,
        args_digest: [23; 32],
        outcome: CallOutcome::Rejected,
        error_code: Some("unavailable".into()),
        issued_at: 120,
        arrived_at: Some(130),
        released_at: Some(140),
        detail: json!({"args":{"x":"é"},"frozen_request":{"x":1},
            "value":{"error":"unavailable"},"handle":"host-handle","usage":{"tokens":5},
            "cost":2,"refusal_detail":"provider unavailable","delivery_order":3,
            "idempotency_key":"key","attempts":2,"dispatch":"accepted"}),
    }
}

fn finished(conn: &mut Connection, run_id: &str) {
    let tx = conn.transaction().unwrap();
    insert_history(&tx, &summary(run_id), &id(run_id)).unwrap();
    tx.commit().unwrap();
}

#[test]
fn production_open_keeps_the_legacy_schema_and_meta() {
    let dir = crate::test_support::timestamp_scratch("run-store-sql", "production");
    let store = Store::open(dir.join("core.db"), Durability::throwaway()).unwrap();
    store
        .read(|conn| {
            for name in [
                "live_runs",
                "run_history",
                "run_identities",
                "obligations",
                "run_calls",
                "run_log_segments",
            ] {
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                    [name],
                    |r| r.get(0),
                )?;
                assert!(!exists, "production created {name}");
            }
            for name in [
                "runs",
                "journal",
                "journal_payloads",
                "mailbox",
                "run_snapshots",
                "broca_calls",
            ] {
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                    [name],
                    |r| r.get(0),
                )?;
                assert!(exists, "production removed {name}");
            }
            let keys: i64 = conn.query_row(
                "SELECT COUNT(*) FROM meta WHERE key IN
            ('log_epoch','log_hwm','commit_seq','gc_floor','migration_phase')",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(keys, 0);
            assert!(!meta_value(conn, "store_id")?.is_empty());
            let broca_sql: String = conn.query_row(
                "SELECT sql FROM sqlite_schema WHERE name='broca_calls'",
                [],
                |r| r.get(0),
            )?;
            assert!(broca_sql.contains("REFERENCES journal"));
            Ok(())
        })
        .unwrap();
    assert!(!dir.join("run-log").exists());
    assert_eq!(MIGRATIONS.last().unwrap().version, 49);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn identity_and_admission_roundtrip_use_the_full_id() {
    let mut conn = fixture();
    let run_id = "run-store-7-random";
    let tx = conn.transaction().unwrap();
    insert_live(&tx, &admission(run_id), &id(run_id)).unwrap();
    tx.commit().unwrap();
    assert_eq!(identity(&conn, run_id).unwrap(), Some(id(run_id)));
    assert_eq!(live(&conn, run_id).unwrap(), Some(admission(run_id)));
    assert_eq!(identity(&conn, "run-store-7").unwrap(), None);
    assert_eq!(live(&conn, "run-store-7").unwrap(), None);
    let tx = conn.transaction().unwrap();
    let mut mismatch = id("different");
    mismatch.code_hash = [8; 32];
    assert!(matches!(
        insert_live(&tx, &admission("different"), &mismatch),
        Err(CoreError::Invalid(_))
    ));
    assert!(insert_live(&tx, &admission(run_id), &id(run_id)).is_err());
    let mut orphan = admission("missing");
    let mut missing = id("missing");
    missing.snapshot_id = 999;
    assert!(insert_live(&tx, &orphan, &missing).is_err());
    orphan.run_id = run_id.into();
    orphan.flow_version = 3;
    assert!(insert_live(&tx, &orphan, &id(run_id)).is_err());
}

#[test]
fn terminal_summary_replaces_admission_and_retains_identity() {
    let mut conn = fixture();
    let tx = conn.transaction().unwrap();
    insert_live(&tx, &admission("run"), &id("run")).unwrap();
    assert_eq!(next_commit_seq(&tx).unwrap(), 21);
    finish_live(&tx, &summary("run")).unwrap();
    tx.commit().unwrap();
    assert_eq!(live(&conn, "run").unwrap(), None);
    assert_eq!(identity(&conn, "run").unwrap(), Some(id("run")));
    assert_eq!(history(&conn, "run").unwrap(), Some(summary("run")));
    assert_eq!(history(&conn, "other").unwrap(), None);
    let tx = conn.transaction().unwrap();
    assert!(matches!(
        finish_live(&tx, &summary("missing")),
        Err(CoreError::NoSuchRun(_))
    ));
}

#[test]
fn staged_schema_preserves_control_rows_and_uses_payload_appropriate_btrees() {
    let mut conn = legacy();
    conn.execute_batch(
        "INSERT INTO trigger_inbox VALUES('flow','trigger',1,'run',10);
        INSERT INTO tombstones VALUES('trigger','key','run',20);
        INSERT INTO rate_windows VALUES('flow',100,0,3,4,1);
        INSERT INTO schedules VALUES('flow',1,'{}','active',0,100,NULL,0);
        INSERT INTO schedule_fires VALUES(17,'flow','fire',1,100,'{}',0);
        INSERT INTO schedule_fires VALUES(21,'flow','consumed',1,200,'{}',0);
        DELETE FROM schedule_fires WHERE seq=21;",
    )
    .unwrap();
    let tx = conn.transaction().unwrap();
    create_test_schema(&tx, &initial_meta()).unwrap();
    tx.commit().unwrap();
    for (table, columns, expected) in [
        (
            "trigger_inbox",
            "json_array(flow_id,trigger_id,attempt,run_id,admitted_at)",
            "[\"flow\",\"trigger\",1,\"run\",10]",
        ),
        (
            "tombstones",
            "json_array(kind,key,run_id,pruned_at)",
            "[\"trigger\",\"key\",\"run\",20]",
        ),
        (
            "rate_windows",
            "json_array(flow_id,window_ms,window_start,runs,dispatches,saturated)",
            "[\"flow\",100,0,3,4,1]",
        ),
        (
            "schedules",
            "json_array(flow_id,version,spec,state,anchor_ms,next_due_ms,last_fired_ms,updated_at)",
            "[\"flow\",1,\"{}\",\"active\",0,100,null,0]",
        ),
        (
            "schedule_fires",
            "json_array(seq,flow_id,trigger_id,version,due_ms,payload,planned_at)",
            "[17,\"flow\",\"fire\",1,100,\"{}\",0]",
        ),
    ] {
        assert_eq!(
            conn.query_row(&format!("SELECT {columns} FROM {table}"), [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            expected,
            "{table}"
        );
    }
    assert_eq!(meta_value(&conn, "next_run").unwrap(), "22");
    assert_eq!(
        conn.query_row(
            "SELECT type FROM sqlite_schema WHERE name='run_identities'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "view"
    );
    for (table, without_rowid) in [
        ("live_runs", true),
        ("trigger_inbox", true),
        ("tombstones", true),
        ("rate_windows", true),
        ("schedules", true),
        ("run_log_segments", true),
        ("run_history", false),
        ("run_calls", false),
        ("obligations", false),
        ("schedule_fires", false),
        ("run_snapshots", false),
    ] {
        assert_eq!(
            conn.query_row(
                "SELECT wr FROM pragma_table_list WHERE name=?1",
                [table],
                |r| r.get::<_, bool>(0)
            )
            .unwrap(),
            without_rowid,
            "{table}"
        );
    }
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_sequence WHERE name='schedule_fires'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn call_detail_roundtrips_only_for_non_successful_available_history() {
    let mut conn = fixture();
    for (i, state) in [
        RunState::Failed,
        RunState::EngineMismatch,
        RunState::Cancelled,
    ]
    .into_iter()
    .enumerate()
    {
        let run_id = format!("failed-{i}");
        let tx = conn.transaction().unwrap();
        let mut history = summary(&run_id);
        history.state = state;
        insert_history(&tx, &history, &id(&run_id)).unwrap();
        insert_call(&tx, &detail(&run_id)).unwrap();
        tx.commit().unwrap();
        assert_eq!(call(&conn, &run_id, 4).unwrap(), Some(detail(&run_id)));
        let listed: (String, String, String, String, String, i64) = conn.query_row(
            "SELECT class,module,op,outcome,error_code,issued_at FROM run_calls WHERE run_id=?1",
            [&run_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        assert_eq!(
            listed,
            (
                "query".into(),
                "remote".into(),
                "read".into(),
                "rejected".into(),
                "unavailable".into(),
                120
            )
        );
    }
    assert_eq!(call(&conn, "failed-0", 999).unwrap(), None);
    let tx = conn.transaction().unwrap();
    for (run_id, state, lost) in [
        ("success", RunState::Succeeded, false),
        ("lost", RunState::Cancelled, true),
    ] {
        let mut row = summary(run_id);
        row.state = state;
        row.history_lost = lost;
        row.result = None;
        insert_history(&tx, &row, &id(run_id)).unwrap();
        assert!(matches!(
            insert_call(&tx, &detail(run_id)),
            Err(CoreError::Invalid(_))
        ));
        assert_eq!(history(&tx, run_id).unwrap().unwrap().result, None);
    }
}

#[test]
fn settled_receipts_survive_redelivery_without_changes() {
    let mut conn = fixture();
    finished(&mut conn, "run");
    let tx = conn.transaction().unwrap();
    insert_obligation(&tx, &late("run", 4)).unwrap();
    tx.commit().unwrap();
    assert_eq!(obligation(&conn, "run", 4).unwrap(), Some(late("run", 4)));
    let tx = conn.transaction().unwrap();
    assert_eq!(
        settle_obligation(
            &tx,
            "run",
            4,
            Some("host-handle"),
            Receipt::Value([7; 32]),
            210
        )
        .unwrap(),
        SettleResult::Settled
    );
    tx.commit().unwrap();
    let mut expected = late("run", 4);
    expected.state = ObligationState::Settled;
    expected.receipt = Some(Receipt::Value([7; 32]));
    expected.settled_at = Some(210);
    expected.detail = Value::Null;
    assert_eq!(obligation(&conn, "run", 4).unwrap(), Some(expected.clone()));
    let tx = conn.transaction().unwrap();
    for (handle, receipt, result) in [
        (
            Some("host-handle"),
            Receipt::Value([7; 32]),
            SettleResult::Duplicate,
        ),
        (
            Some("host-handle"),
            Receipt::Unknown,
            SettleResult::Duplicate,
        ),
        (
            Some("host-handle"),
            Receipt::Value([8; 32]),
            SettleResult::Conflict,
        ),
        (
            Some("other-handle"),
            Receipt::Value([7; 32]),
            SettleResult::Conflict,
        ),
        (None, Receipt::Value([7; 32]), SettleResult::Conflict),
    ] {
        assert_eq!(
            settle_obligation(&tx, "run", 4, handle, receipt, 999).unwrap(),
            result
        );
    }
    assert!(!mark_obligation_unknown(&tx, "run", 4).unwrap());
    assert_eq!(
        settle_obligation(&tx, "run", 99, None, Receipt::Unknown, 999).unwrap(),
        SettleResult::Missing
    );
    tx.commit().unwrap();
    assert_eq!(obligation(&conn, "run", 4).unwrap(), Some(expected));
}

#[test]
fn unknown_obligation_retains_an_unknown_settlement_receipt() {
    let mut conn = fixture();
    finished(&mut conn, "run");
    let tx = conn.transaction().unwrap();
    let mut row = late("run", 0);
    row.handle = None;
    insert_obligation(&tx, &row).unwrap();
    assert!(mark_obligation_unknown(&tx, "run", 0).unwrap());
    row.state = ObligationState::Unknown;
    assert_eq!(obligation(&tx, "run", 0).unwrap(), Some(row.clone()));
    assert_eq!(
        settle_obligation(&tx, "run", 0, None, Receipt::Unknown, 220).unwrap(),
        SettleResult::Settled
    );
    tx.commit().unwrap();
    row.state = ObligationState::Settled;
    row.receipt = Some(Receipt::Unknown);
    row.settled_at = Some(220);
    row.detail = Value::Null;
    assert_eq!(obligation(&conn, "run", 0).unwrap(), Some(row));
    let tx = conn.transaction().unwrap();
    assert_eq!(
        settle_obligation(&tx, "run", 0, None, Receipt::Unknown, 230).unwrap(),
        SettleResult::Duplicate
    );
    assert_eq!(
        settle_obligation(&tx, "run", 0, None, Receipt::Value([1; 32]), 230).unwrap(),
        SettleResult::Conflict
    );
}

#[test]
fn history_pruning_removes_receipts_calls_and_identity_together() {
    let mut conn = fixture();
    finished(&mut conn, "run");
    let tx = conn.transaction().unwrap();
    insert_obligation(&tx, &late("run", 4)).unwrap();
    insert_call(&tx, &detail("run")).unwrap();
    assert!(!prune_history(&tx, "run").unwrap());
    assert_eq!(
        settle_obligation(
            &tx,
            "run",
            4,
            Some("host-handle"),
            Receipt::Value([9; 32]),
            220
        )
        .unwrap(),
        SettleResult::Settled
    );
    tx.commit().unwrap();
    assert!(identity(&conn, "run").unwrap().is_some());
    assert!(obligation(&conn, "run", 4).unwrap().is_some());
    let tx = conn.transaction().unwrap();
    assert!(prune_history(&tx, "run").unwrap());
    tx.commit().unwrap();
    assert_eq!(history(&conn, "run").unwrap(), None);
    assert_eq!(identity(&conn, "run").unwrap(), None);
    assert_eq!(obligation(&conn, "run", 4).unwrap(), None);
    assert_eq!(call(&conn, "run", 4).unwrap(), None);
}

#[test]
fn metadata_roundtrips_and_normal_updates_are_monotonic_and_epoch_qualified() {
    let mut conn = fixture();
    let initial = initial_meta();
    assert_eq!(meta(&conn).unwrap(), initial);
    let tx = conn.transaction().unwrap();
    let newer = EpochLsn {
        log_epoch: 7,
        lsn: Lsn {
            sequence: 4,
            offset: 100,
        },
    };
    advance_log_hwm(&tx, newer).unwrap();
    advance_log_hwm(&tx, initial.log_hwm).unwrap();
    assert_eq!(next_commit_seq(&tx).unwrap(), 21);
    assert_eq!(next_commit_seq(&tx).unwrap(), 22);
    assert!(
        advance_log_hwm(
            &tx,
            EpochLsn {
                log_epoch: 8,
                ..newer
            }
        )
        .is_err()
    );
    tx.commit().unwrap();
    let expected = Meta {
        log_hwm: newer,
        commit_seq: 22,
        ..initial
    };
    assert_eq!(meta(&conn).unwrap(), expected);
    let tx = conn.transaction().unwrap();
    let inconsistent = Meta {
        log_epoch: 8,
        ..expected.clone()
    };
    assert!(write_meta(&tx, &inconsistent).is_err());
    assert_eq!(meta(&tx).unwrap(), expected);
}

#[test]
fn segments_reserve_promote_and_reclaim_in_epoch_order() {
    let mut conn = fixture();
    let tx = conn.transaction().unwrap();
    reserve_segment(&tx, 7, 4).unwrap();
    reserve_segment(&tx, 7, 3).unwrap();
    reserve_segment(&tx, 6, 4).unwrap();
    assert!(reserve_segment(&tx, 7, 4).is_err());
    tx.commit().unwrap();
    assert_eq!(
        segments(&conn, 7).unwrap(),
        vec![
            Segment {
                log_epoch: 7,
                seq: 3,
                state: SegmentState::Reserved
            },
            Segment {
                log_epoch: 7,
                seq: 4,
                state: SegmentState::Reserved
            },
        ]
    );
    let tx = conn.transaction().unwrap();
    let watermark = EpochLsn {
        log_epoch: 7,
        lsn: Lsn {
            sequence: 4,
            offset: 256,
        },
    };
    assert!(promote_segment(&tx, watermark).unwrap());
    assert!(promote_segment(&tx, watermark).unwrap());
    assert!(
        !promote_segment(
            &tx,
            EpochLsn {
                lsn: Lsn {
                    sequence: 99,
                    offset: 0
                },
                ..watermark
            }
        )
        .unwrap()
    );
    tx.commit().unwrap();
    assert_eq!(segments(&conn, 7).unwrap()[1].state, SegmentState::Active);
    assert_eq!(segments(&conn, 6).unwrap()[0].state, SegmentState::Reserved);
    assert_eq!(meta(&conn).unwrap().log_hwm, watermark);
    let tx = conn.transaction().unwrap();
    let mut state = meta(&tx).unwrap();
    state.gc_floor.first_retained = 4;
    state.gc_floor.predecessor = Some(SegmentEnd {
        lsn: Lsn {
            sequence: 3,
            offset: 8192,
        },
        crc: 5678,
    });
    state.gc_floor.synced_lsn = watermark.lsn;
    write_meta(&tx, &state).unwrap();
    assert_eq!(remove_segments_before(&tx, 7, 4).unwrap(), 1);
    tx.commit().unwrap();
    assert_eq!(
        segments(&conn, 7).unwrap(),
        vec![Segment {
            log_epoch: 7,
            seq: 4,
            state: SegmentState::Active
        }]
    );
    assert_eq!(segments(&conn, 6).unwrap().len(), 1);
    assert_eq!(meta(&conn).unwrap(), state);
}

#[test]
fn a_rolled_back_terminal_bundle_keeps_admission_and_watermark() {
    let mut conn = fixture();
    let tx = conn.transaction().unwrap();
    insert_live(&tx, &admission("run"), &id("run")).unwrap();
    tx.commit().unwrap();
    {
        let tx = conn.transaction().unwrap();
        finish_live(&tx, &summary("run")).unwrap();
        insert_call(&tx, &detail("run")).unwrap();
        insert_obligation(&tx, &late("run", 4)).unwrap();
        next_commit_seq(&tx).unwrap();
        advance_log_hwm(
            &tx,
            EpochLsn {
                log_epoch: 7,
                lsn: Lsn {
                    sequence: 9,
                    offset: 300,
                },
            },
        )
        .unwrap();
        tx.rollback().unwrap();
    }
    assert_eq!(live(&conn, "run").unwrap(), Some(admission("run")));
    assert_eq!(identity(&conn, "run").unwrap(), Some(id("run")));
    assert_eq!(history(&conn, "run").unwrap(), None);
    assert_eq!(call(&conn, "run", 4).unwrap(), None);
    assert_eq!(obligation(&conn, "run", 4).unwrap(), None);
    assert_eq!(meta(&conn).unwrap(), initial_meta());
}

#[test]
fn schema_rejects_incomplete_receipts_invalid_history_and_overflow() {
    let mut conn = fixture();
    finished(&mut conn, "run");
    let tx = conn.transaction().unwrap();
    let mut receipt = late("run", 4);
    receipt.state = ObligationState::Settled;
    assert!(insert_obligation(&tx, &receipt).is_err());
    receipt.settled_at = Some(200);
    assert!(insert_obligation(&tx, &receipt).is_err());
    receipt.receipt = Some(Receipt::Value([5; 32]));
    insert_obligation(&tx, &receipt).unwrap();
    assert!(
        tx.execute(
            "INSERT INTO obligations(run_id,position,state,audit_at,detail,value_hash,receipt_unknown,settled_at) VALUES('run',99,'settled',100,'{}',zeroblob(32),1,200)",
            []
        )
        .is_err()
    );
    assert!(
        tx.execute("DELETE FROM run_snapshots WHERE snapshot_id=1", [])
            .is_err()
    );
    assert!(
        tx.execute("UPDATE run_history SET run_id=NULL WHERE run_id='run'", [])
            .is_err()
    );
    assert!(insert_obligation(&tx, &late("missing", 1)).is_err());
    assert!(reserve_segment(&tx, 7, u64::MAX).is_err());
    let mut row = summary("invalid");
    row.state = RunState::Running;
    assert!(insert_history(&tx, &row, &id("invalid")).is_err());
    row.state = RunState::Failed;
    row.error_detail = Some("é".repeat(4097));
    assert!(insert_history(&tx, &row, &id("invalid")).is_err());
    row.error_detail = Some("é".repeat(4096));
    row.result = Some(Value::Null);
    insert_history(&tx, &row, &id("invalid")).unwrap();
    assert_eq!(history(&tx, "invalid").unwrap(), Some(row));
    assert!(
        tx.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}

#[test]
fn settled_receipts_share_history_and_late_settlement_moves_atomically() {
    let mut conn = fixture();
    finished(&mut conn, "run");
    let open = late("run", 7);
    let tx = conn.transaction().unwrap();
    insert_obligation(&tx, &open).unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    assert_eq!(
        settle_obligation(
            &tx,
            "run",
            7,
            open.handle.as_deref(),
            Receipt::Value([7; 32]),
            300
        )
        .unwrap(),
        SettleResult::Settled
    );
    assert_eq!(
        tx.query_row(
            "SELECT count(*) FROM obligations WHERE run_id='run'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        tx.query_row(
            "SELECT count(*) FROM run_history, json_each(receipts) WHERE run_id='run'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    tx.rollback().unwrap();
    assert_eq!(obligation(&conn, "run", 7).unwrap(), Some(open.clone()));
    assert_eq!(
        conn.query_row(
            "SELECT receipts FROM run_history WHERE run_id='run'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "{}"
    );

    let tx = conn.transaction().unwrap();
    settle_obligation(
        &tx,
        "run",
        7,
        open.handle.as_deref(),
        Receipt::Value([7; 32]),
        300,
    )
    .unwrap();
    tx.commit().unwrap();
    let mut expected = open;
    expected.state = ObligationState::Settled;
    expected.receipt = Some(Receipt::Value([7; 32]));
    expected.settled_at = Some(300);
    expected.detail = Value::Null;
    assert_eq!(obligation(&conn, "run", 7).unwrap(), Some(expected));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM obligations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let tx = conn.transaction().unwrap();
    assert!(prune_history(&tx, "run").unwrap());
    tx.commit().unwrap();
    assert_eq!(obligation(&conn, "run", 7).unwrap(), None);
}
