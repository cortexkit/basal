//! The actual subc serve loop refuses an unreadable control request without
//! dropping the module connection, so the following health probe can complete.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use basal_module::manifest::manifest;
use basal_module::serve::BasalHandler;
use serde_json::json;
use subc_protocol::scope::{ScopeAttributes, ScopeKind, ScopeStamp};
use subc_protocol::session::{HealthStatus, ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, PROTOCOL_VERSION, Principal, Priority, RouteTarget,
};
use subc_transport::auth::authenticate_server;
use subc_transport::connection_file::{ConnectionInfo, Endpoint, SCHEMA_VERSION, write_atomic};
use subc_transport::{read_frame, write_frame};
use tokio::net::TcpListener;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
const KEY: [u8; 32] = [0x5a; 32];
const DAEMON_ID: [u8; 16] = [0x2b; 16];

struct TempDirectory(std::path::PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "basal-unknown-control-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create private test directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("restrict test directory permissions");
        }
        Self(path)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

fn route_bind_with_future_scope_field() -> Vec<u8> {
    let scope = ScopeStamp {
        owner: Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        scope_ref: "scope-1".into(),
        scope_epoch: 1,
        kind: ScopeKind::Head,
        parent: None,
        parent_state: None,
        attributes: ScopeAttributes::new().with_agent_id(Some("SYNAPSE".into())),
        owner_authorized: true,
    };
    let request = ModuleControlRequest::RouteBind {
        route_channel: 7,
        epoch: 1,
        target: RouteTarget::ManagementSurface {
            module_id: "basal".into(),
        },
        identity: BindIdentity::new("/work/project", "test", "session-1"),
        principal: Some(Principal::Direct),
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: None,
        scope: Some(scope),
    };
    let mut value = serde_json::to_value(request).expect("encode valid route bind");
    value["scope"]["attributes"]["future_scope_fact"] = json!("future daemon value");
    serde_json::to_vec(&value).expect("encode route bind with an unknown field")
}

#[tokio::test]
async fn unknown_control_field_is_refused_and_the_following_health_check_is_answered() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listen");
    let address = listener.local_addr().expect("listener address");
    let temp = TempDirectory::new();
    let connection_file = temp.0.join("subc-connection.json");
    write_atomic(
        &connection_file,
        &ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: address.ip().to_string(),
                port: address.port(),
            }],
            key: KEY.to_vec(),
            daemon_id: DAEMON_ID,
            pid: std::process::id(),
            daemon_ver: "test-daemon".into(),
        },
    )
    .expect("write connection file");

    let storage = cortexkit_store_types::StorageDescriptor {
        module_id: "basal".into(),
        storage_namespace: "core".into(),
        isolation: cortexkit_store_types::Isolation::Module,
        backend: cortexkit_store_types::StorageBackend::Sqlite {
            path: temp.0.join("store.db").to_string_lossy().into_owned(),
        },
    };

    let daemon = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("module connects");
        authenticate_server(
            &mut stream,
            &KEY,
            &DAEMON_ID,
            "test-daemon",
            Duration::from_secs(2),
        )
        .await
        .expect("authenticate module");

        let hello = read_frame(&mut stream)
            .await
            .expect("read module HELLO")
            .expect("module sent HELLO");
        assert_eq!(hello.header.ty, FrameType::Hello);
        let ack = Frame::build(
            FrameType::HelloAck,
            flags(),
            0,
            0,
            hello.header.corr,
            serde_json::to_vec(&json!({
                "negotiated_ver": PROTOCOL_VERSION,
                "subc_ops": [],
                "subc_capabilities": [],
                "storage": storage,
                "machine_id": null
            }))
            .expect("encode HELLO_ACK"),
        )
        .expect("build HELLO_ACK");
        write_frame(&mut stream, &ack)
            .await
            .expect("write HELLO_ACK");

        let request = Frame::build(
            FrameType::Request,
            flags(),
            0,
            0,
            41,
            route_bind_with_future_scope_field(),
        )
        .expect("build control request with an unknown scope field");
        write_frame(&mut stream, &request)
            .await
            .expect("write future control request");
        let refusal = read_frame(&mut stream)
            .await
            .expect("read control refusal")
            .expect("module answered the unknown request");
        assert_eq!(refusal.header.ty, FrameType::Error);
        assert_eq!(refusal.header.corr, 41);
        let refusal: subc_protocol::ErrorBody =
            serde_json::from_slice(&refusal.body).expect("decode refusal");
        assert_eq!(refusal.code, "invalid_request");
        assert!(refusal.message.contains("future_scope_fact"));

        let health_request = Frame::build(
            FrameType::Request,
            flags(),
            0,
            0,
            42,
            serde_json::to_vec(&ModuleControlRequest::HealthCheck {})
                .expect("encode health request"),
        )
        .expect("build health request");
        write_frame(&mut stream, &health_request)
            .await
            .expect("write health request after refusal");
        let health = read_frame(&mut stream)
            .await
            .expect("read health response")
            .expect("module answered health after the refusal");
        assert_eq!(health.header.ty, FrameType::Response);
        assert_eq!(health.header.corr, 42);
        let health: ModuleControlResponse =
            serde_json::from_slice(&health.body).expect("decode health response");
        let ModuleControlResponse::HealthCheck { status, .. } = health else {
            panic!("expected health response, got {health:?}");
        };
        // HELLO_ACK starts store preparation on another thread. Whichever
        // startup phase the probe observes, the connection must still answer.
        assert!(matches!(status, HealthStatus::Ok | HealthStatus::Degraded));

        let goodbye =
            Frame::build(FrameType::Goodbye, flags(), 0, 0, 0, Vec::new()).expect("build GOODBYE");
        write_frame(&mut stream, &goodbye)
            .await
            .expect("close module connection");
    });

    let handler = BasalHandler::new(
        Box::new(|store_path| {
            let mut pool = basal_module::pool::PoolConfig::new(
                "unused-worker",
                basal_module::process::WorkerLaunch::Plain,
            );
            pool.warm_spares = 0;
            let scratch = store_path.parent().unwrap().join("dry-run");
            basal_module::module::ModuleConfig {
                store_path,
                durability: basal_core::Durability { fullfsync: false },
                runtime: basal_core::Config::default(),
                pool,
                engine: Default::default(),
                dry_run: basal_module::dryrun::DryRunConfig::new(scratch),
            }
        }),
        Box::new(|| basal_module::module::Hosts {
            transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
            host: Arc::new(basal_host::mock::MockHost::new()),
            catalog: Arc::new(basal_host::MockCatalog::standard()),
            consent: Arc::new(basal_host::MockConsent::new()),
            hooks: Arc::new(basal_core::NoHooks),
        }),
    );
    let serving = subc_client_rs::serve_with(&connection_file, manifest(), handler);
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("basal serve loop exits after GOODBYE")
        .expect("serve loop returned an error");
    daemon.await.expect("fake daemon task completed");
}
