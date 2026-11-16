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
    calls: Mutex<Vec<Value>>,
    statuses: Mutex<Vec<u32>>,
    status: Mutex<Option<InstallStatus>>,
}
impl Host for NoticeHost {
    fn supports_core_capability(&self, cap: &str) -> bool {
        cap == basal_host::core_host::FLOW_OWNER_NOTICE_V1
    }
    fn owner_notice(&self, request: Value) -> Result<Value, basal_host::transport::WireError> {
        self.calls.lock().unwrap().push(request);
        Ok(json!({"accepted":true}))
    }
    fn install_status(&self, _: &str, version: u32) -> Result<InstallStatus, TransportError> {
        self.statuses.lock().unwrap().push(version);
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
    rt.store().write(|tx| { tx.execute("INSERT INTO outbox(at,kind,flow_id,body,retry_at) VALUES(1000,'flow.owner_notice','f',?1,1000)",[body.to_string()])?; Ok(()) }).unwrap();
    body
}

fn header<'a>(m: &'a async_nats::jetstream::Message, key: &str) -> &'a str {
    m.headers.as_ref().unwrap().get_last(key).unwrap().as_str()
}

#[tokio::test]
async fn owner_notice_bus_publish_is_once_and_call_delivery_is_independent() {
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("owner.db"), host.clone());
    let body = owner(&rt);
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        1
    );
    rt.drain_owner_notices().unwrap();
    assert_eq!(*host.calls.lock().unwrap(), vec![body.clone()]);
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        0
    );
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
async fn owner_notice_bus_failure_retries_without_touching_call_delivery() {
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("retry-owner.db"), host.clone());
    let body = owner(&rt);
    let js = async_nats::jetstream::new(server.bus.client());
    js.delete_stream(STREAM).await.unwrap();
    assert!(publish_outbox(&rt, &server.bus, "account").await.is_err());
    assert_eq!(rt.store().read(|c|Ok(c.query_row("SELECT json_array(delivered_at,attempts,retry_at,bus_attempts,bus_retry_at) FROM outbox",[],|r|r.get::<_,String>(0))?)).unwrap(),"[null,0,1000,1,1250]");
    rt.drain_owner_notices().unwrap();
    assert_eq!(*host.calls.lock().unwrap(), vec![body]);
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
    assert_eq!(host.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn late_result_wakes_survive_restart_before_publish_and_dedupe_ack_crash() {
    let server = Server::start().await;
    let path = server.dir.join("late.db");
    let rt = runtime(&path);
    for custodian in ["reserved:broca", "reserved:prefrontal-core"] {
        rt.store().write(|tx| {
            tx.execute("INSERT INTO codemode_late_results(custodian,entry,event_id,settled_at) VALUES(?1,'{}',?1,1000)",[custodian])?;
            basal_core::bus_outbox::enqueue_late_result_wake(tx,custodian,tx.last_insert_rowid(),1000)
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
    for (message, (module, seq)) in messages.iter().zip([("broca", 1), ("prefrontal-core", 2)]) {
        assert_eq!(
            message.subject.as_str(),
            format!("ck.account.event.basal.late_result_ready.v1.{module}")
        );
        assert_eq!(header(message, "Nats-Msg-Id"), format!("{module}:{seq}"));
        assert_eq!(
            serde_json::from_slice::<Value>(&message.payload).unwrap(),
            json!({"seq":seq})
        );
    }
}

async fn publish_control(server: &Server, subject: &str, body: Value, id: &str) {
    let body = body.to_string();
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Nats-Msg-Id", id);
    headers.insert(
        "Ck-Content-Digest",
        ContentDigest::of_bytes(body.as_bytes())
            .to_string()
            .as_str(),
    );
    async_nats::jetstream::new(server.bus.client())
        .publish_with_headers(subject.to_owned(), headers, body.into())
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

#[tokio::test]
async fn install_change_notice_reads_status_once_and_applies_like_hint() {
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("install.db"), host.clone());
    let card = pending_card(&rt, 1);
    *host.status.lock().unwrap() = Some(InstallStatus::Declined {
        code_hash: basal_core::ids::hex(&card.code_hash),
    });
    publish_control(
        &server,
        "ck.account.event.prefrontal-core.flow_install_changed.v1",
        json!({"flow_id":"control-flow","seq":1}),
        "install-change:1",
    )
    .await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    commit_batch(rt.clone(), &messages, true).await.unwrap();
    assert_eq!(*host.statuses.lock().unwrap(), vec![1]);
    assert_eq!(
        rt.card(&card.card_id).unwrap().unwrap().state,
        basal_core::cards::CardState::Declined
    );
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
    rt.install_decided("control-flow", 1, 1).unwrap();
    assert_eq!(host.statuses.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn malformed_install_change_is_acked_without_status_read() {
    let subject = "ck.account.event.prefrontal-core.flow_install_changed.v1";
    let warnings_before = super::reader::warning_count_for(subject);
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("invalid-install.db"), host.clone());
    pending_card(&rt, 1);
    publish_control(
        &server,
        "ck.account.event.prefrontal-core.flow_install_changed.v1",
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
        json!({"seq":1}),
        "basal:1",
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
async fn install_change_checks_pending_versions_oldest_first_and_caps_at_eight() {
    let server = Server::start().await;
    let host = Arc::new(NoticeHost::default());
    let rt = with_host(&server.dir.join("many-installs.db"), host.clone());
    for version in 1..=9 {
        pending_card(&rt, version);
    }
    publish_control(
        &server,
        "ck.account.event.prefrontal-core.flow_install_changed.v1",
        json!({"flow_id":"control-flow","seq":2}),
        "many-installs:1",
    )
    .await;
    let messages = pull(&bind(&server.bus, STREAM).await.unwrap())
        .await
        .unwrap();
    commit_batch(rt, &messages, true).await.unwrap();
    assert_eq!(*host.statuses.lock().unwrap(), (1..=8).collect::<Vec<_>>());
}
