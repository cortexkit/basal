mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use basal_host::routing::RoutingHost;
use basal_host::subc_catalog::SubcCatalog;
use basal_module::scope_connection::ScopeConnection;
use basal_module::serve::BasalHandler;
use serde_json::{Value, json};
use subc_protocol::session::ModuleControlRequest;
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::connection_file::{ConnectionInfo, Endpoint, SCHEMA_VERSION, write_atomic};
use subc_transport::{authenticate_server, read_frame, write_frame};

fn frame(kind: FrameType, channel: u16, epoch: u32, corr: u64, body: Value) -> Frame {
    Frame::build(
        kind,
        Flags::new(false, Priority::Passive, false),
        channel,
        epoch,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

#[test]
#[cfg_attr(
    windows,
    ignore = "codemode is refused on Windows (unsupported_platform)"
)]
fn codemode_handler_attests_via_real_sdk_and_replaces_the_describe_connection() {
    // A single-thread executor makes an accidental block_on from handle()
    // fail, rather than letting a spare Tokio worker conceal the mistake.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = common::scratch("codemode-sdk-scope");
    let store_path = dir.join("basal.db");
    let connection = ScopeConnection::default();
    let host_connection = connection.clone();
    let descriptions = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (built, builds) = std::sync::mpsc::channel();
    let handler = BasalHandler::new(
        Box::new(|store_path| {
            let mut pool = common::pool_config(&common::Options::default());
            pool.warm_spares = 0;
            basal_module::module::ModuleConfig {
                dry_run: basal_module::dryrun::DryRunConfig::new(
                    store_path.parent().unwrap().join("dry-run"),
                ),
                store_path,
                pool,
                durability: basal_core::Durability { fullfsync: false },
                runtime: basal_core::Config {
                    clock: basal_core::Clock::manual(common::T0),
                    ..Default::default()
                },
                engine: Default::default(),
            }
        }),
        Box::new(move || {
            built.send(()).unwrap();
            let transport = Arc::new(basal_module::unconfigured::UnconfiguredTransport);
            let catalog = Arc::new(SubcCatalog::new(transport.clone()));
            let ops = host_connection.module_ops(transport.clone(), catalog);
            let mock = Arc::new(basal_host::mock::MockHost::new());
            basal_module::module::Hosts {
                host: Arc::new(RoutingHost::new(ops, mock.clone(), mock)),
                transport,
                catalog: Arc::new(basal_host::MockCatalog::standard()),
                consent: Arc::new(basal_host::MockConsent::new()),
                hooks: Arc::new(basal_core::NoHooks),
            }
        }),
    );
    for session in 1..=2 {
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let info = ConnectionInfo {
                schema: SCHEMA_VERSION, wire_version: None,
                endpoints: vec![Endpoint { host: "127.0.0.1".into(), port: listener.local_addr().unwrap().port() }],
                key: vec![7; 32], daemon_id: [session; 16], pid: std::process::id(), daemon_ver: "codemode-sdk-test".into(),
            };
            let file = dir.join("connection.json");
            write_atomic(&file, &info).unwrap();
            let records = descriptions.clone();
            let path = store_path.clone();
            let daemon = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                authenticate_server(&mut stream, &info.key, &info.daemon_id, "codemode-sdk-test", Duration::from_secs(5)).await.unwrap();
                let hello = read_frame(&mut stream).await.unwrap().unwrap();
                assert_eq!(hello.header.ty, FrameType::Hello);
                let storage = cortexkit_store_types::StorageDescriptor {
                    module_id: "basal".into(), storage_namespace: "core".into(),
                    isolation: cortexkit_store_types::Isolation::Module,
                    backend: cortexkit_store_types::StorageBackend::Sqlite { path: path.to_string_lossy().into_owned() },
                };
                write_frame(&mut stream, &frame(FrameType::HelloAck, 0, 0, hello.header.corr, json!({
                    "negotiated_ver": subc_protocol::PROTOCOL_VERSION, "subc_ops": ["scope.describe"], "subc_capabilities": [],
                    "storage": storage
                }))).await.unwrap();
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                loop {
                    write_frame(&mut stream, &frame(FrameType::Request, 0, 0, 40, serde_json::to_value(ModuleControlRequest::HealthCheck {}).unwrap())).await.unwrap();
                    let health = read_frame(&mut stream).await.unwrap().unwrap();
                    let body: Value = serde_json::from_slice(&health.body).unwrap();
                    if body["status"] == "ok" { break; }
                    assert!(tokio::time::Instant::now() < deadline, "module did not become ready: {body}");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let bind = ModuleControlRequest::RouteBind {
                    route_channel: 7, epoch: 1,
                    target: subc_protocol::RouteTarget::ManagementSurface { module_id: "basal".into() },
                    identity: subc_protocol::BindIdentity::new("/work", "test", "session"),
                    principal: Some(subc_protocol::Principal::Reserved { module_id: "prefrontal-core".into() }),
                    consumer_capabilities: None, role_versions: None, admission_facts: None, scope: None,
                };
                write_frame(&mut stream, &frame(FrameType::Request, 0, 0, 41, serde_json::to_value(bind).unwrap())).await.unwrap();
                assert_eq!(read_frame(&mut stream).await.unwrap().unwrap().header.ty, FrameType::Response);
                let id = format!("sdk-run-{session}");
                let agent = format!("sdk-agent-{session}");
                write_frame(&mut stream, &frame(FrameType::Request, 7, 1, 99, json!({
                    "method":"codemode.run", "params": {"run_id":id, "agent_id":agent, "program":"return 1;", "catalog":[],
                        "scope":{"owner":{"kind":"reserved", "module_id":"prefrontal-core"}, "ref":"scope:wire", "epoch":19 + session as u64},
                        "deadline_ms":common::T0}
                }))).await.unwrap();
                let describe = read_frame(&mut stream).await.unwrap().unwrap();
                assert_eq!(describe.header.channel, 0);
                let body: Value = serde_json::from_slice(&describe.body).unwrap();
                assert_eq!(body, json!({"op":"scope.describe", "owner":{"kind":"reserved", "module_id":"prefrontal-core"}, "ref":"scope:wire"}));
                records.lock().unwrap().push(body);
                write_frame(&mut stream, &frame(FrameType::Response, 0, 0, describe.header.corr, json!({
                    "op":"scope.describe", "status":"live", "scope_epoch":19 + session as u64,
                    "daemon_incarnation":format!("daemon:{session}"), "owner_synced":true, "owner_configured":true,
                    "scope":{"owner":{"kind":"reserved", "module_id":"prefrontal-core"}, "ref":"scope:wire", "scope_epoch":19 + session as u64,
                        "kind":"head", "attributes":{"agent_id":agent, "run_id":id}, "owner_authorized":true}
                }))).await.unwrap();
                let response = read_frame(&mut stream).await.unwrap().unwrap();
                assert_eq!(response.header.ty, FrameType::Response, "{}", String::from_utf8_lossy(&response.body));
                assert_eq!(serde_json::from_slice::<Value>(&response.body).unwrap()["result"]["status"], "budget_exhausted:wall");
                write_frame(&mut stream, &Frame::build(FrameType::Goodbye, Flags::new(false, Priority::Passive, false), 0, 0, 0, Vec::new()).unwrap()).await.unwrap();
            });
            let (handle, serving) = subc_client_rs::serve_with_handle(&file, basal_module::manifest::manifest(), handler.clone()).await.unwrap();
            connection.attach(handle, runtime.handle().clone());
            tokio::time::timeout(Duration::from_secs(15), serving).await.unwrap().unwrap();
            daemon.await.unwrap();
        });
    }
    assert_eq!(descriptions.lock().unwrap().len(), 2);
    builds.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        matches!(
            builds.recv_timeout(Duration::from_secs(2)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "replacing the connection must not restart the module or reopen its store"
    );
    drop(handler);
    drop(connection);
    let store =
        basal_core::Store::open(&store_path, basal_core::Durability { fullfsync: false }).unwrap();
    for session in 1..=2 {
        let id = format!("sdk-run-{session}");
        store
            .read(|conn| {
                let basal_core::codemode::store::Lookup::Found(run) =
                    basal_core::codemode::store::lookup(conn, &id)?
                else {
                    panic!("missing admitted run")
                };
                assert_eq!(run.run_id, id);
                assert_eq!(run.agent_id, format!("sdk-agent-{session}"));
                Ok(())
            })
            .unwrap();
    }
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
