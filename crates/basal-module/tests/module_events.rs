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

mod common;
#[path = "module_events/lifecycle.rs"]
mod lifecycle;
#[path = "module_events/reader.rs"]
mod reader;
#[path = "module_events/v2.rs"]
mod v2;

static NEXT: AtomicUsize = AtomicUsize::new(0);
const STREAM: &str = "CK_ACCOUNT_EVENT";
const SUBJECT: &str = "ck.account.event.plexus.pull_request_review.v1";

#[cfg(windows)]
#[path = "module_events/windows.rs"]
mod windows;
#[cfg(unix)]
use std::process::Child;
#[cfg(windows)]
use windows::{Child, start_server};

/// Arguments for a test server that picks its own free port (`-p -1`) and
/// writes the address it bound to a ports file in `dir`. Reserving a port in
/// the test and passing it on would leave a window in which another process
/// takes the port first, and the server then fails with "address already in use".
fn server_args(dir: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        ["-js", "-a", "127.0.0.1", "-p", "-1", "--ports_file_dir"]
            .map(Into::into)
            .into();
    args.push(dir.into());
    args.push("-sd".into());
    args.push(dir.join("jetstream").into());
    args
}

/// The client address the server wrote to its ports file, once it exists.
/// Each server has its own directory, so the one `.ports` file is this server's.
fn bound_client_url(dir: &std::path::Path) -> Option<String> {
    let entry = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .find(|e| e.path().extension().is_some_and(|x| x == "ports"))?;
    let ports: serde_json::Value =
        serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
    ports["nats"][0].as_str().map(str::to_owned)
}

#[cfg(unix)]
fn start_server(dir: &std::path::Path, log: std::fs::File) -> Child {
    std::process::Command::new("nats-server")
        .args(server_args(dir))
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .expect(
            "REAL NATS TEST REQUIRES nats-server ON PATH; install pinned v2.15.0 (no silent skip)",
        )
}

struct Server {
    child: Child,
    dir: std::path::PathBuf,
    bus: NatsConnection,
    url: String,
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
        let log = std::fs::File::create(dir.join("server.log")).unwrap();
        let mut child = start_server(&dir, log);
        let until = Instant::now() + Duration::from_secs(10);
        let fail = |child: &mut Child, error: &dyn std::fmt::Display| -> ! {
            let _ = child.kill();
            panic!(
                "real NATS server failed to start: {error}; log {}",
                dir.join("server.log").display()
            );
        };
        let url = loop {
            if let Some(url) = bound_client_url(&dir) {
                break url;
            }
            if Instant::now() >= until || child.try_wait().unwrap().is_some() {
                fail(&mut child, &"no ports file was written");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        let bus = loop {
            match NatsConnection::connect(
                url.clone(),
                async_nats::ConnectOptions::new().connection_timeout(Duration::from_millis(200)),
                ConnectConfig::new("TEST").unwrap(),
            )
            .await
            {
                Ok(bus) => break bus,
                Err(error) => {
                    if Instant::now() >= until || child.try_wait().unwrap().is_some() {
                        fail(&mut child, &error);
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
        Self {
            child,
            dir,
            bus,
            url,
        }
    }
    async fn publish(&self, n: u64, subject: &str) {
        self.publish_with_headers(n, subject, Default::default())
            .await;
    }

    async fn publish_with_headers(
        &self,
        n: u64,
        subject: &str,
        headers: cortexkit_bus_trait::Headers,
    ) {
        let receipt = self
            .bus
            .stream(STREAM)
            .publish(
                subject,
                &format!("{n:064x}"),
                ContentDigest::of_bytes(b"{}"),
                headers,
            )
            .await
            .unwrap();
        assert!(!receipt.duplicate());
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
    runtime_with_catalog(path, MockCatalog::standard())
}

fn runtime_with_catalog(path: &std::path::Path, catalog: MockCatalog) -> Runtime {
    Runtime::new(
        Arc::new(Store::open(path, Durability::throwaway()).unwrap()),
        Arc::new(MockHost::new()),
        Arc::new(catalog),
        Arc::new(basal_core::NoHooks),
        None,
        Config {
            clock: Clock::manual(1000),
            install_gate: InstallGate::Off,
            ..Default::default()
        },
    )
}

const DELIVERED_SUBJECT: &str = "ck.account.event.prefrontal-core.digest_delivered.v1";

fn delivered_runtime(path: &std::path::Path) -> Runtime {
    let catalog = MockCatalog::standard();
    catalog.set_flow_capable("prefrontal-core", true);
    catalog.set_event(
        "prefrontal-core",
        "digest_delivered",
        1,
        basal_host::EventDecl {
            headers: vec![],
            origin: basal_host::EventOrigin::External,
            body: basal_host::EventBody::Resolved {
                resolve_op: basal_host::catalog::EVENT_BODY_OP.into(),
            },
        },
    );
    catalog.set_op(
        "prefrontal-core",
        basal_host::catalog::EVENT_BODY_OP,
        basal_host::OpDecl {
            kind: Some(basal_host::OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    let rt = runtime_with_catalog(path, catalog);
    for id in ["a", "b"] {
        let installed = rt.install(&InstallRequest {
            script: "return 1;".into(), author: "operator".into(), loop_override: false,
            manifest: json!({"id":id,"version":1,"purpose":"Receive digest delivery",
                "trigger":{"events":[{"module":"prefrontal-core","name":"digest_delivered","version":1}]},
                "ops":[{"module":"prefrontal-core","op":basal_host::catalog::EVENT_BODY_OP}]}).to_string(),
        }).unwrap();
        rt.approve(id, 1, &installed.code_hash, "test").unwrap();
    }
    rt
}

#[tokio::test]
async fn delivered_notice_targets_only_named_flow_and_acks_no_match() {
    let server = Server::start().await;
    let rt = delivered_runtime(&server.dir.join("basal.db"));
    server
        .publish_with_headers(
            0,
            DELIVERED_SUBJECT,
            std::collections::BTreeMap::from([
                ("flow_id".into(), "b".into()),
                ("agent".into(), "SYNAPSE".into()),
            ]),
        )
        .await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 1);
    let report = commit_batch(rt.clone(), &messages, true).await.unwrap();
    assert_eq!((report.matched, report.admitted), (1, 1));
    let flows: Vec<String> = rt
        .store()
        .read(|c| {
            Ok(c.prepare("SELECT flow_id FROM runs")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(flows, ["b"]);
    let receipts: Vec<String> = rt
        .store()
        .read(|c| {
            Ok(c.prepare("SELECT flow_id FROM event_receipts")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(
        receipts,
        ["b"],
        "other declaring flows are no-match, not backlog"
    );
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
    server
        .publish_with_headers(
            1,
            DELIVERED_SUBJECT,
            std::collections::BTreeMap::from([("flow_id".into(), "absent".into())]),
        )
        .await;
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(
        commit_batch(rt.clone(), &messages, true)
            .await
            .unwrap()
            .matched,
        0
    );
    assert_eq!(run_count(&rt), 1);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}

#[tokio::test]
async fn delivered_notice_missing_flow_header_is_malformed_and_acked() {
    let server = Server::start().await;
    let rt = delivered_runtime(&server.dir.join("basal.db"));
    server.publish(0, DELIVERED_SUBJECT).await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert!(
        basal_module::events::decode(&messages[0], rt.catalog())
            .unwrap_err()
            .contains("flow_id")
    );
    assert_eq!(
        commit_batch(rt.clone(), &messages, true)
            .await
            .unwrap()
            .matched,
        0
    );
    assert_eq!(run_count(&rt), 0);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
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
async fn header_filtered_notice_is_acked_without_receipt_and_redelivery_stays_idempotent() {
    let server = Server::start().await;
    let catalog = MockCatalog::standard();
    let mut event =
        basal_host::Catalog::event(&catalog, "plexus", "pull_request_review", 1).unwrap();
    event.headers = vec!["repo".into()];
    catalog.set_event("plexus", "pull_request_review", 1, event);
    let rt = runtime_with_catalog(&server.dir.join("basal.db"), catalog);
    let installed = rt
        .install(&InstallRequest {
            script: "return 1;".into(),
            manifest: json!({"id":"flow","version":1,"purpose":"Filter repository notices",
            "trigger":{"events":[{"module":"plexus","name":"pull_request_review","version":1,
            "match":{"repo":"cortexkit/*"}}]}})
            .to_string(),
            author: "local:unverified".into(),
            loop_override: false,
        })
        .unwrap();
    rt.approve("flow", 1, &installed.code_hash, "test").unwrap();
    server
        .publish_with_headers(1, SUBJECT, [("repo".into(), "other/repo".into())].into())
        .await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    let report = commit_batch(rt.clone(), &messages, true).await.unwrap();
    assert_eq!(
        (report.filtered_out, report.admitted, report.queued),
        (1, 0, 0)
    );
    assert_eq!(run_count(&rt), 0);
    let receipts: i64 = rt
        .store()
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event_receipts", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(receipts, 0);
    assert_eq!(rt.flow_health().unwrap()[0].filtered_out, 1);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);

    server
        .publish_with_headers(
            2,
            SUBJECT,
            [("repo".into(), "CortexKit/basal".into())].into(),
        )
        .await;
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(
        commit_batch(rt.clone(), &messages, false)
            .await
            .unwrap()
            .admitted,
        1
    );
    assert_eq!(
        commit_batch(rt.clone(), &messages, true)
            .await
            .unwrap()
            .duplicate,
        1
    );
    assert_eq!(run_count(&rt), 1);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
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
    let rt = runtime(&server.dir.join("basal.db"));
    approve(&rt, "flow");
    for n in 0..200 {
        server.publish(n, SUBJECT).await;
    }
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

fn published_ms(message: &async_nats::jetstream::Message) -> i64 {
    i64::try_from(message.info().unwrap().published.unix_timestamp_nanos() / 1_000_000).unwrap()
}

#[tokio::test]
async fn notices_before_first_approval_are_acked_and_reported() {
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("basal.db"));
    // A publisher cannot turn old history into a new event by labelling it
    // with a later time. Only JetStream's stored publish time is authoritative.
    server
        .publish_with_headers(
            0,
            SUBJECT,
            std::collections::BTreeMap::from([("published_at_ms".into(), i64::MAX.to_string())]),
        )
        .await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    let old_time = published_ms(&messages[0]);
    rt.config().clock.set(old_time + 1);
    approve(&rt, "flow");
    let report = commit_batch(rt.clone(), &messages, true).await.unwrap();
    assert_eq!(report.skipped_before_install, 1);
    assert_eq!(report.matched, 0);
    assert_eq!(run_count(&rt), 0);
    let receipts: i64 = rt
        .store()
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event_receipts", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(receipts, 0);
    assert_eq!(rt.flow_health().unwrap()[0].skipped_before_install, 1);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
    assert_eq!(consumer.info().await.unwrap().num_pending, 0);
}

#[tokio::test]
async fn notices_published_after_first_approval_are_admitted() {
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("basal.db"));
    let approval = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    rt.config().clock.set(approval);
    approve(&rt, "flow");
    server.publish(0, SUBJECT).await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert!(published_ms(&messages[0]) >= approval);
    let report = commit_batch(rt.clone(), &messages, true).await.unwrap();
    assert_eq!(report.admitted, 1);
    assert_eq!(report.skipped_before_install, 0);
    assert_eq!(run_count(&rt), 1);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}

#[tokio::test]
async fn newer_version_keeps_first_approval_cutoff() {
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("basal.db"));
    approve(&rt, "flow");
    server.publish(0, SUBJECT).await;
    let consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    let between = published_ms(&messages[0]);
    assert!(between > rt.config().clock.now_ms());
    rt.config().clock.set(between + 1);
    let installed = rt.install(&InstallRequest {
        script: "return 2;".into(),
        manifest: json!({"id":"flow","version":2,"purpose":"Consume events after an upgrade","trigger":{"events":[{"module":"plexus","name":"pull_request_review","version":1}]}}).to_string(),
        author: "local:unverified".into(), loop_override: false,
    }).unwrap();
    rt.approve("flow", 2, &installed.code_hash, "test-v2")
        .unwrap();
    let report = commit_batch(rt.clone(), &messages, true).await.unwrap();
    assert_eq!(report.admitted, 1);
    assert_eq!(report.skipped_before_install, 0);
    let script: String = rt
        .store()
        .read(|c| Ok(c.query_row("SELECT script FROM runs", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(
        script, "return 2;",
        "eligible notices run the currently approved code"
    );
}
