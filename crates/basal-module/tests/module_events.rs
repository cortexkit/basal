//! Real JetStream tests provision the event stream and `m_basal` pull consumer.
//! The production reader only binds and acknowledges that existing consumer.
//! Short ack waits exercise redelivery without waiting the production 30 seconds.
use basal_core::{Clock, Config, Durability, InstallGate, InstallRequest, Runtime, Store};
use basal_host::{MockCatalog, mock::MockHost};
use basal_module::events::{bind, commit_batch, pull};
use cortexkit_bus_nats::{ConnectConfig, NatsConnection};
use cortexkit_bus_trait::{ContentDigest, Stream};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

static NEXT: AtomicUsize = AtomicUsize::new(0);
const STREAM: &str = "CK_ACCOUNT_EVENT";
const SUBJECT: &str = "ck.account.event.plexus.pull_request_review.v1";

struct Server {
    child: std::process::Child,
    dir: std::path::PathBuf,
    bus: NatsConnection,
}
impl Server {
    async fn start() -> Self {
        Self::with_ack_wait(Duration::from_millis(150)).await
    }

    async fn with_ack_wait(ack_wait: Duration) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "basal-nats-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let log = std::fs::File::create(dir.join("server.log")).unwrap();
        let mut child = std::process::Command::new("nats-server")
            .args(["-js","-a","127.0.0.1","-p", &port.to_string(),"-sd"])
            .arg(dir.join("jetstream")).stdout(log.try_clone().unwrap()).stderr(log)
            .spawn().expect("REAL NATS TEST REQUIRES nats-server ON PATH; install pinned v2.15.0 (no silent skip)");
        let until = Instant::now() + Duration::from_secs(10);
        let bus = loop {
            match NatsConnection::connect(
                format!("nats://127.0.0.1:{port}"),
                async_nats::ConnectOptions::new().connection_timeout(Duration::from_millis(200)),
                ConnectConfig::new("TEST").unwrap(),
            )
            .await
            {
                Ok(bus) => break bus,
                Err(error) => {
                    if Instant::now() >= until || child.try_wait().unwrap().is_some() {
                        let _ = child.kill();
                        panic!(
                            "real NATS server failed to start: {error}; log {}",
                            dir.join("server.log").display()
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        };
        let js = async_nats::jetstream::new(bus.client());
        let stream = js
            .create_stream(async_nats::jetstream::stream::Config {
                name: STREAM.into(),
                subjects: vec!["ck.account.event.>".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        stream
            .create_consumer(async_nats::jetstream::consumer::pull::Config {
                durable_name: Some("m_basal".into()),
                filter_subject: "ck.account.event.>".into(),
                ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                ack_wait,
                max_ack_pending: 1000,
                ..Default::default()
            })
            .await
            .unwrap();
        Self { child, dir, bus }
    }
    async fn publish(&self, n: u64, subject: &str) {
        self.bus
            .stream(STREAM)
            .publish(
                subject,
                &format!("{n:064x}"),
                ContentDigest::of_bytes(b"{}"),
                Default::default(),
            )
            .await
            .unwrap();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn runtime(path: &std::path::Path) -> Runtime {
    Runtime::new(
        Arc::new(Store::open(path, Durability { fullfsync: false }).unwrap()),
        Arc::new(MockHost::new()),
        Arc::new(MockCatalog::standard()),
        Arc::new(basal_core::NoHooks),
        None,
        Config {
            clock: Clock::manual(1000),
            install_gate: InstallGate::Off,
            ..Default::default()
        },
    )
}
fn approve(rt: &Runtime, id: &str) {
    let installed=rt.install(&InstallRequest {script:"return 1;".into(), manifest:json!({"id":id,"version":1,"purpose":"Consume retained events","trigger":{"events":[{"module":"plexus","name":"pull_request_review","version":1}]}}).to_string(),author:"local:unverified".into(),loop_override:false}).unwrap();
    rt.approve(id, 1, &installed.code_hash, "test").unwrap();
}
fn run_count(rt: &Runtime) -> i64 {
    rt.store()
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?))
        .unwrap()
}

#[tokio::test]
async fn journal_then_ack_crash_redelivers_without_second_run_across_restart() {
    let server = Server::start().await;
    let path = server.dir.join("basal.db");
    let rt = runtime(&path);
    approve(&rt, "flow");
    server.publish(0, SUBJECT).await;
    let consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        commit_batch(rt.clone(), &messages, false)
            .await
            .unwrap()
            .admitted,
        1
    );
    assert_eq!(run_count(&rt), 1, "commit precedes ack");
    drop(messages);
    drop(consumer);
    drop(rt);
    let rt = runtime(&path);
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let redelivery = pull(&consumer).await.unwrap();
    assert_eq!(redelivery.len(), 1);
    assert!(redelivery[0].info().unwrap().delivered >= 2);
    let report = commit_batch(rt.clone(), &redelivery, true).await.unwrap();
    assert_eq!((report.admitted, report.duplicate), (0, 1));
    assert_eq!(run_count(&rt), 1);
    let info = consumer.info().await.unwrap();
    assert_eq!(info.num_ack_pending, 0);
    assert_eq!(info.num_pending, 0);
}

#[tokio::test]
async fn no_match_is_acked_without_any_store_row() {
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("basal.db"));
    server.publish(0, SUBJECT).await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(
        commit_batch(rt.clone(), &messages, true)
            .await
            .unwrap()
            .matched,
        0
    );
    assert_eq!(run_count(&rt), 0);
    let receipts: i64 = rt
        .store()
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event_receipts", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(receipts, 0);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}

#[tokio::test]
async fn per_flow_fanout_rolls_back_before_ack_on_commit_failure() {
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("basal.db"));
    approve(&rt, "a");
    approve(&rt, "b");
    server.publish(0, SUBJECT).await;
    rt.store().write(|tx|{tx.execute_batch("CREATE TRIGGER refuse_second BEFORE INSERT ON runs WHEN NEW.flow_id='b' BEGIN SELECT RAISE(ABORT,'second flow failed'); END;")?;Ok(())}).unwrap();
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert!(commit_batch(rt.clone(), &messages, true).await.is_err());
    assert_eq!(run_count(&rt), 0);
    assert_eq!(
        consumer.info().await.unwrap().num_ack_pending,
        1,
        "failed commit must not ack"
    );
    rt.store()
        .write(|tx| {
            tx.execute_batch("DROP TRIGGER refuse_second")?;
            Ok(())
        })
        .unwrap();
    let redelivered = pull(&consumer).await.unwrap();
    assert_eq!(
        commit_batch(rt.clone(), &redelivered, true)
            .await
            .unwrap()
            .admitted,
        2
    );
    assert_eq!(run_count(&rt), 2);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}

#[tokio::test]
async fn deliver_all_first_batch_is_bounded_and_history_cannot_flood_runs() {
    let server = Server::start().await;
    for n in 0..200 {
        server.publish(n, SUBJECT).await;
    }
    let rt = runtime(&server.dir.join("basal.db"));
    approve(&rt, "flow");
    let consumer = bind(&server.bus, STREAM).await.unwrap();
    let start = Instant::now();
    let first = pull(&consumer).await.unwrap();
    assert_eq!(first.len(), basal_module::events::BATCH);
    assert_eq!(
        first[0].info().unwrap().stream_sequence,
        1,
        "deliver all starts at oldest retained notice"
    );
    let report = commit_batch(rt.clone(), &first, true).await.unwrap();
    assert_eq!(report.admitted, 32);
    eprintln!(
        "first retained batch: {} notices, {} admitted, {} ms, oldest sequence {}",
        first.len(),
        report.admitted,
        start.elapsed().as_millis(),
        first[0].info().unwrap().stream_sequence
    );
    let mut notices = first.len();
    while notices < 200 {
        let messages = pull(&consumer).await.unwrap();
        assert!(!messages.is_empty());
        notices += messages.len();
        commit_batch(rt.clone(), &messages, true).await.unwrap();
    }
    assert_eq!(run_count(&rt), 60);
    let health = rt.flow_health().unwrap();
    assert_eq!(health[0].event_backlog, 120);
    assert_eq!(health[0].event_overflow, 20);
}

#[tokio::test]
async fn slow_commit_sends_progress_before_ack_wait_expires() {
    let server = Server::with_ack_wait(Duration::from_secs(6)).await;
    let rt = runtime(&server.dir.join("basal.db"));
    approve(&rt, "flow");
    server.publish(0, SUBJECT).await;
    let consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    let (locked, acquired) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let held_runtime = rt.clone();
    // Hold the writer without changing data so the commit remains pending.
    let holder = std::thread::spawn(move || {
        held_runtime
            .store()
            .write(|_| {
                locked.send(()).unwrap();
                released.recv().unwrap();
                Ok(())
            })
            .unwrap()
    });
    acquired.recv_timeout(Duration::from_secs(2)).unwrap();
    let committing = tokio::spawn(async move { commit_batch(rt, &messages, true).await });
    tokio::time::sleep(Duration::from_secs(7)).await;
    let redelivery = pull(&consumer).await.unwrap();
    release.send(()).unwrap();
    holder.join().unwrap();
    committing.await.unwrap().unwrap();
    assert!(
        redelivery.is_empty(),
        "progress ack must prevent redelivery during a slow commit"
    );
}
