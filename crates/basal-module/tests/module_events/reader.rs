use super::*;
use basal_host::subc_catalog::{DaemonCapabilities, SubcCatalog};
use basal_host::transport::{Transport, WireError};
use basal_module::events::{Reader, commit_fetched_batch_for_test};
use cortexkit_role_tool_provider::catalog::{CatalogAnswer, CatalogTool, schema_digest};
use cortexkit_role_tool_provider::describe::{Major, RoleDescribe};
use serde_json::Value;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

const SUBJECT_V2: &str = "ck.account.event.plexus.pull_request_review.v2";

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<(tracing::Level, String)>>>);

static CAPTURED_LOGS: OnceLock<CapturedLogs> = OnceLock::new();

impl tracing::Subscriber for CapturedLogs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?};", field.name()));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.push_str(&format!("{}={value:?};", field.name()));
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push((
            *event.metadata().level(),
            format!("{};{}", event.metadata().target(), fields.0),
        ));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn captured_logs() -> &'static CapturedLogs {
    CAPTURED_LOGS.get_or_init(|| {
        let logs = CapturedLogs::default();
        tracing::subscriber::set_global_default(logs.clone())
            .expect("test log subscriber is installed once");
        logs
    })
}

pub(super) fn warning_count_for(subject: &str) -> usize {
    captured_logs()
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, fields)| *level == tracing::Level::WARN && fields.contains(subject))
        .count()
}

fn logged_at(logs: &CapturedLogs, level: tracing::Level, subject: &str, reason: &str) -> bool {
    logs.0.lock().unwrap().iter().any(|(seen_level, fields)| {
        *seen_level == level && fields.contains(subject) && fields.contains(reason)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_other_token_is_acked_silently_even_with_a_payload() {
    let logs = captured_logs();
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    let reader = Reader::start(rt.clone(), transport);
    let subject = format!("{SUBJECT_V2}.other");
    let js = async_nats::jetstream::new(server.bus.client());
    js.publish(subject.clone(), "private payload".into())
        .await
        .unwrap()
        .await
        .unwrap();
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    // The fixture's flow approval also publishes basal's own attribution
    // fact on the same durable; the reader counts it as a notice too.
    let own: u64 = rt
        .store()
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM attribution_facts", [], |r| r.get(0))?))
        .unwrap();
    let health = wait_health(&reader, |h| h.notices == 1 + own).await;
    assert_eq!(health.malformed, 0);
    assert_eq!(health.admitted, 0);
    assert_eq!(run_count(&rt), 0);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
    assert!(
        !logs
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(level, fields)| *level == tracing::Level::WARN && fields.contains(&subject))
    );
}

struct BlockingTransport {
    handle: tokio::runtime::Handle,
    url: String,
    panic_once: AtomicBool,
    pause_restart: AtomicBool,
    unreadable: AtomicBool,
    catalog_calls: AtomicUsize,
    installing: AtomicBool,
    reader_tool_calls: AtomicUsize,
}

impl BlockingTransport {
    fn assert_no_reader_provider_calls(&self) {
        assert_eq!(
            self.reader_tool_calls.load(Ordering::SeqCst),
            0,
            "event admission, fan-out and health must not call a provider"
        );
    }
}

impl Transport for BlockingTransport {
    fn catalog(&self) -> Result<Value, WireError> {
        self.catalog_calls.fetch_add(1, Ordering::SeqCst);
        if self.panic_once.swap(false, Ordering::SeqCst) {
            self.pause_restart.store(true, Ordering::SeqCst);
            panic!("injected reader catalog panic");
        }
        // Like SubcTransport, synchronously enter Tokio with block_on: this
        // panics if decoding calls the catalog on a thread driving async tasks.
        self.handle.block_on(async {
            if self.unreadable.load(Ordering::SeqCst) {
                return Err(WireError::Unreadable("catalog unavailable".into()));
            }
            Ok(json!({"modules":[{"module_id":"plexus", "capabilities":{"provides":["flow-scopes/v1"]}, "events":[
                {"name":"pull_request_review", "version":2, "headers":[]}
            ], "roles":[{"role":"tool_provider", "tools":[{
                "name":"events_get", "execution_mode":"pure", "schema":{}
            }], "identity_scope":[], "concurrency":"serial", "emits_push":false, "sub_supervises":false}]}]}))
        })
    }

    fn management(&self, module: &str, op: &str, _: Value) -> Result<Value, WireError> {
        assert_eq!(module, "ckbus");
        let until = Instant::now() + Duration::from_secs(5);
        while self.pause_restart.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        match op {
            "ckbus.credential" | "ckbus.credential_renew" => Ok(json!({
                "jwt":"", "exp":basal_proto::util::unix_millis() / 1000 + 3600,
                "acct":"account", "inbox_prefix":ConnectConfig::new("TEST").unwrap().inbox_prefix(),
                "server_url":self.url, "credential_public":"TEST",
                "spawn_generation":1, "credential_epoch":1
            })),
            "ckbus.nonce_sign" => {
                use base64::Engine as _;
                Ok(
                    json!({"signature_b64":base64::engine::general_purpose::STANDARD.encode([0u8; 64])}),
                )
            }
            _ => panic!("unexpected credential op {op}"),
        }
    }

    fn tool(&self, module: &str, name: &str, _: Value, _: &str) -> Result<Value, WireError> {
        let installing = self.installing.load(Ordering::SeqCst);
        if !installing {
            self.reader_tool_calls.fetch_add(1, Ordering::SeqCst);
        }
        match (module, name) {
            ("plexus", "role.describe" | "tool.catalog") => {
                // Only fixture installation may inspect the granted tool's contract.
                assert!(installing, "reader must not read provider metadata");
                let answer = if name == "role.describe" {
                    serde_json::to_value(RoleDescribe::new(
                        vec![Major {
                            version: cortexkit_role_tool_provider::PROVIDES.into(),
                            ops: cortexkit_role_tool_provider::REQUIRED_OPS
                                .iter()
                                .map(|op| (*op).into())
                                .collect(),
                            stability: "stable".into(),
                        }],
                        "reader-fixture",
                    ))
                    .unwrap()
                } else {
                    let schema = json!({});
                    let tool =
                        CatalogTool::new("events_get", schema_digest(&schema).unwrap(), 1, schema)
                            .with_modes(vec![
                                cortexkit_role_tool_provider::CallMode::Classic,
                                cortexkit_role_tool_provider::CallMode::Flow,
                            ]);
                    serde_json::to_value(
                        CatalogAnswer::new("fixture", "fixture").with_tools(vec![tool]),
                    )
                    .unwrap()
                };
                Ok(answer)
            }
            _ => panic!("reader must not resolve event bodies"),
        }
    }
}

async fn fixture(server: &Server) -> (Runtime, Arc<BlockingTransport>) {
    let transport = Arc::new(BlockingTransport {
        handle: tokio::runtime::Handle::current(),
        url: server.url.clone(),
        panic_once: AtomicBool::new(false),
        pause_restart: AtomicBool::new(false),
        unreadable: AtomicBool::new(false),
        catalog_calls: AtomicUsize::new(0),
        installing: AtomicBool::new(true),
        reader_tool_calls: AtomicUsize::new(0),
    });
    let capabilities = Arc::new(DaemonCapabilities::default());
    capabilities.observe_hello_ack(
        &serde_json::from_value(json!({
            "negotiated_ver":subc_protocol::PROTOCOL_VERSION,
            "subc_ops":[], "subc_capabilities":[subc_protocol::manifest::CAP_CATALOG_EVENTS_V1],
            "storage":null, "machine_id":null
        }))
        .unwrap(),
    );
    let rt = Runtime::new(
        Arc::new(Store::open(server.dir.join("reader.db"), Durability::throwaway()).unwrap()),
        Arc::new(MockHost::new()),
        Arc::new(SubcCatalog::new(transport.clone()).with_daemon_capabilities(capabilities)),
        Arc::new(basal_core::NoHooks),
        None,
        Config {
            clock: Clock::manual(1000),
            install_gate: InstallGate::Off,
            ..Default::default()
        },
    )
    .expect("runtime startup");
    let install_rt = rt.clone();
    tokio::task::spawn_blocking(move || {
        let installed = install_rt.install(&InstallRequest {
            script: "return 1;".into(),
            manifest: json!({"id":"reader", "version":1, "explanation":"Exercises the declared trigger and grants for a test, without additional effects.","purpose":"Consume declared v2 events",
                "trigger":{"events":[{"module":"plexus", "name":"pull_request_review", "version":2}]},
                "ops":[{"module":"plexus", "op":"events_get"}]}).to_string(),
            author: "local:unverified".into(),
            loop_override: false,
        }).unwrap();
        install_rt.approve("reader", 1, &installed.code_hash, "test").unwrap();
    }).await.unwrap();
    // Install-time daemon catalog reads are not reader activity.
    transport.catalog_calls.store(0, Ordering::SeqCst);
    transport.installing.store(false, Ordering::SeqCst);
    (rt, transport)
}

async fn publish(server: &Server, n: u64, version: u32) {
    let key = format!("{n:064x}");
    let subject = SUBJECT_V2.replace(".v2", &format!(".v{version}"));
    super::v2::publish(server, &subject, &format!("{key}.v{version}"), Some(&key)).await;
}

async fn wait_health(
    reader: &Reader,
    predicate: impl Fn(&basal_module::events::Health) -> bool,
) -> basal_module::events::Health {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let health = reader.health();
        if predicate(&health) {
            return health;
        }
        assert!(
            Instant::now() < until,
            "reader health timed out: {health:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_admits_v2_with_blocking_catalog_and_stays_alive() {
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    let reader = Reader::start(rt.clone(), transport.clone());
    publish(&server, 1, 2).await;
    wait_health(&reader, |h| h.admitted == 1).await;
    publish(&server, 2, 2).await;
    let health = wait_health(&reader, |h| h.admitted == 2).await;
    assert!(health.connected);
    assert!(health.error.is_none(), "{health:?}");
    assert_eq!(health.malformed, 0);
    assert!(health.batches >= 2);
    assert_eq!(run_count(&rt), 2);
    assert!(transport.catalog_calls.load(Ordering::SeqCst) >= 2);
    wait_health(&reader, |h| h.backlog == 0).await;
    drop(reader);
    transport.assert_no_reader_provider_calls();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_drops_wrong_ack_origin_without_acking() {
    use async_nats::jetstream::{self, consumer};

    let server = Server::with_ack_wait(Duration::from_secs(30)).await;
    let (rt, transport) = fixture(&server).await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let mut other = jetstream::new(server.bus.client())
        .get_stream(STREAM)
        .await
        .unwrap()
        .create_consumer(consumer::pull::Config {
            durable_name: Some("other_reader".into()),
            filter_subject: SUBJECT_V2.into(),
            ack_policy: consumer::AckPolicy::Explicit,
            ack_wait: Duration::from_secs(30),
            ..Default::default()
        })
        .await
        .unwrap();
    publish(&server, 1, 2).await;
    let mut other_messages = pull(&other).await.unwrap();
    assert_eq!(other_messages.len(), 1);
    assert_reader_drops_off_subscription(
        &rt,
        &transport,
        &mut consumer,
        other_messages.pop().unwrap(),
        "ACK metadata does not name the bound stream and durable",
    )
    .await;
    let other_info = other.info().await.unwrap();
    assert_eq!(other_info.num_ack_pending, 1);
    assert_eq!(other_info.ack_floor.stream_sequence, 0);

    let genuine = pull(&consumer).await.unwrap();
    assert_genuine_follow_up(&rt, &mut consumer, genuine).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_drops_subject_outside_server_filter_without_acking() {
    let server = Server::with_ack_wait(Duration::from_secs(30)).await;
    let (rt, transport) = fixture(&server).await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    publish(&server, 1, 2).await;
    let genuine = pull(&consumer).await.unwrap();
    assert_eq!(genuine.len(), 1);
    // Change only the subject at the delivery seam. This keeps the real ACK
    // reply, so accidentally acknowledging the drop would advance the floor.
    let mut wrong = genuine[0].clone();
    wrong.message.subject = "ck.account.events.plexus.pull_request_review.v2".into();
    assert_reader_drops_off_subscription(
        &rt,
        &transport,
        &mut consumer,
        wrong,
        "subject does not match the bound consumer filter",
    )
    .await;
    let info = consumer.info().await.unwrap();
    assert_eq!(info.num_ack_pending, 1);
    assert_eq!(info.ack_floor.stream_sequence, 0);

    assert_genuine_follow_up(&rt, &mut consumer, genuine).await;
}

async fn assert_reader_drops_off_subscription(
    rt: &Runtime,
    transport: &BlockingTransport,
    consumer: &mut async_nats::jetstream::consumer::PullConsumer,
    delivery: async_nats::jetstream::Message,
    reason: &str,
) {
    let logs = captured_logs();
    let subject = delivery.subject.to_string();
    let health = commit_fetched_batch_for_test(
        rt.clone(),
        consumer,
        STREAM,
        vec![delivery.clone(), delivery],
    )
    .await
    .unwrap();
    assert_eq!(health.off_subscription, 2);
    assert_eq!(health.malformed, 0);
    assert_eq!(health.notices, 0);
    assert_eq!(health.admitted, 0);
    assert_eq!(run_count(rt), 0, "off-subscription events must not admit");
    assert_eq!(transport.catalog_calls.load(Ordering::SeqCst), 0);
    transport.assert_no_reader_provider_calls();
    let log_count = logs
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, fields)| {
            *level == tracing::Level::WARN
                && fields.starts_with("events;")
                && fields.contains(&subject)
                && fields.contains(reason)
                && fields.contains("dropping off-subscription delivery")
        })
        .count();
    assert_eq!(log_count, 1, "warn only on the first drop of each reason");
}

async fn assert_genuine_follow_up(
    rt: &Runtime,
    consumer: &mut async_nats::jetstream::consumer::PullConsumer,
    genuine: Vec<async_nats::jetstream::Message>,
) {
    assert_eq!(genuine.len(), 1);
    let health = commit_fetched_batch_for_test(rt.clone(), consumer, STREAM, genuine)
        .await
        .unwrap();
    assert_eq!(health.off_subscription, 0);
    assert_eq!(health.malformed, 0);
    assert_eq!(health.notices, 1);
    assert_eq!(health.admitted, 1);
    assert_eq!(run_count(rt), 1);
    let info = consumer.info().await.unwrap();
    assert_eq!(info.num_ack_pending, 0);
    assert_eq!(info.ack_floor.stream_sequence, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_uses_broad_server_filter_instead_of_derived_account() {
    let server = Server::with_subject(Duration::from_secs(30), "ck.>").await;
    let (rt, transport) = fixture(&server).await;
    let own = attribution_facts(&rt);
    let reader = Reader::start(rt.clone(), transport.clone());
    let subject = "ck.account2.event.plexus.pull_request_review.v2";
    let key = format!("{:064x}", 1);
    super::v2::publish(&server, subject, &format!("{key}.v2"), Some(&key)).await;
    let health = wait_health(&reader, |h| h.notices + h.off_subscription == own + 1).await;
    assert_eq!(health.off_subscription, 0);
    assert_eq!(health.malformed, 0);
    assert_eq!(health.notices, own + 1);
    assert_eq!(health.admitted, 1);
    assert_eq!(run_count(&rt), 1);
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let info = consumer.info().await.unwrap();
    assert_eq!(info.num_ack_pending, 0);
    assert_eq!(info.ack_floor.stream_sequence, own + 1);
    transport.assert_no_reader_provider_calls();
}

fn attribution_facts(rt: &Runtime) -> u64 {
    rt.store()
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM attribution_facts", [], |r| r.get(0))?))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_accepts_in_filter_subjects_at_several_token_depths() {
    let server = Server::with_ack_wait(Duration::from_secs(30)).await;
    let (rt, transport) = fixture(&server).await;
    let own = attribution_facts(&rt);
    let reader = Reader::start(rt.clone(), transport.clone());
    server
        .publish(1, "ck.account.event.magic-context.docs_proposal.v1")
        .await;
    server
        .publish(2, "ck.account.event.broca.run.task.step.started.v1")
        .await;
    publish(&server, 3, 2).await;
    let js = async_nats::jetstream::new(server.bus.client());
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Nats-Msg-Id", "settlement:1:basal");
    js.publish_with_headers(
        "ck.account.event.aft.late_result_ready.v1.basal",
        headers,
        serde_json::to_vec(&json!({"call_key":"call:1","outcome":"ok"}))
            .unwrap()
            .into(),
    )
    .await
    .unwrap()
    .await
    .unwrap();

    let health = wait_health(&reader, |h| h.notices + h.off_subscription == own + 4).await;
    assert_eq!(health.off_subscription, 0);
    assert_eq!(health.malformed, 0);
    assert_eq!(health.notices, own + 4);
    assert_eq!(health.admitted, 1);
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let info = consumer.info().await.unwrap();
    assert_eq!(info.num_ack_pending, 0);
    assert_eq!(info.ack_floor.stream_sequence, own + 4);
    transport.assert_no_reader_provider_calls();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_reader_parks_and_outbox_commit_wakes_publisher() {
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    let reader = Reader::start(rt.clone(), transport);
    wait_health(&reader, |h| h.notices > 0 && h.outbox_passes_for_test > 0).await;
    // Let the publisher consume its own acknowledgement's commit signal.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = reader.health();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let idle = reader.health();
    assert_eq!(idle.pull_requests_for_test, before.pull_requests_for_test);
    assert_eq!(idle.backlog_polls_for_test, before.backlog_polls_for_test);
    assert_eq!(idle.outbox_passes_for_test, before.outbox_passes_for_test);
    rt.store()
        .write(|tx| {
            basal_core::bus_outbox::enqueue_late_result_wake(
                tx,
                "reserved:prefrontal",
                "call:123",
                "ok",
                "settlement:123",
                1000,
            )
        })
        .unwrap();
    let published = wait_health(&reader, |h| {
        h.outbox_passes_for_test > idle.outbox_passes_for_test
    })
    .await;
    assert!(published.connected);
    assert!(published.outbox_error.is_none(), "{published:?}");
    assert!(rt.pending_bus_events(32).unwrap().is_empty());
    drop(reader);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_idle_reader_reconnects_and_publishes_committed_rows() {
    let mut server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    let reader = Reader::start(rt.clone(), transport);
    wait_health(&reader, |h| h.notices > 0).await;
    // Leave the retained batch and its acknowledgements behind so the reader
    // is parked on the same fetch when the server disappears.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let parked = reader.health();
    assert!(parked.connected, "{parked:?}");
    let stopped_at = Instant::now();
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    // Disconnect notification must cancel the parked fetch, not wait for its
    // 60-second expiry. Ten seconds leaves room for loaded CI machines.
    let until = Instant::now() + Duration::from_secs(10);
    while reader.health().connected {
        assert!(Instant::now() < until, "disconnection did not reach health");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let disconnected = reader.health();
    println!(
        "parked reader noticed disconnection in {:?}",
        stopped_at.elapsed()
    );
    assert_eq!(
        disconnected.disconnect_wakes_for_test,
        parked.disconnect_wakes_for_test + 1,
        "the connection-state notification must end the session"
    );
    assert_eq!(
        disconnected.pull_requests_for_test, parked.pull_requests_for_test,
        "the parked fetch must end without expiring and starting another pull"
    );
    assert!(disconnected.disconnected_since_ms.is_some());
    rt.store()
        .write(|tx| {
            basal_core::bus_outbox::enqueue_late_result_wake(
                tx,
                "reserved:prefrontal",
                "call:124",
                "ok",
                "settlement:124",
                1000,
            )
        })
        .unwrap();
    let port = server.url.rsplit(':').next().unwrap();
    let log = std::fs::OpenOptions::new()
        .append(true)
        .open(server.dir.join("server.log"))
        .unwrap();
    server.child = std::process::Command::new("nats-server")
        .args(["-js", "-a", "127.0.0.1", "-p", port, "-sd"])
        .arg(server.dir.join("jetstream"))
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    wait_health(&reader, |h| h.connected && h.error.is_none()).await;
    let until = Instant::now() + Duration::from_secs(10);
    while !rt.pending_bus_events(32).unwrap().is_empty() {
        assert!(
            Instant::now() < until,
            "reconnect did not drain the committed outbox"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(reader);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_bus_row_advances_retry_and_publisher_parks() {
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    // An empty JSON object satisfies SQLite's JSON constraint but lacks the
    // call key, outcome and event id needed to publish a late-result wake.
    rt.store().write(|tx| {
        tx.execute("INSERT INTO outbox(at,kind,flow_id,recipient,body,bus_retry_at) VALUES (1000,'codemode.late_result_ready','','prefrontal','{}',1000)", [])?;
        Ok(())
    }).unwrap();
    let reader = Reader::start(rt.clone(), transport);
    wait_health(&reader, |h| h.outbox_passes_for_test >= 2).await;
    let retry: (i64,u32) = rt.store().read(|c| Ok(c.query_row("SELECT bus_retry_at,bus_attempts FROM outbox WHERE kind='codemode.late_result_ready'", [], |r| Ok((r.get(0)?,r.get(1)?)))?)).unwrap();
    assert_eq!(retry, (1250, 1));
    assert!(rt.pending_bus_events(32).unwrap().is_empty());
    assert_eq!(rt.next_bus_event_at().unwrap(), Some(1250));
    let passes = reader.health().outbox_passes_for_test;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        reader.health().outbox_passes_for_test - passes <= 2,
        "a malformed row must not cause a tight publish loop"
    );
    drop(reader);
    rt.config().clock.set(1250);
    assert!(rt.pending_bus_events(32).is_err());
    assert_eq!(rt.next_bus_event_at().unwrap(), Some(1750));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_panic_reports_dead_health_and_restarts() {
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    let reader = Reader::start(rt.clone(), transport.clone());
    wait_health(&reader, |h| h.connected).await;
    transport.panic_once.store(true, Ordering::SeqCst);
    publish(&server, 1, 2).await;
    let health = wait_health(&reader, |h| {
        h.error
            .as_ref()
            .is_some_and(|e| e.contains("injected reader catalog panic"))
    })
    .await;
    assert!(!health.connected, "{health:?}");
    assert!(
        health
            .error
            .as_ref()
            .unwrap()
            .starts_with("reader stopped:")
    );
    assert!(health.retry_in_ms >= 250);
    assert_eq!(health.admitted, 0);
    assert!(health.backlog >= 1);
    transport.pause_restart.store(false, Ordering::SeqCst);
    let health = wait_health(&reader, |h| h.admitted == 1).await;
    assert!(health.connected);
    assert!(health.error.is_none());
    assert_eq!(run_count(&rt), 1);
    publish(&server, 2, 2).await;
    wait_health(&reader, |h| h.admitted == 2).await;
    drop(reader);
    transport.assert_no_reader_provider_calls();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_foreign_malformed_notice_is_debug_and_counts_malformed() {
    let logs = captured_logs();
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    let reader = Reader::start(rt, transport.clone());
    let subject = "ck.account.event.subc.activity.v3";
    let key = format!("{:064x}", 41);
    super::v2::publish(&server, subject, &format!("{key}.v3"), Some(&key)).await;

    let health = wait_health(&reader, |h| h.malformed == 1).await;
    assert_eq!(health.malformed, 1);
    assert_eq!(health.admitted, 0);
    assert!(
        !logged_at(
            logs,
            tracing::Level::WARN,
            subject,
            "undeclared event version"
        ),
        "unsubscribed malformed notice must not warn"
    );
    assert!(logged_at(
        logs,
        tracing::Level::DEBUG,
        subject,
        "undeclared event version"
    ));
    drop(reader);
    transport.assert_no_reader_provider_calls();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_subscribed_malformed_notice_still_warns() {
    let logs = captured_logs();
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    let reader = Reader::start(rt, transport.clone());
    // A space is outside the printable-ASCII key rule.
    let key = "not valid";
    super::v2::publish(&server, SUBJECT_V2, &format!("{key}.v2"), Some(key)).await;

    let health = wait_health(&reader, |h| h.malformed == 1).await;
    assert_eq!(health.malformed, 1);
    assert_eq!(health.admitted, 0);
    assert!(logged_at(
        logs,
        tracing::Level::WARN,
        SUBJECT_V2,
        "invalid event key"
    ));
    drop(reader);
    transport.assert_no_reader_provider_calls();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_refuses_undeclared_or_unreadable_versions() {
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    let reader = Reader::start(rt.clone(), transport.clone());
    publish(&server, 1, 3).await;
    wait_health(&reader, |h| h.malformed == 1).await;
    transport.unreadable.store(true, Ordering::SeqCst);
    publish(&server, 2, 2).await;
    let health = wait_health(&reader, |h| h.malformed == 2).await;
    assert_eq!(health.admitted, 0);
    assert_eq!(run_count(&rt), 0);
    assert!(health.connected);
    assert!(health.error.is_none());
    drop(reader);
    transport.assert_no_reader_provider_calls();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_health_counts_model_wakes() {
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    rt.store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO broca_waits (send_id, broca_run_id, check_at) VALUES ('send-1', 'run-1', 0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let reader = Reader::start(rt.clone(), transport.clone());
    server
        .publish_with_headers(
            7,
            "ck.account.event.broca.run.ended.v1",
            std::collections::BTreeMap::from([("run_id".into(), "run-1".into())]),
        )
        .await;
    let health = wait_health(&reader, |h| h.model_wakes == 1).await;
    assert_eq!(health.admitted, 0);
    assert!(health.error.is_none(), "{health:?}");
    drop(reader);
    transport.assert_no_reader_provider_calls();
}
