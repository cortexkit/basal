use basal_host::builtins::BuiltinHost;
use basal_host::mock::MockHost;
use basal_host::routing::{ModuleOpsHost, RoutingHost};
use basal_host::scope_describe::ScopeCallError;
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{Transport, WireError};
use basal_host::{Host, ScopeDescribeError, ScopeDescription, ScopeStatus};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subc_protocol::Principal;

fn owner() -> Principal {
    Principal::Reserved {
        module_id: "prefrontal-core".into(),
    }
}

fn live_reply() -> Value {
    json!({
        "op": "scope.describe", "status": "live", "scope_epoch": 19,
        "daemon_incarnation": "daemon:attesting", "owner_synced": true,
        "owner_configured": true,
        "scope": {"owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": "scope:run", "scope_epoch": 19, "kind": "head",
            "attributes": {"agent_id": "agent:7", "flow_id": "flow:parent"},
            "owner_authorized": true}
    })
}

fn description() -> ScopeDescription {
    serde_json::from_value(live_reply()).unwrap()
}

struct NoTransport;
impl Transport for NoTransport {
    fn catalog(&self) -> Result<Value, WireError> {
        panic!("describe must not need a provider catalog")
    }
    fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
        panic!("describe is not a management route call")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("describe is not a tool route call")
    }
}

fn module_host() -> ModuleOpsHost {
    let transport = Arc::new(NoTransport);
    ModuleOpsHost::new(transport.clone(), Arc::new(SubcCatalog::new(transport)))
}

#[test]
fn a_host_without_describe_support_fails_closed() {
    let host: &dyn Host = &BuiltinHost::default();
    assert_eq!(
        host.scope_describe(&owner(), "scope:run"),
        Err(ScopeDescribeError::Unavailable)
    );
}

#[test]
fn an_unwired_module_host_fails_closed() {
    assert_eq!(
        module_host().scope_describe(&owner(), "scope:run"),
        Err(ScopeDescribeError::Unavailable)
    );
}

#[test]
fn mock_describe_is_injected_per_owner_and_ref() {
    let mock = MockHost::new();
    mock.set_scope_description(owner(), "scope:run", Ok(description()));
    let host: &dyn Host = &mock.clone();
    assert_eq!(
        host.scope_describe(&owner(), "scope:run"),
        Ok(description())
    );
    assert_eq!(
        host.scope_describe(&owner(), "scope:unknown"),
        Err(ScopeDescribeError::Unavailable)
    );
    let other_owner = Principal::Reserved {
        module_id: "other-core".into(),
    };
    assert_eq!(
        host.scope_describe(&other_owner, "scope:run"),
        Err(ScopeDescribeError::Unavailable)
    );
    assert_eq!(
        mock.scope_queries(),
        vec![
            (owner(), "scope:run".into()),
            (owner(), "scope:unknown".into()),
            (other_owner, "scope:run".into()),
        ]
    );
}

#[test]
fn routing_describe_uses_only_the_ops_host_and_keeps_typed_errors() {
    let ops = Arc::new(MockHost::new());
    let core = Arc::new(MockHost::new());
    let model = Arc::new(MockHost::new());
    let routing = RoutingHost::new(ops.clone(), core.clone(), model.clone());
    ops.set_scope_description(owner(), "scope:run", Ok(description()));
    assert_eq!(
        routing.scope_describe(&owner(), "scope:run"),
        Ok(description())
    );
    for error in [
        ScopeCallError::NotSupported {
            op: "scope.describe",
        },
        ScopeCallError::Refused {
            code: "no_scope".into(),
            message: "scope unavailable".into(),
        },
        ScopeCallError::Timeout,
        ScopeCallError::ConnectionClosed,
        ScopeCallError::Protocol("unreadable response".into()),
    ] {
        let expected = ScopeDescribeError::Daemon(error);
        ops.set_scope_description(owner(), "scope:run", Err(expected.clone()));
        assert_eq!(routing.scope_describe(&owner(), "scope:run"), Err(expected));
    }
    assert_eq!(ops.scope_queries().len(), 6);
    assert!(core.scope_queries().is_empty());
    assert!(model.scope_queries().is_empty());
}

#[test]
fn module_describe_preserves_live_and_non_live_answers() {
    let replies = [
        live_reply(),
        json!({"op":"scope.describe", "status":"ended", "scope_epoch":18,
            "daemon_incarnation":"daemon:old", "owner_synced":true,
            "owner_configured":true}),
        json!({"op":"scope.describe", "status":"not_live",
            "daemon_incarnation":"daemon:restarted", "owner_synced":false,
            "owner_configured":true}),
        json!({"op":"scope.describe", "status":"not_live",
            "daemon_incarnation":"daemon:unconfigured", "owner_synced":false,
            "owner_configured":false}),
    ];
    let expected: Vec<ScopeDescription> = replies
        .iter()
        .cloned()
        .map(|reply| serde_json::from_value(reply).unwrap())
        .collect();
    with_module(
        &["scope.describe"],
        replies
            .into_iter()
            .map(|v| (subc_protocol::FrameType::Response, v))
            .collect(),
        |host, calls| {
            for answer in &expected {
                assert_eq!(host.scope_describe(&owner(), "scope:run").unwrap(), *answer);
            }
            assert_eq!(expected[0].status, ScopeStatus::Live);
            let stamp = expected[0].scope.as_ref().unwrap();
            assert_eq!(stamp.scope_epoch, 19);
            assert_eq!(stamp.attributes.agent_id.as_deref(), Some("agent:7"));
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 4, "one daemon request per describe");
            for call in calls.iter() {
                assert_eq!(
                    *call,
                    json!({"op":"scope.describe", "owner":{"kind":"reserved", "module_id":"prefrontal-core"}, "ref":"scope:run"})
                );
            }
        },
    );
}

#[test]
fn module_describe_preserves_typed_daemon_failures() {
    use subc_protocol::FrameType;
    with_module(
        &["scope.describe"],
        vec![
            (
                FrameType::Error,
                json!({"code":"not_registered", "message":"module is gone"}),
            ),
            (
                FrameType::Response,
                json!({"op":"scope.describe", "status":"unrecognised"}),
            ),
            (FrameType::Response, json!({"op":"catalog.update"})),
        ],
        |host, calls| {
            assert_eq!(
                host.scope_describe(&owner(), "scope:run"),
                Err(ScopeDescribeError::Daemon(ScopeCallError::Refused {
                    code: "not_registered".into(),
                    message: "module is gone".into()
                }))
            );
            for _ in 0..2 {
                assert!(matches!(
                    host.scope_describe(&owner(), "scope:run"),
                    Err(ScopeDescribeError::Daemon(ScopeCallError::Protocol(_)))
                ));
            }
            assert_eq!(
                host.scope_describe(&owner(), "scope:run"),
                Err(ScopeDescribeError::Daemon(ScopeCallError::ConnectionClosed))
            );
            assert_eq!(calls.lock().unwrap().len(), 4);
        },
    );
}

#[test]
fn a_daemon_without_describe_support_is_a_typed_unsent_error() {
    with_module(&[], vec![], |host, calls| {
        assert_eq!(
            host.scope_describe(&owner(), "scope:run"),
            Err(ScopeDescribeError::Daemon(ScopeCallError::NotSupported {
                op: "scope.describe"
            }))
        );
        assert!(calls.lock().unwrap().is_empty());
    });
}

struct NoRequests;
#[subc_client_rs::async_trait]
impl subc_client_rs::ModuleHandler for NoRequests {
    async fn handle(
        &self,
        _: subc_client_rs::RequestCtx,
        _: Vec<u8>,
    ) -> subc_client_rs::HandlerOutcome {
        panic!("the describe fixture accepts only module-originated control calls")
    }
}

fn with_module(
    supported_ops: &[&str],
    replies: Vec<(subc_protocol::FrameType, Value)>,
    test: impl FnOnce(ModuleOpsHost, Arc<Mutex<Vec<Value>>>),
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use subc_protocol::{Flags, Frame, FrameType, ModuleHelloAckBody, Priority};
    use subc_transport::connection_file::{ConnectionInfo, Endpoint, SCHEMA_VERSION, write_atomic};
    use subc_transport::{authenticate_server, read_frame, write_frame};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "basal-describe-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
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
        daemon_ver: "describe-test".into(),
    };
    write_atomic(&file, &info).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let records = calls.clone();
    let supported_ops: Vec<String> = supported_ops.iter().map(|op| (*op).into()).collect();
    let daemon = runtime.spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        authenticate_server(
            &mut stream,
            &info.key,
            &info.daemon_id,
            "describe-test",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        let hello = read_frame(&mut stream).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        let ack = ModuleHelloAckBody {
            negotiated_ver: subc_protocol::PROTOCOL_VERSION,
            subc_ops: supported_ops,
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        };
        write_frame(
            &mut stream,
            &Frame::build(
                FrameType::HelloAck,
                Flags::new(false, Priority::Passive, false),
                0,
                0,
                hello.header.corr,
                serde_json::to_vec(&ack).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let mut replies = replies.into_iter();
        while let Some(request) = read_frame(&mut stream).await.unwrap() {
            if request.header.ty != FrameType::Request {
                continue;
            }
            assert_eq!(
                request.header.channel, 0,
                "describe is a module control call"
            );
            records
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&request.body).unwrap());
            let Some((kind, body)) = replies.next() else {
                break;
            };
            write_frame(
                &mut stream,
                &Frame::build(
                    kind,
                    Flags::new(false, Priority::Passive, false),
                    0,
                    0,
                    request.header.corr,
                    serde_json::to_vec(&body).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        }
    });
    let (handle, serving) = runtime
        .block_on(subc_client_rs::serve_with_handle(
            &file,
            subc_protocol::manifest::ModuleManifest::builder("basal", "0.1.0").build(),
            NoRequests,
        ))
        .unwrap();
    let serving = runtime.spawn(serving);
    test(
        module_host().with_scope_describer(handle, runtime.handle().clone()),
        calls,
    );
    serving.abort();
    daemon.abort();
    drop(runtime);
    std::fs::remove_dir_all(dir).unwrap();
}
