use super::*;
use crate::{Clock, Config, Durability, InstallGate, InstallRequest, Store};
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, MockCatalog, OpDecl,
    OpKind, TransportError,
};
use basal_proto::{CallKind, Settlement};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
const BODY: &str = "{\"changed\": true}";

struct Source {
    replies: Mutex<Vec<std::result::Result<Dispatched, TransportError>>>,
    calls: Mutex<Vec<CallRequest>>,
    on_call: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl Host for Source {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, request: &CallRequest) -> std::result::Result<Dispatched, TransportError> {
        self.calls.lock().unwrap().push(request.clone());
        if let Some(on_call) = self.on_call.lock().unwrap().take() {
            on_call();
        }
        self.replies.lock().unwrap().remove(0)
    }
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.5
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
}
struct Fixture {
    rt: Runtime,
    source: Arc<Source>,
    catalog: MockCatalog,
    path: std::path::PathBuf,
}
impl Fixture {
    fn new(max_runs: u32) -> Self {
        let path = std::env::temp_dir().join(format!(
            "basal-events-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let store =
            Arc::new(Store::open(path.join("basal.db"), Durability { fullfsync: false }).unwrap());
        let source = Arc::new(Source {
            replies: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            on_call: Mutex::new(None),
        });
        let catalog = MockCatalog::new();
        catalog.set_event(
            "plexus",
            "github_pr_changed",
            1,
            basal_host::EventDecl {
                origin: basal_host::EventOrigin::External,
                body: basal_host::EventBody::Resolved {
                    resolve_op: basal_host::catalog::EVENT_BODY_OP.into(),
                },
            },
        );
        catalog.set_op(
            "plexus",
            basal_host::catalog::EVENT_BODY_OP,
            OpDecl {
                kind: Some(OpKind::Query),
                cause_echo: false,
                shell_capable: false,
            },
        );
        let rt = Runtime::new(
            store,
            source.clone(),
            Arc::new(catalog.clone()),
            Arc::new(crate::NoHooks),
            None,
            Config {
                clock: Clock::manual(1_000),
                install_gate: InstallGate::Off,
                rate: crate::RateLimits {
                    max_runs,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        Self {
            rt,
            source,
            catalog,
            path,
        }
    }
    fn request(&self, id: &str) -> InstallRequest {
        InstallRequest { script: "return trigger.event.body;".into(), author: "local:unverified".into(), loop_override: false, manifest: json!({"id":id,"version":1,"purpose":"Receive a module event", "trigger":{"events":[{"module":"plexus","name":"github_pr_changed","version":1}]}, "ops":[{"module":"plexus","op":basal_host::catalog::EVENT_BODY_OP}]}).to_string() }
    }
    fn approve(&self, id: &str) {
        let installed = self.rt.install(&self.request(id)).unwrap();
        self.rt
            .approve(id, 1, &installed.code_hash, "test-approved")
            .unwrap();
    }
    fn run(&self) -> crate::Run {
        self.approve("flow");
        self.rt.admit_events(&[notice(0)]).unwrap();
        let id: String = self
            .rt
            .store()
            .read(|c| Ok(c.query_row("SELECT run_id FROM runs", [], |r| r.get(0))?))
            .unwrap();
        self.rt.run(&id).unwrap()
    }
    fn reply(&self, value: serde_json::Value, settlement: Settlement) {
        self.source
            .replies
            .lock()
            .unwrap()
            .push(Ok(Dispatched::Completed(HostOutcome {
                settlement,
                value: JsonText::new(value.to_string()).unwrap(),
                usage: None,
            })));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
fn notice(n: u64) -> Notice {
    use sha2::{Digest, Sha256};
    Notice {
        published_at_ms: 1_000,
        subject: "ck.account.event.plexus.github_pr_changed.v1".into(),
        event_key: format!("{n:064x}"),
        digest: format!("{:x}", Sha256::digest(BODY.as_bytes())),
        headers: BTreeMap::from([("repo".into(), "owner/repo".into())]),
    }
}
fn reply() -> serde_json::Value {
    json!({"result":{"body":BODY,"digest":notice(0).digest,"event_key":notice(0).event_key,"event_name":"github_pr_changed"}})
}

#[test]
fn fanout_commits_atomically_and_only_matches_enabled_approved_flows() {
    let f = Fixture::new(60);
    f.approve("a");
    f.approve("b");
    f.approve("disabled");
    f.rt.disable_flow("disabled", &crate::Actor::Runtime, "test")
        .unwrap();
    f.rt.store().write(|tx| { tx.execute_batch("CREATE TRIGGER refuse_second BEFORE INSERT ON runs WHEN NEW.flow_id='b' BEGIN SELECT RAISE(ABORT,'second flow failed'); END;")?; Ok(()) }).unwrap();
    assert!(f.rt.admit_events(&[notice(0)]).is_err());
    let count: i64 =
        f.rt.store()
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?))
            .unwrap();
    assert_eq!(count, 0, "fan-out cannot partially commit");
    f.rt.store()
        .write(|tx| {
            tx.execute_batch("DROP TRIGGER refuse_second")?;
            Ok(())
        })
        .unwrap();
    assert_eq!(f.rt.admit_events(&[notice(0)]).unwrap().admitted, 2);
    assert_eq!(f.rt.admit_events(&[notice(0)]).unwrap().duplicate, 2);
    let mut other = notice(1);
    other.subject = "ck.account.event.plexus.github_pr_changed.v2".into();
    assert_eq!(f.rt.admit_events(&[other]).unwrap().matched, 0);
}

#[test]
fn backlog_is_bounded_overflow_is_reported_and_oldest_runs_first() {
    let f = Fixture::new(1);
    f.approve("flow");
    let notices: Vec<_> = (0..123).map(notice).collect();
    let report = f.rt.admit_events(&notices).unwrap();
    assert_eq!(
        (report.admitted, report.queued, report.overflow),
        (1, 120, 2)
    );
    assert_eq!(f.rt.admit_events(&notices).unwrap().duplicate, 123);
    let health = f.rt.flow_health().unwrap();
    assert_eq!(
        (health[0].event_backlog, health[0].event_overflow),
        (120, 2)
    );
    f.rt.config.clock.set(61_000);
    assert_eq!(f.rt.drain_event_backlog().unwrap(), 1);
    let trigger: String =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT trigger_id FROM runs ORDER BY admit_seq DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    assert_eq!(trigger, notice(1).trigger_id());
    f.rt.config.clock.set(121_000);
    f.rt.drain_event_backlog().unwrap();
    assert!(
        !f.rt.flow("flow").unwrap().unwrap().enabled,
        "three saturated windows auto-disable"
    );
}

#[test]
fn receipts_keep_eight_days_and_prune_by_index_with_tombstone_dedup() {
    let f = Fixture::new(60);
    f.approve("flow");
    f.rt.admit_events(&[notice(0)]).unwrap();
    let id: String =
        f.rt.store()
            .read(|c| Ok(c.query_row("SELECT run_id FROM runs", [], |r| r.get(0))?))
            .unwrap();
    f.rt.cancel(&id, "test").unwrap();
    f.rt.prune(2_000, 0).unwrap();
    assert_eq!(
        f.rt.store()
            .write(|tx| prune_receipts(tx, 1_000 + 8 * 24 * 60 * 60 * 1000 - 1))
            .unwrap(),
        0
    );
    assert_eq!(
        f.rt.store()
            .write(|tx| prune_receipts(tx, 1_000 + 8 * 24 * 60 * 60 * 1000))
            .unwrap(),
        1
    );
    assert_eq!(f.rt.admit_events(&[notice(0)]).unwrap().duplicate, 1);
    let plans: Vec<String> = f.rt.store().read(|c| Ok(c.prepare("EXPLAIN QUERY PLAN SELECT rowid FROM event_receipts WHERE state<>'backlog' AND received_at<=?1 ORDER BY +rowid LIMIT 128")?.query_map([1], |r| r.get(3))?.collect::<rusqlite::Result<_>>()?)).unwrap();
    assert!(
        plans.iter().any(|p| p.contains("event_receipts_retention")),
        "{plans:?}"
    );
}

#[test]
fn event_body_resolver_must_be_query() {
    let f = Fixture::new(60);
    f.catalog.set_op(
        "plexus",
        basal_host::catalog::EVENT_BODY_OP,
        OpDecl {
            kind: Some(OpKind::Mutate),
            cause_echo: false,
            shell_capable: false,
        },
    );
    assert!(matches!(
        f.rt.install(&f.request("flow")),
        Err(crate::InstallError::ResolveOpNotQuery { .. })
    ));
}

#[test]
fn event_body_resolver_must_be_granted() {
    let f = Fixture::new(60);
    let mut request = f.request("flow");
    let mut manifest: serde_json::Value = serde_json::from_str(&request.manifest).unwrap();
    manifest["ops"] = json!([]);
    request.manifest = manifest.to_string();
    assert!(matches!(
        f.rt.install(&request),
        Err(crate::InstallError::EventBodyOpNotGranted { .. })
    ));
}

#[test]
fn body_bytes_are_hashed_independently() {
    let mut changed = reply();
    changed["result"]["body"] = json!("{\"changed\":false}");
    assert_eq!(
        verify_body(&notice(0), &changed),
        Err("event_body_mismatch")
    );
}
#[test]
fn reply_digest_must_match_notice() {
    let mut changed = reply();
    changed["result"]["digest"] = json!("0".repeat(64));
    assert_eq!(
        verify_body(&notice(0), &changed),
        Err("event_body_mismatch")
    );
}
#[test]
fn notice_digest_must_match_body() {
    let mut changed = notice(0);
    changed.digest = "0".repeat(64);
    assert_eq!(verify_body(&changed, &reply()), Err("event_body_mismatch"));
}
#[test]
fn digest_spelling_is_exact_and_only_notice_prefix_is_normalized() {
    let mut n = notice(0);
    n.digest = format!("sha256:{}", n.digest);
    assert_eq!(verify_body(&n, &reply()).unwrap(), BODY.as_bytes());
    for digest in [
        format!("sha256:{}", notice(0).digest),
        notice(0).digest.to_uppercase(),
        format!(" {}", notice(0).digest),
    ] {
        let mut changed = reply();
        changed["result"]["digest"] = json!(digest);
        assert_eq!(
            verify_body(&notice(0), &changed),
            Err("event_body_mismatch")
        );
    }
}

#[test]
fn body_is_journaled_before_script_and_replay_never_calls_live() {
    let f = Fixture::new(60);
    let run = f.run();
    f.reply(reply(), Settlement::Fulfilled);
    let manifest = Manifest::parse(&run.manifest).unwrap();
    let Preamble::Ready(first) = f.rt.event_preamble(&run, &manifest).unwrap() else {
        panic!("body preamble not ready")
    };
    let bytes: Vec<u8> =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT body FROM event_body_journal WHERE run_id=?1",
                    [&run.run_id],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    assert_eq!(bytes, BODY.as_bytes());
    let Preamble::Ready(replay) = f.rt.event_preamble(&run, &manifest).unwrap() else {
        panic!("replay not ready")
    };
    assert_eq!(first, replay);
    let event: serde_json::Value = serde_json::from_str(first.as_str()).unwrap();
    assert_eq!(event["event"]["body"], json!({"changed":true}));
    let calls = f.source.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].flow_id, "flow");
    assert_eq!(
        calls[0].kind,
        CallKind::Op {
            module: "plexus".into(),
            op: basal_host::catalog::EVENT_BODY_OP.into()
        }
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(calls[0].args.as_str()).unwrap(),
        json!({"event_key":notice(0).event_key,"event_name":"github_pr_changed"})
    );
}

#[test]
fn each_events_get_refusal_preserves_its_failure_kind() {
    for code in [
        "event_not_visible",
        "event_name_mismatch",
        "invalid_request",
        "flow_scope_required",
        "flow_carrier_not_admitted",
        "events_get_carrier_refused",
    ] {
        let f = Fixture::new(60);
        let run = f.run();
        f.reply(json!({"error":{"code":code}}), Settlement::Rejected);
        let Preamble::Failed { kind, .. } =
            f.rt.event_preamble(&run, &Manifest::parse(&run.manifest).unwrap())
                .unwrap()
        else {
            panic!("refusal {code} was not terminal")
        };
        assert_eq!(kind, code);
    }
}

#[test]
fn unavailable_body_is_deferred_with_backoff_until_deadline() {
    let f = Fixture::new(60);
    let mut run = f.run();
    run.deadline_at = Some(10_000);
    f.source
        .replies
        .lock()
        .unwrap()
        .push(Err(TransportError::unsent("source offline")));
    let manifest = Manifest::parse(&run.manifest).unwrap();
    assert!(matches!(
        f.rt.event_preamble(&run, &manifest).unwrap(),
        Preamble::Retry
    ));
    assert!(f.rt.startable().unwrap().is_empty());
    let first: i64 =
        f.rt.store()
            .read(|c| Ok(c.query_row("SELECT retry_at FROM event_body_journal", [], |r| r.get(0))?))
            .unwrap();
    f.rt.config.clock.set(first);
    f.source
        .replies
        .lock()
        .unwrap()
        .push(Err(TransportError::unsent("source offline")));
    assert!(matches!(
        f.rt.event_preamble(&run, &manifest).unwrap(),
        Preamble::Retry
    ));
    let second: i64 =
        f.rt.store()
            .read(|c| Ok(c.query_row("SELECT retry_at FROM event_body_journal", [], |r| r.get(0))?))
            .unwrap();
    assert!(second - first > first - 1_000);
    f.rt.config.clock.set(10_000);
    assert!(
        matches!(f.rt.event_preamble(&run,&manifest).unwrap(), Preamble::Failed {kind,..} if kind == "deadline")
    );
    assert_eq!(f.source.calls.lock().unwrap().len(), 2);
}

#[test]
fn synthetic_event_dry_run_never_resolves_live() {
    let f = Fixture::new(60);
    let mut run = f.run();
    run.trigger = JsonText::new(json!({"event":{"module":"plexus","name":"github_pr_changed","version":1,"event_key":notice(0).event_key,"headers":{},"body":{"synthetic":true}}}).to_string()).unwrap();
    assert!(
        matches!(f.rt.event_preamble(&run,&Manifest::parse(&run.manifest).unwrap()).unwrap(),Preamble::Ready(trigger) if trigger == run.trigger)
    );
    assert!(f.source.calls.lock().unwrap().is_empty());
}

#[test]
fn same_key_on_different_subjects_is_not_a_duplicate() {
    let f = Fixture::new(60);
    f.catalog.set_event(
        "plexus",
        "github_issue_changed",
        1,
        basal_host::EventDecl {
            origin: basal_host::EventOrigin::External,
            body: basal_host::EventBody::Resolved {
                resolve_op: basal_host::catalog::EVENT_BODY_OP.into(),
            },
        },
    );
    let mut request = f.request("flow");
    let mut manifest: serde_json::Value = serde_json::from_str(&request.manifest).unwrap();
    manifest["trigger"]["events"]
        .as_array_mut()
        .unwrap()
        .push(json!({"module":"plexus","name":"github_issue_changed","version":1}));
    request.manifest = manifest.to_string();
    let installed = f.rt.install(&request).unwrap();
    f.rt.approve("flow", 1, &installed.code_hash, "test")
        .unwrap();
    let mut other = notice(0);
    other.subject = "ck.account.event.plexus.github_issue_changed.v1".into();
    assert_eq!(f.rt.admit_events(&[notice(0), other]).unwrap().admitted, 2);
}

#[test]
fn malformed_notices_are_refused_before_admission() {
    let f = Fixture::new(60);
    f.approve("flow");
    for subject in [
        "ck.account.room.plexus.github_pr_changed.v1",
        "ck.account.event.plexus.github_pr_changed.v01",
        "ck.account.event.plexus.github_pr_changed.v0",
        "ck.account.event.plexus.github_pr_changed.v1.extra",
    ] {
        let mut invalid = notice(0);
        invalid.subject = subject.into();
        assert!(
            f.rt.admit_events(&[invalid]).is_err(),
            "invalid subject {subject}"
        );
    }
    let mut invalid = notice(0);
    invalid.event_key = "A".repeat(64);
    assert!(f.rt.admit_events(&[invalid]).is_err());
}

#[test]
fn body_returning_after_deadline_is_not_delivered_to_script() {
    let f = Fixture::new(60);
    let mut run = f.run();
    run.deadline_at = Some(2_000);
    let clock = f.rt.config.clock.clone();
    *f.source.on_call.lock().unwrap() = Some(Box::new(move || clock.set(2_000)));
    f.reply(reply(), Settlement::Fulfilled);
    assert!(
        matches!(f.rt.event_preamble(&run,&Manifest::parse(&run.manifest).unwrap()).unwrap(), Preamble::Failed {kind,..} if kind == "deadline")
    );
    let bodies: i64 =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM event_body_journal WHERE body IS NOT NULL",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    assert_eq!(bodies, 0);
}

#[test]
fn first_approval_cutoff_is_per_flow_and_includes_equal_publish_time() {
    let f = Fixture::new(60);
    f.approve("old");
    f.rt.config.clock.set(2_000);
    f.approve("new");
    let mut between = notice(1);
    between.published_at_ms = 1_500;
    let report = f.rt.admit_events(&[between]).unwrap();
    assert_eq!(
        (
            report.admitted,
            report.matched,
            report.skipped_before_install
        ),
        (1, 1, 1)
    );
    let mut equal = notice(2);
    equal.published_at_ms = 2_000;
    assert_eq!(f.rt.admit_events(&[equal]).unwrap().admitted, 2);
    let health = f.rt.flow_health().unwrap();
    assert_eq!(
        health
            .iter()
            .find(|h| h.flow_id == "new")
            .unwrap()
            .skipped_before_install,
        1
    );
    assert_eq!(
        health
            .iter()
            .find(|h| h.flow_id == "old")
            .unwrap()
            .skipped_before_install,
        0
    );
}

#[test]
fn package_instance_first_approval_survives_version_changes() {
    let f = Fixture::new(60);
    f.catalog.add_agent("owner");
    let first = f.request("pkg");
    f.rt.register_package(&first.script, &first.manifest)
        .unwrap();
    let instance = f.rt.ensure_instance("pkg", 1, "owner", 1).unwrap()["flow_id"]
        .as_str()
        .unwrap()
        .to_owned();
    f.rt.config.clock.set(3_000);
    let mut manifest: serde_json::Value = serde_json::from_str(&first.manifest).unwrap();
    manifest["version"] = json!(2);
    f.rt.register_package("return 2;", &manifest.to_string())
        .unwrap();
    f.rt.ensure_instance("pkg", 2, "owner", 2).unwrap();
    let first_approved: i64 =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT first_approved_at FROM flows WHERE flow_id=?1",
                    [&instance],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    assert_eq!(first_approved, 1_000);
    let mut between = notice(0);
    between.published_at_ms = 2_000;
    assert_eq!(f.rt.admit_events(&[between]).unwrap().admitted, 1);
}

#[test]
fn migration_22_backfills_first_approval_without_using_latest_version_time() {
    let c = rusqlite::Connection::open_in_memory().unwrap();
    c.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for m in crate::schema::MIGRATIONS.iter().filter(|m| m.version < 22) {
        c.execute_batch(m.statements).unwrap();
    }
    c.execute_batch(r#"
        INSERT INTO flows(flow_id,created_at,approved_version) VALUES ('normal',500,2),('pending',500,NULL);
        INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,installed_at,approved_at,approval_ref)
        VALUES ('normal',1,zeroblob(32),'{}','return 1;','operator','superseded',500,1000,'v1'),
               ('normal',2,zeroblob(32),'{}','return 2;','operator','approved',1500,2000,'v2');
        INSERT INTO flows(flow_id,created_at,approved_version,package) VALUES ('instance',1200,1,'pkg');
    "#).unwrap();
    c.execute_batch(
        crate::schema::MIGRATIONS
            .iter()
            .find(|m| m.version == 22)
            .unwrap()
            .statements,
    )
    .unwrap();
    let timestamp = |flow| {
        c.query_row(
            "SELECT first_approved_at FROM flows WHERE flow_id=?1",
            [flow],
            |r| r.get::<_, Option<i64>>(0),
        )
        .unwrap()
    };
    assert_eq!(timestamp("normal"), Some(1000));
    assert_eq!(timestamp("instance"), Some(1200));
    assert_eq!(timestamp("pending"), None);
}

#[test]
fn queued_history_before_approval_is_refused_without_a_run() {
    let f = Fixture::new(60);
    f.approve("flow");
    let mut old = notice(0);
    old.published_at_ms = 999;
    f.rt.store()
        .write(|tx| receipt(tx, "flow", &old, "backlog", 1000))
        .unwrap();
    assert_eq!(f.rt.drain_event_backlog().unwrap(), 0);
    let health = f.rt.flow_health().unwrap();
    assert_eq!(
        (health[0].event_backlog, health[0].skipped_before_install),
        (0, 1)
    );
    let runs: i64 =
        f.rt.store()
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?))
            .unwrap();
    assert_eq!(runs, 0);
}
