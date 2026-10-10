use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use basal_host::{MockCatalog, mock::MockHost};
use basal_proto::{CallKind, Primitive};
use rusqlite::{Connection, params};
use serde_json::json;

use crate::*;

pub(crate) fn connection() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    for migration in schema::MIGRATIONS {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn
}

pub(crate) struct TestRuntime {
    pub rt: Runtime,
    // Declared after the runtime so it drops after it: on Windows an open
    // file blocks deleting its directory, so the store must close first.
    _dir: ScratchDir,
}

/// Removes the runtime's scratch directory when dropped.
struct ScratchDir(std::path::PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

impl TestRuntime {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "basal-regression-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let store = Arc::new(
            Store::open(path.join("store.sqlite"), Durability { fullfsync: false }).unwrap(),
        );
        let rt = Runtime::new(
            store,
            Arc::new(MockHost::new()),
            Arc::new(MockCatalog::standard()),
            Arc::new(NoHooks),
            None,
            Config::default(),
        );
        Self {
            rt,
            _dir: ScratchDir(path),
        }
    }
}

#[test]
fn facts_include_must_be_an_array() {
    let manifest = Manifest::parse(
        &json!({
            "id":"facts", "version":1, "purpose":"test", "trigger":{"schedule":{"interval":"1m"}},
            "facts":{"targets":["ALF"], "text":false}
        })
        .to_string(),
    )
    .unwrap();
    for include in [
        json!("text"),
        json!(null),
        json!(true),
        json!(1),
        json!({"text":true}),
    ] {
        let result = authorize::check(
            &manifest,
            &ShellDenylist::default(),
            &MockCatalog::standard(),
            &CallKind::Primitive(Primitive::Facts),
            &json!({"agent":"ALF","options":{"include":include}}),
        );
        assert_eq!(
            result.unwrap_err().code,
            authorize::codes::INVALID_ARGUMENTS
        );
    }
    for include in [json!([]), json!(["claims"])] {
        assert!(
            authorize::check(
                &manifest,
                &ShellDenylist::default(),
                &MockCatalog::standard(),
                &CallKind::Primitive(Primitive::Facts),
                &json!({"agent":"ALF","options":{"include":include}})
            )
            .is_ok()
        );
    }
}

#[test]
fn instance_collision_cannot_change_a_regular_flow() {
    let fixture = TestRuntime::new();
    let rt = &fixture.rt;
    rt.register_package("return 1", &json!({"id":"package","version":1,"purpose":"test","trigger":{"schedule":{"interval":"1m"}}}).to_string()).unwrap();
    let id = packages::instance_id("package", "ALF");
    rt.store().write(|tx| {
        tx.execute("INSERT INTO flows(flow_id,owner,approved_version,created_at) VALUES (?1,'operator',99,1)", [&id])?;
        Ok(())
    }).unwrap();
    let reply = rt.ensure_instance("package", 1, "ALF", 1);
    assert!(
        matches!(
            reply,
            Err(packages::PackageError::Refused {
                code: "instance_id_conflict",
                ..
            })
        ),
        "{reply:?}"
    );
    rt.store()
        .read(|conn| {
            assert_eq!(
                conn.query_row(
                    "SELECT approved_version FROM flows WHERE flow_id=?1",
                    [&id],
                    |r| r.get::<_, i64>(0)
                )?,
                99
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM instance_generations", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn huge_usage_settles_and_keeps_window_totals_integral() {
    let mut conn = connection();
    let tx = conn.transaction().unwrap();
    tx.execute("INSERT INTO token_windows(flow_id,window_ms,window_start,reserved) VALUES ('f',60000,0,20)", []).unwrap();
    for (p, key) in [(0, "a"), (1, "b")] {
        tx.execute("INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,reserved_at) VALUES (?1,'f','r',?2,60000,0,10,'reserved',0)", params![key,p]).unwrap();
        assert!(
            tokens::settle(
                &tx,
                tokens::Key::SendId(key),
                tokens::Report::Usage(tokens::Usage {
                    input_tokens: u64::MAX,
                    output_tokens: u64::MAX,
                    cache_write_tokens: u64::MAX,
                    cached_input_tokens: u64::MAX
                }),
                1
            )
            .unwrap()
        );
    }
    let totals: (i64,i64,i64,i64,i64) = tx.query_row("SELECT reserved,input_tokens,cache_write_tokens,output_tokens,cached_input_tokens FROM token_windows", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(totals, (0, i64::MAX, i64::MAX, i64::MAX, i64::MAX));
}

#[test]
fn health_does_not_hide_a_store_error() {
    let conn = connection();
    conn.execute("DROP TABLE meta", []).unwrap();
    assert!(ops::health(&conn).is_err());
}

#[test]
fn decision_prompts_identify_the_calendar_day() {
    for ms in [0, 86_400_000] {
        let context = basal_host::DecisionContext::Reconcile {
            run_id: "r".into(),
            run_admitted_at_ms: ms,
            step: None,
            call_key: "k".into(),
            op: "mock.send".into(),
            attempts: 1,
            unknown_reason: basal_host::UnknownReason::ReplyTimeout,
        };
        let expected = if ms == 0 {
            "1970-01-01 00:00 UTC"
        } else {
            "1970-01-02 00:00 UTC"
        };
        assert!(decisions::prompt("f", &context).contains(expected));
    }
}

#[test]
fn corrupt_schedule_does_not_block_healthy_schedules() {
    let fixture = TestRuntime::new();
    fixture.rt.store().write(|tx| {
        for id in ["bad", "good"] {
            tx.execute("INSERT INTO schedules(flow_id,version,spec,state,anchor_ms,next_due_ms,updated_at) VALUES (?1,1,?2,'active',0,60000,0)", params![id, if id == "bad" {"{}"} else {r#"{"interval":"1m"}"#}])?;
        }
        Ok(())
    }).unwrap();
    let now = jiff::Timestamp::from_millisecond(60000).unwrap();
    let scheduler = schedule::Scheduler::new(
        fixture.rt.shared.store.clone(),
        Arc::new(schedule::ManualClock::new(now)),
        schedule::SchedulerConfig::default(),
    );
    let report = scheduler.tick().unwrap();
    assert_eq!(report.planned.len(), 1);
    assert_eq!(report.planned[0].flow_id, "good");
    fixture
        .rt
        .store()
        .read(|conn| {
            assert_eq!(
                conn.query_row("SELECT state FROM schedules WHERE flow_id='bad'", [], |r| r
                    .get::<_, String>(0))?,
                "disabled"
            );
            assert_eq!(
                conn.query_row(
                    "SELECT COUNT(*) FROM audit WHERE action='schedule.corrupt'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                1
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn corrupt_oldest_fire_does_not_block_healthy_fires() {
    let fixture = TestRuntime::new();
    fixture.rt.store().write(|tx| {
        let oversized = "x".repeat(basal_proto::MAX_VALUE_BYTES + 1);
        for (id,payload) in [("bad",oversized.as_str()),("good","{}") ] {
            tx.execute("INSERT INTO schedule_fires(flow_id,trigger_id,version,due_ms,payload,planned_at) VALUES (?1,?1,1,60000,?2,0)", params![id,payload])?;
        }
        Ok(())
    }).unwrap();
    let now = jiff::Timestamp::from_millisecond(60000).unwrap();
    let scheduler = schedule::Scheduler::new(
        fixture.rt.shared.store.clone(),
        Arc::new(schedule::ManualClock::new(now)),
        schedule::SchedulerConfig::default(),
    );
    let report = scheduler.tick().unwrap();
    assert_eq!(report.waiting, 0);
    assert_eq!(report.admitted.len(), 1);
    assert_eq!(report.admitted[0].flow_id, "good");
    fixture
        .rt
        .store()
        .read(|conn| {
            assert_eq!(
                conn.query_row(
                    "SELECT reason FROM schedule_dropped WHERE flow_id='bad'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "not_approved"
            );
            Ok(())
        })
        .unwrap();
}
