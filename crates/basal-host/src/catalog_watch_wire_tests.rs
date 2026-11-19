//! The catalog watch against a fake daemon that answers `catalog.list` and,
//! when asked to, `catalog.watch`. Each test counts the `catalog.list`
//! requests the daemon receives.

use super::*;
use crate::Catalog;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::connection_file::{ConnectionInfo, Endpoint, SCHEMA_VERSION, write_atomic};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::io::AsyncWriteExt;

#[derive(Default)]
struct Daemon {
    /// The generation `catalog.list` reports and pushes announce.
    generation: AtomicU64,
    /// Whether the daemon serves `catalog.watch` or refuses it.
    watch: AtomicBool,
    /// Whether `provider` is in the catalog.
    provider: AtomicBool,
    opens: Mutex<Vec<Value>>,
    lists: AtomicUsize,
    watches: AtomicUsize,
}

impl Daemon {
    fn list(&self) -> Value {
        let mut modules = vec![json!({"module_id":"prefrontal","roles":[],"control_ops":[]})];
        if self.provider.load(Ordering::SeqCst) {
            modules.push(json!({"module_id":"provider","control_ops":[],
                "capabilities":{"provides":["flow-scopes/v1"]},"roles":[
                {"role":"management_surface","operations":[{"name":"read","kind":"query"}],
                 "config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}]}));
        }
        json!({"op":"catalog.list","generation":self.generation.load(Ordering::SeqCst),
            "modules":modules,"subc_ops":["catalog.list","catalog.watch"]})
    }
}

fn frame(ty: FrameType, corr: u64, body: &Value) -> Frame {
    Frame::build(
        ty,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        corr,
        serde_json::to_vec(body).unwrap(),
    )
    .unwrap()
}

struct Wire {
    daemon: Arc<Daemon>,
    transport: Arc<SubcTransport>,
    /// Sends a watch push announcing the given generation.
    push: tokio::sync::mpsc::UnboundedSender<u64>,
    /// Keeps the fake daemon's runtime alive for the test.
    _runtime: tokio::runtime::Runtime,
    dir: std::path::PathBuf,
}

impl Drop for Wire {
    fn drop(&mut self) {
        self.transport.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wire(watch: bool) -> Wire {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "basal-catalog-watch-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("subc-connection.json");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
        .unwrap();
    let info = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        }],
        key: vec![7; 32],
        daemon_id: [9; 16],
        pid: std::process::id(),
        daemon_ver: "0.44.5".into(),
    };
    write_atomic(&file, &info).unwrap();
    let daemon = Arc::new(Daemon {
        generation: AtomicU64::new(1),
        watch: AtomicBool::new(watch),
        ..Daemon::default()
    });
    let (push, mut pushes) = tokio::sync::mpsc::unbounded_channel::<u64>();
    let state = daemon.clone();
    runtime.spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        authenticate_server(
            &mut stream,
            &info.key,
            &info.daemon_id,
            "0.44.5",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        let (mut reader, writer) = tokio::io::split(stream);
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        let watch_corr = Arc::new(Mutex::new(None::<u64>));
        let push_writer = writer.clone();
        let push_corr = watch_corr.clone();
        let push_state = state.clone();
        tokio::spawn(async move {
            while let Some(generation) = pushes.recv().await {
                push_state.generation.store(generation, Ordering::SeqCst);
                let corr = *push_corr.lock().unwrap();
                if let Some(corr) = corr {
                    let body = json!({"op":"catalog.watch","generation":generation});
                    let mut writer = push_writer.lock().await;
                    write_frame(&mut *writer, &frame(FrameType::StreamData, corr, &body))
                        .await
                        .unwrap();
                    writer.flush().await.unwrap();
                }
            }
        });
        while let Ok(Some(request)) = read_frame(&mut reader).await {
            if request.header.ty != FrameType::Request {
                continue;
            }
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let corr = request.header.corr;
            let reply = if request.header.channel != 0 {
                Frame::build(
                    FrameType::Response,
                    Flags::new(false, Priority::Interactive, false),
                    request.header.channel,
                    request.header.epoch,
                    corr,
                    serde_json::to_vec(&json!({"result":{"ok":true}})).unwrap(),
                )
                .unwrap()
            } else {
                match body["op"].as_str() {
                    Some("route.open") => {
                        state.opens.lock().unwrap().push(body.clone());
                        frame(
                            FrameType::Response,
                            corr,
                            &json!({"op":"route.open","route_channel":11,"route_epoch":1}),
                        )
                    }
                    Some("catalog.list") => {
                        state.lists.fetch_add(1, Ordering::SeqCst);
                        frame(FrameType::Response, corr, &state.list())
                    }
                    Some("catalog.watch") if state.watch.load(Ordering::SeqCst) => {
                        state.watches.fetch_add(1, Ordering::SeqCst);
                        *watch_corr.lock().unwrap() = Some(corr);
                        let generation = state.generation.load(Ordering::SeqCst);
                        frame(
                            FrameType::StreamData,
                            corr,
                            &json!({"op":"catalog.watch","generation":generation}),
                        )
                    }
                    Some("catalog.watch") => {
                        state.watches.fetch_add(1, Ordering::SeqCst);
                        frame(
                            FrameType::Error,
                            corr,
                            &json!({"code":"unknown_op","message":"no catalog.watch here"}),
                        )
                    }
                    _ => frame(FrameType::Response, corr, &json!({"result":null})),
                }
            };
            let mut writer = writer.lock().await;
            write_frame(&mut *writer, &reply).await.unwrap();
            writer.flush().await.unwrap();
        }
    });
    let transport =
        SubcTransport::connect_with_file(Duration::from_secs(10), Some(&file), None).unwrap();
    Wire {
        daemon,
        transport,
        push,
        _runtime: runtime,
        dir,
    }
}

/// Polls `done` until it holds, failing after a generous deadline.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until the watch is open and its first generation has been read.
fn watching(wire: &Wire) {
    eventually("the watch's first read", || {
        wire.transport.shared_catalog.current().is_some()
    });
}

#[test]
fn a_generation_push_triggers_exactly_one_reread_and_feeds_change_subscribers() {
    let wire = wire(true);
    let changes = Arc::new(AtomicUsize::new(0));
    let seen = changes.clone();
    wire.transport.on_catalog_change(Arc::new(move || {
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    watching(&wire);
    assert_eq!(wire.daemon.lists.load(Ordering::SeqCst), 1);
    let changes_before = changes.load(Ordering::SeqCst);

    wire.push.send(2).unwrap();
    eventually("the push's read", || {
        wire.transport
            .shared_catalog
            .current()
            .is_some_and(|snapshot| snapshot.generation == 2)
    });
    eventually("the change notice", || {
        changes.load(Ordering::SeqCst) == changes_before + 1
    });
    // A repeated announcement of the generation already held reads nothing.
    wire.push.send(2).unwrap();
    wire.push.send(2).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        wire.daemon.lists.load(Ordering::SeqCst),
        2,
        "one push of a new generation costs exactly one catalog read"
    );
    assert_eq!(wire.transport.catalog_snapshot().unwrap().generation, 2);
    assert_eq!(wire.daemon.lists.load(Ordering::SeqCst), 2);
    assert_eq!(changes.load(Ordering::SeqCst), changes_before + 1);
}

#[test]
fn requests_between_pushes_make_no_catalog_reads() {
    let wire = wire(true);
    watching(&wire);
    let before = wire.daemon.lists.load(Ordering::SeqCst);
    let catalog = crate::subc_catalog::SubcCatalog::new(wire.transport.clone());
    for _ in 0..100 {
        let snapshot = catalog.request_snapshot().unwrap();
        assert_eq!(snapshot.lookup_op("prefrontal", "read"), Ok(None));
        assert_eq!(wire.transport.catalog_snapshot().unwrap().generation, 1);
    }
    assert_eq!(
        wire.daemon.lists.load(Ordering::SeqCst),
        before,
        "requests share the watch-fed copy"
    );
}

#[test]
fn core_route_opens_without_a_catalog_read() {
    use crate::Host;

    let wire = wire(false);
    let before = wire.daemon.lists.load(Ordering::SeqCst);
    crate::core_host::CoreHost::new(wire.transport.clone())
        .owner_notice(json!({"flow_id":"flow"}))
        .unwrap();
    let opens = wire.daemon.opens.lock().unwrap();
    assert_eq!(opens.len(), 1);
    assert_eq!(opens[0]["target"]["module_id"], "prefrontal");
    assert_eq!(wire.daemon.lists.load(Ordering::SeqCst), before);
}

#[test]
fn without_a_watch_every_request_reads_the_catalog() {
    let wire = wire(false);
    eventually("the refused watch", || {
        wire.daemon.watches.load(Ordering::SeqCst) >= 1
    });
    for n in 1..=100u64 {
        wire.daemon.generation.store(n, Ordering::SeqCst);
        assert_eq!(
            wire.transport.catalog_snapshot().unwrap().generation,
            n,
            "each request sees the daemon's current catalog"
        );
    }
    assert_eq!(wire.daemon.lists.load(Ordering::SeqCst), 100);
}

#[test]
fn a_module_missing_from_the_shared_copy_is_read_once_more() {
    let wire = wire(true);
    watching(&wire);
    let catalog = crate::subc_catalog::SubcCatalog::new(wire.transport.clone());
    // The provider registers, and its generation's push has not arrived.
    wire.daemon.provider.store(true, Ordering::SeqCst);
    let before = wire.daemon.lists.load(Ordering::SeqCst);
    assert!(catalog.lookup_op("provider", "read").unwrap().is_some());
    assert_eq!(wire.daemon.lists.load(Ordering::SeqCst), before + 1);
    assert!(catalog.lookup_op("provider", "read").unwrap().is_some());
    assert_eq!(
        wire.daemon.lists.load(Ordering::SeqCst),
        before + 1,
        "the refreshed copy is shared"
    );
    // A module that is really absent costs one read per miss, not a loop.
    assert!(matches!(
        catalog.lookup_op("absent", "read"),
        Err(WireError::Typed(_))
    ));
    assert_eq!(wire.daemon.lists.load(Ordering::SeqCst), before + 2);
}

#[test]
fn calls_before_the_connection_opens_wait_and_then_refuse_as_never_sent() {
    let transport = SubcTransport::deferred(Duration::from_millis(50)).unwrap();
    assert!(matches!(
        transport.management("prefrontal", "agent.list", json!({})),
        Err(WireError::NeverSent(message)) if message.contains("not open yet")
    ));
    assert!(matches!(
        transport.catalog_snapshot(),
        Err(WireError::NeverSent(_))
    ));
    transport.shutdown();
    assert!(matches!(
        transport.catalog_snapshot(),
        Err(WireError::NeverSent(message)) if message.contains("closed")
    ));
}
