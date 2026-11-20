use super::*;
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, InstallStatus, TransportError,
};
use basal_module::events::{commit_batch_with_custody, publish_outbox};
use basal_proto::CallKind;
use serde_json::Value;
use std::sync::Mutex;

#[tokio::test]
async fn attribution_outbox_publishes_once_and_vectors_match_real_paths() {
    use sha2::{Digest, Sha256};
    let server = Server::start().await;
    let f = common::attribution::setup("attribution-bus");
    let mut vectors = common::attribution::vectors(&f);
    let pending = f.module.rt.pending_bus_events(500).unwrap();
    assert_eq!(pending.len(), 4);
    assert_eq!(
        publish_outbox(&f.module.rt, &server.bus, "account")
            .await
            .unwrap(),
        5
    );
    // A broker acknowledgement can survive a crash that loses basal's local
    // published mark. The immutable fact id deduplicates that retry.
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute(
                "UPDATE outbox SET bus_published_at=NULL WHERE kind='attribution.fact'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        publish_outbox(&f.module.rt, &server.bus, "account")
            .await
            .unwrap(),
        4
    );
    assert_eq!(
        publish_outbox(&f.module.rt, &server.bus, "account")
            .await
            .unwrap(),
        0
    );
    let messages = pull(&bind(&server.bus, STREAM).await.unwrap())
        .await
        .unwrap();
    let facts: Vec<_> = messages
        .iter()
        .filter(|m| m.subject.as_str() == "ck.account.event.basal.attribution_fact.v1")
        .collect();
    assert_eq!(facts.len(), 4);
    for message in &facts {
        let body: Value = serde_json::from_slice(&message.payload).unwrap();
        assert_eq!(
            header(message, "Nats-Msg-Id"),
            body["fact_id"].as_str().unwrap()
        );
        assert!(
            pending
                .iter()
                .any(|e| e.message_id == body["fact_id"].as_str().unwrap()
                    && serde_json::from_str::<Value>(&e.body).unwrap() == body)
        );
    }
    let message = facts
        .iter()
        .find(|m| {
            serde_json::from_slice::<Value>(&m.payload).unwrap()["fact_id"]
                == "basal:flow:attribution-flow:v1"
        })
        .unwrap();
    vectors.insert("bus-message.json".into(),json!({"subject":message.subject.as_str(),"headers":{"Nats-Msg-Id":header(message,"Nats-Msg-Id")},"body":serde_json::from_slice::<Value>(&message.payload).unwrap()}));
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/attribution-facts-v1");
    let write = std::env::var_os("BASAL_WRITE_VECTORS").is_some();
    if write {
        std::fs::create_dir_all(&dir).unwrap();
    }
    let mut sums = String::new();
    for (name, value) in &vectors {
        let actual = serde_json::to_string_pretty(value).unwrap() + "\n";
        if write {
            std::fs::write(dir.join(name), &actual).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(dir.join(name)).unwrap_or_default(),
            actual,
            "vector drift: {name}"
        );
        sums.push_str(&format!(
            "{:x}  {name}\n",
            Sha256::digest(actual.as_bytes())
        ));
    }
    if write {
        std::fs::write(dir.join("SHA256SUMS"), &sums).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap(),
        sums
    );
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "README.md")
        .collect();
    names.sort();
    let mut expected: Vec<_> = vectors
        .keys()
        .cloned()
        .chain(["SHA256SUMS".into()])
        .collect();
    expected.sort();
    assert_eq!(
        names, expected,
        "every vector must be generated and checked"
    );
}

#[derive(Default)]
struct NoticeHost {
    statuses: Mutex<Vec<u32>>,
    status: Mutex<Option<InstallStatus>>,
    timeout: std::sync::atomic::AtomicBool,
}
impl Host for NoticeHost {
    fn install_status(&self, _: &str, version: u32) -> Result<InstallStatus, TransportError> {
        self.statuses.lock().unwrap().push(version);
        if self.timeout.load(Ordering::SeqCst) {
            return Err(TransportError::unsent("flow.install_status timed out"));
        }
        Ok(self
            .status
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(InstallStatus::Unknown))
    }
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> Result<Dispatched, TransportError> {
        unreachable!()
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.5
    }
}

fn with_host(path: &std::path::Path, host: Arc<NoticeHost>) -> Runtime {
    Runtime::new(
        Arc::new(Store::open(path, Durability::throwaway()).unwrap()),
        host,
        Arc::new(MockCatalog::standard()),
        Arc::new(basal_core::NoHooks),
        None,
        Config {
            clock: Clock::manual(1000),
            install_gate: InstallGate::Off,
            ..Default::default()
        },
    )
    .unwrap()
}

fn owner(rt: &Runtime) -> Value {
    let body = json!({"notice_id":"f:1:failing","flow_id":"f","version":1,"kind":"failing","consecutive_failures":3,"error_kind":"event_not_visible","phase":"preamble","failing_since_ms":1000,"run_id":"run:1","at_ms":1000});
    rt.store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO outbox(at,kind,flow_id,body) VALUES(1000,'flow.owner_notice','f',?1)",
                [body.to_string()],
            )?;
            Ok(())
        })
        .unwrap();
    body
}

fn header<'a>(m: &'a async_nats::jetstream::Message, key: &str) -> &'a str {
    m.headers.as_ref().unwrap().get_last(key).unwrap().as_str()
}

#[tokio::test]
async fn owner_notice_bus_publish_completes_delivery_and_dedupes_a_lost_ack_record() {
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("owner.db"));
    let body = owner(&rt);
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        1
    );
    assert!(rt.outbox().unwrap().is_empty());
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        0
    );
    // The broker may have stored the notice even if basal lost its local ack mark.
    rt.store()
        .write(|tx| {
            tx.execute("UPDATE outbox SET bus_published_at=NULL", [])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        1
    );
    assert!(rt.outbox().unwrap().is_empty());
    let messages = pull(&bind(&server.bus, STREAM).await.unwrap())
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].subject.as_str(),
        "ck.account.event.basal.flow_owner_notice.v1"
    );
    assert_eq!(header(&messages[0], "Nats-Msg-Id"), "f:1:failing");
    assert_eq!(
        serde_json::from_slice::<Value>(&messages[0].payload).unwrap(),
        body
    );
}

#[tokio::test]
async fn owner_notice_bus_failure_leaves_delivery_pending_until_retry_succeeds() {
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("retry-owner.db"));
    owner(&rt);
    let js = async_nats::jetstream::new(server.bus.client());
    js.delete_stream(STREAM).await.unwrap();
    assert!(publish_outbox(&rt, &server.bus, "account").await.is_err());
    assert_eq!(rt.outbox().unwrap().len(), 1);
    assert_eq!(rt.next_bus_event_at().unwrap(), Some(1250));
    let bus_state: (Option<i64>, i64, i64) = rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT bus_published_at,bus_attempts,bus_retry_at FROM outbox",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .unwrap();
    assert_eq!(bus_state, (None, 1, 1250));
    rt.config().clock.set(1249);
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        0
    );
    js.create_stream(async_nats::jetstream::stream::Config {
        name: STREAM.into(),
        subjects: vec!["ck.account.event.>".into()],
        ..Default::default()
    })
    .await
    .unwrap();
    rt.config().clock.set(1250);
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        1
    );
    assert!(rt.outbox().unwrap().is_empty());
    assert_eq!(rt.next_bus_event_at().unwrap(), None);
}

#[tokio::test]
async fn late_result_wakes_survive_restart_before_publish_and_dedupe_ack_crash() {
    let server = Server::start().await;
    let path = server.dir.join("late.db");
    let rt = runtime(&path);
    for custodian in ["reserved:broca", "reserved:prefrontal"] {
        rt.store().write(|tx| {
            tx.execute("INSERT INTO codemode_late_results(custodian,entry,event_id,settled_at) VALUES(?1,'{}',?1,1000)",[custodian])?;
            basal_core::bus_outbox::enqueue_late_result_wake(tx,custodian,"key","ok",custodian,1000)
        }).unwrap();
    }
    drop(rt);
    let rt = runtime(&path);
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        2
    );
    // Simulate losing only the local acknowledgement after the server committed.
    rt.store()
        .write(|tx| {
            tx.execute("UPDATE outbox SET bus_published_at=NULL", [])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        2
    );
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        0
    );
    let messages = pull(&bind(&server.bus, STREAM).await.unwrap())
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    for (message, module) in messages.iter().zip(["broca", "prefrontal"]) {
        assert_eq!(
            message.subject.as_str(),
            format!("ck.account.event.basal.late_result_ready.v1.{module}")
        );
        assert_eq!(
            header(message, "Nats-Msg-Id"),
            format!("reserved:{module}:{module}")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&message.payload).unwrap(),
            json!({"call_key":"key","outcome":"ok"})
        );
    }
}

fn late_ready_vector(path: &str) -> &'static str {
    cortexkit_role_tool_provider::vectors::late_result_ready::FILES
        .iter()
        .find(|(name, _)| *name == path)
        .unwrap()
        .1
}

#[tokio::test]
async fn late_result_publisher_matches_every_packaged_sample() {
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("vectors.db"));
    // The samples name provider `provider`; basal publishes the same payload
    // and settlement id under its own provider token.
    for (case, call_key, outcome, event_id) in [
        ("ok", "call:ok", "ok", "settlement:1"),
        ("error", "call:error", "tool_denied", "settlement:2"),
        ("unknown", "call:unknown", "unknown", "settlement:3"),
    ] {
        rt.store()
            .write(|tx| {
                basal_core::bus_outbox::enqueue_late_result_wake(
                    tx,
                    "reserved:reader",
                    call_key,
                    outcome,
                    event_id,
                    1000,
                )
            })
            .unwrap();
        assert_eq!(
            publish_outbox(&rt, &server.bus, "account").await.unwrap(),
            1
        );
        let messages = pull(&bind(&server.bus, STREAM).await.unwrap())
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(
            message.payload.as_ref(),
            late_ready_vector(&format!("{case}/payload.json")).as_bytes()
        );
        assert_eq!(
            header(message, "Nats-Msg-Id"),
            late_ready_vector(&format!("{case}/nats-msg-id.txt")).trim_end()
        );
        let subject = late_ready_vector(&format!("{case}/subject.txt"))
            .trim_end()
            .replace(".provider.", ".basal.");
        assert_eq!(message.subject.as_str(), format!("ck.account.{subject}"));
        commit_batch(rt.clone(), &messages, true).await.unwrap();
    }
}

#[tokio::test]
async fn legacy_sequence_wakes_are_retired_without_publish_or_backoff() {
    let server = Server::start().await;
    let path = server.dir.join("legacy.db");
    let rt = runtime(&path);
    rt.store().write(|tx| {
        tx.execute("INSERT INTO outbox(at,kind,flow_id,recipient,body,bus_retry_at) VALUES (1000,'codemode.late_result_ready','','broca','{\"seq\":1}',1000)", [])?;
        basal_core::bus_outbox::enqueue_late_result_wake(tx,"reserved:broca","call:new","ok","settlement:new",1000)
    }).unwrap();
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        1
    );
    let retired: (Option<i64>, u32) = rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT bus_published_at,bus_attempts FROM outbox WHERE body='{\"seq\":1}'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert!(retired.0.is_some());
    assert_eq!(retired.1, 0);
    drop(rt);
    let rt = runtime(&path);
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        0
    );
    assert_eq!(rt.next_bus_event_at().unwrap(), None);
    let messages = pull(&bind(&server.bus, STREAM).await.unwrap())
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].payload.as_ref(),
        br#"{"call_key":"call:new","outcome":"ok"}"#
    );
}

async fn publish_control(server: &Server, subject: &str, body: Value, id: &str) {
    publish_control_body(server, subject, &body.to_string(), id).await;
}

async fn publish_control_body(server: &Server, subject: &str, body: &str, id: &str) {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Nats-Msg-Id", id);
    headers.insert(
        "Ck-Content-Digest",
        ContentDigest::of_bytes(body.as_bytes())
            .to_string()
            .as_str(),
    );
    async_nats::jetstream::new(server.bus.client())
        .publish_with_headers(subject.to_owned(), headers, body.to_owned().into())
        .await
        .unwrap()
        .await
        .unwrap();
}

fn pending_card(rt: &Runtime, version: u32) -> basal_core::cards::CardRecord {
    let mut manifest = json!({"id":"control-flow","version":version,"explanation":"Exercises an install status hint without making external calls.","purpose":"Test control notice","trigger":{"schedule":{"interval":"1h"}}});
    if version > 1 {
        manifest["changes"] = json!("Update pending version");
    }
    let installed = rt
        .install(&InstallRequest {
            script: "return 1;".into(),
            manifest: manifest.to_string(),
            author: "local:unverified".into(),
            loop_override: false,
        })
        .unwrap();
    rt.record_card(
        "control-flow",
        version,
        &installed.code_hash,
        "local:unverified",
        "test card",
    )
    .unwrap()
}

fn install_engine(rt: &Runtime) -> basal_module::engine::Engine {
    use basal_module::pool::{Pool, PoolConfig, ProcessSpawner};
    use basal_module::process::WorkerLaunch;
    let mut config = PoolConfig::new("unused", WorkerLaunch::Plain);
    config.warm_spares = 0;
    let spawner = Arc::new(ProcessSpawner::new(&config));
    basal_module::engine::Engine::new(
        rt.clone(),
        Pool::new(
            config,
            spawner,
            rt.config().clock.clone(),
            Default::default(),
        ),
        Arc::new(basal_host::MockConsent::new()),
        Default::default(),
        Default::default(),
        Default::default(),
    )
    .unwrap()
}

async fn install_notices(server: &Server, rt: &Runtime, seqs: std::ops::RangeInclusive<u64>) {
    let len = seqs.clone().count();
    for seq in seqs {
        publish_control(
            server,
            "ck.account.event.prefrontal.flow_install_changed.v1",
            json!({"flow_id":"control-flow","seq":seq}),
            &format!("install-change:{seq}"),
        )
        .await;
    }
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), len);
    commit_batch(rt.clone(), &messages, true).await.unwrap();
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}

#[tokio::test]
async fn audit_install_timeout_retries_after_ack_without_a_second_notice() {
    use basal_core::cards::CardState;
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let path = server.dir.join("install-timeout.db");
    let rt = with_host(&path, host.clone());
    let engine = install_engine(&rt);
    let card = pending_card(&rt, 1);
    host.timeout.store(true, Ordering::SeqCst);
    install_notices(&server, &rt, 1..=1).await;
    engine.pass().unwrap();
    assert_eq!(*host.statuses.lock().unwrap(), [1]);
    assert_eq!(
        rt.card(&card.card_id).unwrap().unwrap().state,
        CardState::Pending
    );
    assert_eq!(rt.next_wake_at(None).unwrap(), Some(31_000));

    host.timeout.store(false, Ordering::SeqCst);
    *host.status.lock().unwrap() = Some(InstallStatus::Active {
        code_hash: basal_core::ids::hex(&card.code_hash),
        scope: None,
    });
    drop(engine);
    drop(rt);

    let rt = with_host(&path, host.clone());
    rt.recover_cardless_installs().unwrap();
    let engine = install_engine(&rt);
    rt.config().clock.set(30_999);
    engine.pass().unwrap();
    assert_eq!(*host.statuses.lock().unwrap(), [1]);
    rt.config().clock.set(31_000);
    engine.pass().unwrap();
    assert_eq!(*host.statuses.lock().unwrap(), [1, 1]);
    assert_eq!(
        rt.card(&card.card_id).unwrap().unwrap().state,
        CardState::Approved
    );
}

#[tokio::test]
async fn audit_install_notice_batch_acks_without_core_reads() {
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("install-notice-batch.db"), host.clone());
    pending_card(&rt, 1);
    pending_card(&rt, 2);
    install_notices(&server, &rt, 1..=4).await;
    assert!(host.statuses.lock().unwrap().is_empty());
    install_engine(&rt).pass().unwrap();
    assert_eq!(*host.statuses.lock().unwrap(), [2, 1]);
}

#[tokio::test]
async fn audit_install_notice_approves_the_ninth_version_and_supersedes_older_cards() {
    use basal_core::cards::CardState;
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("install-newest.db"), host.clone());
    let cards: Vec<_> = (1..=9).map(|v| pending_card(&rt, v)).collect();
    *host.status.lock().unwrap() = Some(InstallStatus::Active {
        code_hash: basal_core::ids::hex(&cards[8].code_hash),
        scope: None,
    });
    install_notices(&server, &rt, 1..=1).await;
    install_engine(&rt).pass().unwrap();
    assert_eq!(
        rt.flow("control-flow").unwrap().unwrap().approved_version,
        Some(9)
    );
    assert_eq!(*host.statuses.lock().unwrap(), [9]);
    for card in &cards[..8] {
        assert_eq!(
            rt.card(&card.card_id).unwrap().unwrap().state,
            CardState::Stale
        );
    }
}

#[test]
fn audit_install_startup_timeout_backs_off_and_retries_on_engine_pass() {
    use basal_core::cards::CardState;
    let dir = common::scratch("startup-install-retry");
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&dir.join("startup.db"), host.clone());
    let card = pending_card(&rt, 1);
    host.timeout.store(true, Ordering::SeqCst);
    rt.recover_cardless_installs().unwrap();
    let engine = install_engine(&rt);
    engine.pass().unwrap();
    let mut now = 1000;
    for delay in [30_000, 60_000, 120_000, 240_000, 480_000, 900_000, 900_000] {
        now += delay;
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(now));
        rt.config().clock.set(now);
        engine.pass().unwrap();
    }
    host.timeout.store(false, Ordering::SeqCst);
    *host.status.lock().unwrap() = Some(InstallStatus::Active {
        code_hash: basal_core::ids::hex(&card.code_hash),
        scope: None,
    });
    rt.config().clock.set(now + 900_000);
    engine.pass().unwrap();
    assert_eq!(*host.statuses.lock().unwrap(), [1; 9]);
    assert_eq!(
        rt.card(&card.card_id).unwrap().unwrap().state,
        CardState::Approved
    );
    drop(engine);
    drop(rt);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn audit_install_startup_held_approves_matching_hash() {
    use basal_core::cards::CardState;
    let dir = common::scratch("startup-install-status");
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&dir.join("startup.db"), host.clone());
    let held = pending_card(&rt, 1);
    *host.status.lock().unwrap() = Some(InstallStatus::Held {
        code_hash: basal_core::ids::hex(&held.code_hash),
        reason: "scope_stale".into(),
        stale_since_ms: Some(1),
    });
    rt.recover_cardless_installs().unwrap();
    assert_eq!(
        rt.card(&held.card_id).unwrap().unwrap().state,
        CardState::Approved
    );
    assert_eq!(
        rt.flow("control-flow").unwrap().unwrap().approved_version,
        Some(1)
    );
    drop(rt);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn audit_install_startup_revoked_settles_without_approval() {
    use basal_core::cards::CardState;
    let dir = common::scratch("startup-install-revoked");
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&dir.join("startup.db"), host.clone());
    let revoked = pending_card(&rt, 1);
    *host.status.lock().unwrap() = Some(InstallStatus::Revoked {
        code_hash: Some(basal_core::ids::hex(&revoked.code_hash)),
    });
    rt.recover_cardless_installs().unwrap();
    let revoked = rt.card(&revoked.card_id).unwrap().unwrap();
    assert_eq!(revoked.state, CardState::Declined);
    assert_eq!(revoked.rejected_reason.as_deref(), Some("revoked"));
    assert_eq!(
        rt.flow("control-flow").unwrap().unwrap().approved_version,
        None
    );
    drop(rt);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn audit_install_notice_rotates_the_eight_card_window() {
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("install-rotation.db"), host.clone());
    let engine = install_engine(&rt);
    for version in 1..=10 {
        pending_card(&rt, version);
    }
    install_notices(&server, &rt, 1..=1).await;
    engine.pass().unwrap();
    assert_eq!(host.statuses.lock().unwrap().len(), 8);
    host.statuses.lock().unwrap().clear();

    rt.config().clock.set(1001);
    install_notices(&server, &rt, 2..=2).await;
    engine.pass().unwrap();
    assert_eq!(*host.statuses.lock().unwrap(), [10, 8, 9, 1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn audit_install_unknown_card_expires_only_after_twenty_four_hours() {
    use basal_core::cards::CardState;
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("install-expiry.db"), host.clone());
    let card = pending_card(&rt, 1);
    let engine = install_engine(&rt);
    let day = 24 * 60 * 60 * 1000;
    rt.config().clock.set(1000 + day);
    install_notices(&server, &rt, 1..=1).await;
    engine.pass().unwrap();
    assert_eq!(
        rt.card(&card.card_id).unwrap().unwrap().state,
        CardState::Pending
    );

    rt.config().clock.set(1001 + day);
    install_notices(&server, &rt, 2..=2).await;
    engine.pass().unwrap();
    assert_eq!(
        rt.card(&card.card_id).unwrap().unwrap().state,
        CardState::Expired
    );
    assert_eq!(*host.statuses.lock().unwrap(), [1, 1]);
}

#[tokio::test]
async fn install_change_notice_checks_status_on_engine_and_skips_settled_cards() {
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("install.db"), host.clone());
    let engine = install_engine(&rt);
    let card = pending_card(&rt, 1);
    *host.status.lock().unwrap() = Some(InstallStatus::Declined {
        code_hash: basal_core::ids::hex(&card.code_hash),
    });
    let subject = "ck.account.event.prefrontal.flow_install_changed.v1";
    publish_control(
        &server,
        subject,
        json!({"flow_id":"control-flow","seq":1}),
        "install-change:1",
    )
    .await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    commit_batch(rt.clone(), &messages, true).await.unwrap();
    engine.pass().unwrap();
    assert_eq!(*host.statuses.lock().unwrap(), vec![1]);
    assert_eq!(
        rt.card(&card.card_id).unwrap().unwrap().state,
        basal_core::cards::CardState::Declined
    );
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
    publish_control(
        &server,
        subject,
        json!({"flow_id":"control-flow","seq":2}),
        "install-change:2",
    )
    .await;
    let messages = pull(&consumer).await.unwrap();
    commit_batch(rt, &messages, true).await.unwrap();
    engine.pass().unwrap();
    assert_eq!(host.statuses.lock().unwrap().len(), 1);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}

#[tokio::test]
async fn malformed_install_change_is_acked_without_status_read() {
    let subject = "ck.account.event.prefrontal.flow_install_changed.v1";
    let warnings_before = super::reader::warning_count_for(subject);
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("invalid-install.db"), host.clone());
    pending_card(&rt, 1);
    publish_control(
        &server,
        "ck.account.event.prefrontal.flow_install_changed.v1",
        json!({"flow_id":"control-flow","seq":"invalid"}),
        "bad-install:1",
    )
    .await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    commit_batch(rt, &messages, true).await.unwrap();
    assert!(host.statuses.lock().unwrap().is_empty());
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
    assert_eq!(
        super::reader::warning_count_for(subject) - warnings_before,
        1
    );
}

struct NoWorkers;
impl basal_core::channel::WorkerSource for NoWorkers {
    fn worker(
        &self,
    ) -> Result<Box<dyn basal_core::channel::WorkerChannel>, basal_core::channel::ChannelError>
    {
        unreachable!()
    }
}

#[tokio::test]
async fn every_late_result_ready_sample_wakes_only_its_addressed_provider() {
    use basal_core::codemode::supervisor::Supervisor;
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("sample-reader.db"));
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    for case in ["ok", "error", "unknown"] {
        let subject = "ck.account.event.aft.late_result_ready.v1.basal";
        let warnings = super::reader::warning_count_for(subject);
        let supervisor = Supervisor::new(
            Arc::new(
                Store::open(
                    server.dir.join(case).join("store.db"),
                    Durability::throwaway(),
                )
                .unwrap(),
            ),
            Arc::new(basal_module::unconfigured::UnconfiguredTransport),
            Arc::new(NoWorkers),
            rt.config().clock.clone(),
            basal_proto::PreludeHash::of("test"),
            Default::default(),
        )
        .unwrap();
        assert_eq!(supervisor.test_late_stream("aft", true), Some((2, false)));
        assert_eq!(
            supervisor.test_late_stream("cerebellum", true),
            Some((2, false))
        );
        let body = late_ready_vector(&format!("{case}/payload.json"));
        publish_control_body(
            &server,
            "ck.account.event.aft.late_result_ready.v1.other",
            body,
            &format!("{case}:other"),
        )
        .await;
        let messages = pull(&consumer).await.unwrap();
        commit_batch_with_custody(rt.clone(), supervisor.clone(), &messages, true)
            .await
            .unwrap();
        assert_eq!(supervisor.test_late_stream("aft", false), Some((2, false)));
        publish_control_body(&server, subject, body, &format!("{case}:basal")).await;
        let messages = pull(&consumer).await.unwrap();
        assert_eq!(messages.len(), 1);
        commit_batch_with_custody(rt.clone(), supervisor.clone(), &messages, true)
            .await
            .unwrap();
        assert_eq!(
            super::reader::warning_count_for(subject),
            warnings,
            "sample {case}"
        );
        assert_eq!(
            supervisor.test_late_stream("aft", false),
            Some((30, true)),
            "sample {case}"
        );
        assert_eq!(
            supervisor.test_late_stream("cerebellum", false),
            Some((2, false))
        );
        assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
    }
}

#[tokio::test]
async fn addressed_aft_wake_is_delivered_and_only_its_backoff_cap_rises() {
    use basal_core::codemode::supervisor::Supervisor;
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("custody.db"));
    let supervisor = Supervisor::new(
        Arc::new(
            Store::open(
                server.dir.join("supervisor/store.db"),
                Durability::throwaway(),
            )
            .unwrap(),
        ),
        Arc::new(basal_module::unconfigured::UnconfiguredTransport),
        Arc::new(NoWorkers),
        rt.config().clock.clone(),
        basal_proto::PreludeHash::of("test"),
        Default::default(),
    )
    .unwrap();
    assert_eq!(supervisor.test_late_stream("aft", true), Some((2, false)));
    assert_eq!(
        supervisor.test_late_stream("cerebellum", true),
        Some((2, false))
    );
    publish_control(
        &server,
        "ck.account.event.aft.late_result_ready.v1.basal",
        json!({"call_key":"call:1","outcome":"ok"}),
        "settlement:1:basal",
    )
    .await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    commit_batch_with_custody(rt.clone(), supervisor.clone(), &messages, true)
        .await
        .unwrap();
    assert_eq!(supervisor.test_late_stream("aft", false), Some((30, true)));
    assert_eq!(
        supervisor.test_late_stream("cerebellum", false),
        Some((2, false))
    );
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
    assert!(!supervisor.late_result_ready("absent"));
    assert_eq!(supervisor.test_late_stream("absent", false), None);
}

#[tokio::test]
async fn a_wake_with_extra_fields_still_reaches_custody() {
    use basal_core::codemode::supervisor::Supervisor;
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("custody-extra.db"));
    let supervisor = Supervisor::new(
        Arc::new(
            Store::open(
                server.dir.join("supervisor/store.db"),
                Durability::throwaway(),
            )
            .unwrap(),
        ),
        Arc::new(basal_module::unconfigured::UnconfiguredTransport),
        Arc::new(NoWorkers),
        rt.config().clock.clone(),
        basal_proto::PreludeHash::of("test"),
        Default::default(),
    )
    .unwrap();
    assert_eq!(
        supervisor.test_late_stream("cerebellum", true),
        Some((2, false))
    );
    publish_control(
        &server,
        "ck.account.event.cerebellum.late_result_ready.v1.basal",
        json!({"call_key":"call:7","outcome":"future_error_code","extra":{"ignored":true}}),
        "settlement:7:basal",
    )
    .await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    commit_batch_with_custody(rt.clone(), supervisor.clone(), &messages, true)
        .await
        .unwrap();
    assert_eq!(
        supervisor.test_late_stream("cerebellum", false),
        Some((30, true))
    );
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}

#[tokio::test]
async fn install_change_checks_newest_first_and_caps_at_eight_per_pass() {
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("many-installs.db"), host.clone());
    for version in 1..=9 {
        pending_card(&rt, version);
    }
    install_notices(&server, &rt, 1..=1).await;
    install_engine(&rt).pass().unwrap();
    assert_eq!(*host.statuses.lock().unwrap(), [9, 1, 2, 3, 4, 5, 6, 7]);
}
