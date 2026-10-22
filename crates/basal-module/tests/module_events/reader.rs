use super::*;
use basal_host::subc_catalog::{DaemonCapabilities, SubcCatalog};
use basal_host::transport::{Transport, WireError};
use basal_module::events::Reader;
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

fn logged_at(logs: &CapturedLogs, level: tracing::Level, subject: &str, reason: &str) -> bool {
    logs.0.lock().unwrap().iter().any(|(seen_level, fields)| {
        *seen_level == level && fields.contains(subject) && fields.contains(reason)
    })
}

struct BlockingTransport {
    handle: tokio::runtime::Handle,
    url: String,
    panic_once: AtomicBool,
    pause_restart: AtomicBool,
    unreadable: AtomicBool,
    catalog_calls: AtomicUsize,
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

    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("reader must not resolve event bodies")
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
    );
    let install_rt = rt.clone();
    tokio::task::spawn_blocking(move || {
        let installed = install_rt.install(&InstallRequest {
            script: "return 1;".into(),
            manifest: json!({"id":"reader", "version":1, "purpose":"Consume declared v2 events",
                "trigger":{"events":[{"module":"plexus", "name":"pull_request_review", "version":2}]},
                "ops":[{"module":"plexus", "op":"events_get"}]}).to_string(),
            author: "local:unverified".into(),
            loop_override: false,
        }).unwrap();
        install_rt.approve("reader", 1, &installed.code_hash, "test").unwrap();
    }).await.unwrap();
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_foreign_malformed_notice_is_debug_and_counts_malformed() {
    let logs = captured_logs();
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    let reader = Reader::start(rt, transport);
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_subscribed_malformed_notice_still_warns() {
    let logs = captured_logs();
    let server = Server::start().await;
    let (rt, transport) = fixture(&server).await;
    let reader = Reader::start(rt, transport);
    let key = "invalid";
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
}
