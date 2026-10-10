mod common;

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU16, AtomicU64, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use basal_host::{
    MockCatalog, MockConsent,
    mock::MockHost,
    run_scope::Open,
    transport::{Transport, WireError},
};
use basal_module::serve::BasalHandler;
use cortexkit_role_harness::{
    Harness, HarnessError, KillPoint, KillReport, PointDeclaration, RouteStamp, Trigger,
};
use cortexkit_role_tool_provider::scope::ScopeIdentity;
use cortexkit_role_tool_provider_conformance::{
    CallSpec, Capability, Exchange, ObservedFrame, RouteFailure, ScopedPrincipals,
    ToolProviderSubject, ToolRoute,
};
use serde_json::{Value, json};
use subc_protocol::session::ModuleControlRequest;
use subc_protocol::{BindIdentity, Flags, Frame, FrameType, Principal, Priority, RouteTarget};
use subc_transport::connection_file::{ConnectionInfo, Endpoint, SCHEMA_VERSION, write_atomic};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::sync::mpsc;

#[derive(Default)]
struct MockCore {
    host: MockHost,
    scopes: basal_testkit::run_scopes::RunScopes,
    clock: basal_core::Clock,
    marker_root: std::path::PathBuf,
}
impl Transport for MockCore {
    fn catalog(&self) -> Result<Value, WireError> {
        Ok(
            json!({"modules":[{"module_id":"mock","capabilities":{"provides":["agent-run-scopes/v1"]},"roles":[{
            "role":"tool_provider","identity_scope":[],"concurrency":"module_managed","emits_push":false,"sub_supervises":false,
            "tools":[{"name":"write_marker","execution_mode":"mutating","schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}]}]}]}),
        )
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        assert_eq!(module, "prefrontal-core");
        match op {
            "codemode.run_scope.open" => {
                let request: Open = serde_json::from_value(params).unwrap();
                self.host.set_scope_description(Principal::Reserved { module_id:"prefrontal-core".into() },&request.run_id,Ok(serde_json::from_value(json!({
                    "status":"live","scope_epoch":19,"daemon_incarnation":"mock","owner_synced":true,"owner_configured":true,
                    "scope":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":request.run_id,"scope_epoch":19,"kind":"ephemeral",
                        "attributes":{"agent_id":request.agent_id,"run_id":request.run_id},"owner_authorized":true}})).unwrap()));
                self.scopes
                    .open(
                        request,
                        vec![basal_host::run_scope::CatalogEntry {
                            tool: "write_marker".into(),
                            module: "mock".into(),
                            op: "write_marker".into(),
                        }],
                    )
                    .map(|reply| serde_json::to_value(reply).unwrap())
            }
            "codemode.run_scope.close" => Ok(serde_json::to_value(
                self.scopes.close(serde_json::from_value(params).unwrap()),
            )
            .unwrap()),
            _ => panic!("unexpected core op {op}"),
        }
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("unscoped tool")
    }
    fn tool_for_run(
        &self,
        run: &str,
        module: &str,
        name: &str,
        input: Value,
        _key: &str,
        _: &basal_host::transport::RunToolOptions,
    ) -> Result<Value, WireError> {
        assert!(run.starts_with("codemode:"));
        assert_eq!((module, name), ("mock", "write_marker"));
        let path = std::path::PathBuf::from(input["path"].as_str().unwrap());
        assert!(path.starts_with(&self.marker_root));
        std::fs::write(path, b"executed").unwrap();
        Ok(json!(42))
    }
}

fn frame(kind: FrameType, channel: u16, corr: u64, body: Value) -> Frame {
    Frame::build(
        kind,
        Flags::new(false, Priority::Passive, false),
        channel,
        if channel == 0 { 0 } else { 1 },
        corr,
        if kind == FrameType::Cancel {
            vec![]
        } else {
            serde_json::to_vec(&body).unwrap()
        },
    )
    .unwrap()
}

type Pending = Arc<Mutex<HashMap<(u16, u64), mpsc::UnboundedSender<Frame>>>>;
struct Wire {
    writer: tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>,
    pending: Pending,
    corr: AtomicU64,
    channel: AtomicU16,
}
impl Wire {
    async fn send(&self, channel: u16, body: Value) -> (u64, mpsc::UnboundedReceiver<Frame>) {
        let corr = self.corr.fetch_add(1, Ordering::SeqCst);
        let (send, receive) = mpsc::unbounded_channel();
        self.pending.lock().unwrap().insert((channel, corr), send);
        write_frame(
            &mut *self.writer.lock().await,
            &frame(FrameType::Request, channel, corr, body),
        )
        .await
        .unwrap();
        (corr, receive)
    }
}
struct Live {
    wire: Arc<Wire>,
    core: Arc<MockCore>,
    provider: tokio::task::JoinHandle<()>,
    reader: tokio::task::JoinHandle<()>,
}
impl Drop for Live {
    fn drop(&mut self) {
        self.core.scopes.release();
        self.provider.abort();
        self.reader.abort();
    }
}
#[derive(Clone)]
struct Route {
    wire: Arc<Wire>,
    channel: u16,
}
impl Route {
    async fn exchange(&self, body: Value, cancel: bool) -> Result<Exchange, RouteFailure> {
        let (corr, mut receive) = self.wire.send(self.channel, body).await;
        if cancel {
            tokio::time::sleep(Duration::from_millis(20)).await;
            write_frame(
                &mut *self.wire.writer.lock().await,
                &frame(FrameType::Cancel, self.channel, corr, json!({})),
            )
            .await
            .unwrap();
        }
        let mut frames = Vec::new();
        loop {
            let observed = tokio::time::timeout(Duration::from_secs(180), receive.recv())
                .await
                .map_err(|_| RouteFailure::new("reply timeout"))?
                .ok_or_else(|| RouteFailure::new("provider closed"))?;
            let terminal = matches!(
                observed.header.ty,
                FrameType::Response | FrameType::Error | FrameType::StreamEnd
            );
            frames.push(observed);
            if terminal {
                break;
            }
        }
        // Keep listening after the terminal frame so a double terminal is a
        // test failure, rather than something hidden by the route adapter.
        while let Ok(Some(observed)) =
            tokio::time::timeout(Duration::from_millis(20), receive.recv()).await
        {
            frames.push(observed);
        }
        self.wire
            .pending
            .lock()
            .unwrap()
            .remove(&(self.channel, corr));
        Ok(Exchange {
            frames: frames
                .into_iter()
                .map(|frame| match frame.header.ty {
                    FrameType::Response => {
                        ObservedFrame::Response(serde_json::from_slice(&frame.body).unwrap())
                    }
                    FrameType::Error => {
                        ObservedFrame::Error(serde_json::from_slice(&frame.body).unwrap())
                    }
                    FrameType::StreamEnd => ObservedFrame::StreamEnd,
                    _ => ObservedFrame::Data(serde_json::from_slice(&frame.body).unwrap()),
                })
                .collect(),
        })
    }
}
#[async_trait]
impl ToolRoute for Route {
    async fn request(&self, body: Value) -> Result<Exchange, RouteFailure> {
        self.exchange(body, false).await
    }
    async fn request_then_cancel(&self, body: Value) -> Result<Exchange, RouteFailure> {
        self.exchange(body, true).await
    }
}

#[derive(Default)]
struct Subject {
    core: Mutex<Option<Arc<MockCore>>>,
}
fn principals() -> ScopedPrincipals {
    ScopedPrincipals {
        scope: ScopeIdentity {
            owner: "reserved:prefrontal-core".into(),
            scope_ref: "agent-scope".into(),
            scope_epoch: 7,
        },
        carrier: "reserved:broca".into(),
        other_carrier: "reserved:aft".into(),
        other_scope: ScopeIdentity {
            owner: "reserved:prefrontal-core".into(),
            scope_ref: "another-scope".into(),
            scope_epoch: 8,
        },
    }
}
fn principal(name: &str) -> Principal {
    if let Some(module) = name.strip_prefix("reserved:") {
        Principal::Reserved {
            module_id: module.into(),
        }
    } else {
        assert_eq!(name, "direct");
        Principal::Direct
    }
}

#[async_trait]
impl Harness for Subject {
    type Handle = Live;
    type Route = Route;
    fn declared_points(&self) -> Vec<PointDeclaration> {
        vec![]
    }
    async fn spawn(&self, state_root: &Path) -> Result<Live, HarnessError> {
        std::fs::create_dir_all(state_root).unwrap();
        let root = state_root.to_path_buf();
        let core = Arc::new(MockCore {
            clock: basal_core::Clock::manual(common::T0),
            marker_root: root.parent().unwrap_or(&root).to_path_buf(),
            ..Default::default()
        });
        *self.core.lock().unwrap() = Some(core.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let info = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: vec![7; 32],
            daemon_id: [1; 16],
            pid: std::process::id(),
            daemon_ver: "conformance".into(),
        };
        let file = root.join("connection.json");
        write_atomic(&file, &info).unwrap();
        let host = core.clone();
        let clock = core.clock.clone();
        let configured_root = root.clone();
        let (ready, prepared) = tokio::sync::oneshot::channel();
        let ready = Arc::new(Mutex::new(Some(ready)));
        let catalog = MockCatalog::standard();
        catalog.set_op(
            "mock",
            "write_marker",
            basal_host::OpDecl {
                kind: Some(basal_host::OpKind::Mutate),
                cause_echo: false,
                shell_capable: false,
            },
        );
        let handler = BasalHandler::new(
            Box::new(move |_| {
                let mut pool = common::pool_config(&common::Options::default());
                pool.warm_spares = 0;
                basal_module::module::ModuleConfig {
                    store_path: configured_root.join("basal.db"),
                    durability: basal_core::Durability { fullfsync: false },
                    runtime: basal_core::Config {
                        clock: clock.clone(),
                        ..Default::default()
                    },
                    pool,
                    engine: Default::default(),
                    dry_run: basal_module::dryrun::DryRunConfig::new(
                        configured_root.join("dry-run"),
                    ),
                }
            }),
            Box::new(move || basal_module::module::Hosts {
                host: Arc::new(host.host.clone()),
                transport: host.clone(),
                catalog: Arc::new(catalog.clone()),
                consent: Arc::new(MockConsent::new()),
                hooks: Arc::new(basal_core::NoHooks),
            }),
        )
        .with_tool_foreground(Duration::from_millis(50))
        .with_ready_hosts(Box::new(move || {
            if let Some(ready) = ready.lock().unwrap().take() {
                let _ = ready.send(());
            }
        }));
        let provider = tokio::spawn(async move {
            let (_, serving) = subc_client_rs::serve_with_handle(
                &file,
                basal_module::manifest::manifest(),
                handler,
            )
            .await
            .unwrap();
            serving.await.unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        authenticate_server(
            &mut stream,
            &info.key,
            &info.daemon_id,
            "conformance",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let hello = read_frame(&mut stream).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        write_frame(&mut stream,&frame(FrameType::HelloAck,0,hello.header.corr,json!({"negotiated_ver":subc_protocol::PROTOCOL_VERSION,"subc_ops":[],"subc_capabilities":[]}))).await.unwrap();
        let (mut read, write) = stream.into_split();
        let pending: Pending = Arc::default();
        let replies = pending.clone();
        let reader = tokio::spawn(async move {
            while let Ok(Some(frame)) = read_frame(&mut read).await {
                if let Some(send) = replies
                    .lock()
                    .unwrap()
                    .get(&(frame.header.channel, frame.header.corr))
                {
                    let _ = send.send(frame);
                }
            }
            replies.lock().unwrap().clear();
        });
        tokio::time::timeout(Duration::from_secs(180), prepared)
            .await
            .unwrap()
            .unwrap();
        Ok(Live {
            wire: Arc::new(Wire {
                writer: tokio::sync::Mutex::new(write),
                pending,
                corr: AtomicU64::new(100),
                channel: AtomicU16::new(1),
            }),
            core,
            provider,
            reader,
        })
    }
    async fn route(&self, handle: &Live, stamp: &RouteStamp) -> Result<Route, HarnessError> {
        let channel = handle.wire.channel.fetch_add(1, Ordering::SeqCst);
        let scope = stamp.scope.as_ref().map(|scope| serde_json::from_value(json!({
            "owner":principal(&scope.owner),"ref":scope.scope_ref,"scope_epoch":scope.scope_epoch,"kind":"head",
            "attributes":{"agent_id":"agent"},"owner_authorized":true})).unwrap());
        let bind = ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "basal".into(),
            },
            identity: BindIdentity::new("/work", "test", "session"),
            principal: Some(principal(&stamp.principal)),
            consumer_capabilities: None,
            role_versions: None,
            admission_facts: None,
            scope,
        };
        let (corr, mut reply) = handle
            .wire
            .send(0, serde_json::to_value(bind).unwrap())
            .await;
        let response = reply.recv().await.unwrap();
        handle.wire.pending.lock().unwrap().remove(&(0, corr));
        assert_eq!(response.header.ty, FrameType::Response, "{response:?}");
        Ok(Route {
            wire: handle.wire.clone(),
            channel,
        })
    }
    async fn kill_at(
        &self,
        _: Live,
        _: &KillPoint,
        _: Trigger<'_>,
    ) -> Result<KillReport, HarnessError> {
        Err(HarnessError::new(
            "basal does not own approval execution or its kill points",
        ))
    }
    async fn restart(&self, state_root: &Path) -> Result<Live, HarnessError> {
        self.spawn(state_root).await
    }
}
#[async_trait]
impl ToolProviderSubject for Subject {
    fn capabilities(&self) -> BTreeSet<Capability> {
        [
            Capability::ScopeStamp,
            Capability::CallKey,
            Capability::SchemaPin,
            Capability::HeldCalls,
            Capability::Cancellation,
            Capability::LateResults,
        ]
        .into_iter()
        .collect()
    }
    fn plain_stamp(&self) -> RouteStamp {
        principals().stamp("reserved:broca")
    }
    fn scoped_principals(&self) -> Option<ScopedPrincipals> {
        Some(principals())
    }
    fn catalog_arguments(&self) -> Value {
        json!({"params":{}})
    }
    fn quick_call(&self) -> CallSpec {
        ("codemode".into(), json!({"code":"return 42;"}))
    }
    fn slow_call(&self) -> Option<CallSpec> {
        Some(("codemode".into(), json!({"code":"while (true) {}"})))
    }
    fn disabled_tool(&self) -> Option<String> {
        None
    }
    fn held_call(&self, marker: &Path) -> Option<CallSpec> {
        self.core
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .scopes
            .hold_next_open();
        Some((
            "codemode".into(),
            json!({"code":format!("return await tools.write_marker({{path:{}}});",serde_json::to_string(&marker.to_string_lossy()).unwrap())}),
        ))
    }
    async fn await_held(&self, _: &str) -> Result<(), HarnessError> {
        let core = self.core.lock().unwrap().as_ref().unwrap().clone();
        tokio::task::spawn_blocking(move || core.scopes.await_held())
            .await
            .unwrap();
        Ok(())
    }
    async fn approve(&self, _: &str) -> Result<(), HarnessError> {
        Err(HarnessError::new("basal never owns or answers approvals"))
    }
    async fn settle(&self) {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn basal_tool_provider_conforms_for_declared_capabilities() {
    let root = common::scratch("tool-conformance");
    let report = cortexkit_role_tool_provider_conformance::run_suite(
        &Subject::default(),
        &root.join("suite"),
    )
    .await
    .unwrap();
    use cortexkit_role_tool_provider_conformance::{CaseOutcome, SuiteVerdict};
    println!("{}", report.render());
    assert!(
        matches!(
            report.verdict,
            SuiteVerdict::ConformingForDeclaredCapabilities { .. }
        ),
        "{}",
        report.render()
    );
    let skipped: BTreeSet<_> = report
        .cases
        .iter()
        .filter(|case| matches!(case.outcome, CaseOutcome::Skipped { .. }))
        .map(|case| case.case)
        .collect();
    assert_eq!(
        skipped,
        [
            "system_text_digests_match_text",
            "system_text_preflight_digest_stable",
            "catalog_disabled_tool_absent",
            "call_disabled_tool_refused_by_name",
            "late_results_cursor_round_trip",
            "late_results_ack",
            "crash_after_prepared_not_started",
            "crash_after_authorized_not_started"
        ]
        .into_iter()
        .collect()
    );
    std::fs::remove_dir_all(root).unwrap();
}

async fn response(route: &Route, body: Value) -> Value {
    let exchange = route.request(body).await.unwrap();
    match cortexkit_role_tool_provider_conformance::single_terminal(&exchange).unwrap() {
        ObservedFrame::Response(value) => value.clone(),
        other => panic!("expected response, got {other:?}"),
    }
}
async fn complete(route: &Route, key: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let result = response(route,json!({"name":"codemode","arguments":{"code":"console.log('kept'); return 42;"},"call_key":key})).await;
        if result["status"] != "running" {
            assert_eq!(result["status"], "completed", "{result}");
            return result;
        }
        assert!(tokio::time::Instant::now() < deadline, "run never settled");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_route_late_cursor_ack_and_custodian_only_serving() {
    let root = common::scratch("late-cursor");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let carrier = subject
        .route(&live, &principals().stamp("reserved:broca"))
        .await
        .unwrap();
    let other = subject
        .route(&live, &principals().stamp("reserved:aft"))
        .await
        .unwrap();
    let owner = subject
        .route(&live, &principals().stamp("reserved:prefrontal-core"))
        .await
        .unwrap();
    let final_result = complete(&carrier, "cursor-round-trip").await;
    let request = json!({"name":"late_results","arguments":{"since":null,"limit":1}});
    let page = response(&carrier, request.clone()).await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["entries"][0]["result"], final_result);
    assert_eq!(page["entries"][0]["custodian"], "reserved:broca");
    assert_eq!(response(&carrier, request.clone()).await, page);
    for route in [&other, &owner] {
        assert_eq!(response(route, request.clone()).await["entries"], json!([]));
    }
    assert_eq!(
        response(
            &carrier,
            json!({"name":"late_results","arguments":{"since":page["cursor"]}})
        )
        .await["entries"],
        json!([])
    );
    assert_eq!(
        response(
            &carrier,
            json!({"name":"late_results.ack","arguments":{"through":page["cursor"]}})
        )
        .await,
        json!({})
    );
    assert_eq!(
        response(&carrier, request).await["entries"],
        json!([]),
        "an ack must not republish its entry"
    );
    drop((carrier, other, owner, live));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_route_late_retention_and_direct_call_owner_custody() {
    let root = common::scratch("late-retention");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let direct = subject
        .route(&live, &principals().stamp("direct"))
        .await
        .unwrap();
    let owner = subject
        .route(&live, &principals().stamp("reserved:prefrontal-core"))
        .await
        .unwrap();
    complete(&direct, "retained").await;
    let request = json!({"name":"late_results","arguments":{"since":null}});
    assert_eq!(
        response(&direct, request.clone()).await["entries"],
        json!([])
    );
    let page = response(&owner, request.clone()).await;
    assert_eq!(page["entries"][0]["kind"], "result");
    assert_eq!(page["entries"][0]["custodian"], "reserved:prefrontal-core");
    live.core.clock.advance(24 * 60 * 60 * 1000 - 1);
    assert_eq!(
        response(&owner, request.clone()).await["entries"][0]["kind"],
        "result"
    );
    live.core.clock.advance(1);
    let expired = response(&owner, request).await;
    assert_eq!(expired["entries"][0]["kind"], "expired");
    assert_eq!(expired["entries"][0]["reduced"], true);
    assert!(expired["entries"][0].get("result").is_none());
    for key in [
        "owner",
        "ref",
        "scope_epoch",
        "custodian",
        "call_key",
        "event_id",
        "settled_at",
    ] {
        assert_eq!(expired["entries"][0][key], page["entries"][0][key]);
    }
    drop((direct, owner, live));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_route_late_incarnation_changes_and_event_identity_survives_restart() {
    let root = common::scratch("late-restart");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let route = subject
        .route(&live, &principals().stamp("reserved:broca"))
        .await
        .unwrap();
    complete(&route, "restart-event").await;
    let page = response(
        &route,
        json!({"name":"late_results","arguments":{"since":null}}),
    )
    .await;
    drop((route, live));
    let live = subject.restart(&root).await.unwrap();
    let route = subject
        .route(&live, &principals().stamp("reserved:broca"))
        .await
        .unwrap();
    let rejected = route
        .request(json!({"name":"late_results","arguments":{"since":page["cursor"]}}))
        .await
        .unwrap();
    assert!(
        matches!(cortexkit_role_tool_provider_conformance::single_terminal(&rejected).unwrap(),ObservedFrame::Error(error) if error.code == "cursor_incarnation_changed")
    );
    let reread = response(
        &route,
        json!({"name":"late_results","arguments":{"since":null}}),
    )
    .await;
    assert_ne!(
        page["cursor"]["provider_incarnation"],
        reread["cursor"]["provider_incarnation"]
    );
    assert_eq!(page["entries"], reread["entries"]);
    drop((route, live));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_route_held_action_reaches_marker_after_core_releases_admission() {
    let root = common::scratch("held-marker-control");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let route = subject
        .route(&live, &principals().stamp("reserved:broca"))
        .await
        .unwrap();
    let marker = root.join("action.marker");
    let (name, arguments) = subject.held_call(&marker).unwrap();
    let request =
        route.request(json!({"name":name,"arguments":arguments,"call_key":"marker-control"}));
    let release = async {
        subject.await_held("marker-control").await.unwrap();
        live.core.scopes.release();
    };
    let (result, ()) = tokio::join!(request, release);
    result.unwrap();
    complete(&route, "marker-control").await;
    assert_eq!(std::fs::read(&marker).unwrap(), b"executed");
    drop((route, live));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_route_malformed_key_types_and_withdraw_null_key_name_the_field() {
    let root = common::scratch("malformed-key-types");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let route = subject
        .route(&live, &principals().stamp("reserved:broca"))
        .await
        .unwrap();
    for (body, field) in [
        (
            json!({"name":"codemode","arguments":{"code":"return 1;"},"call_key":1}),
            "call_key",
        ),
        (
            json!({"name":"codemode","arguments":{"code":"return 1;"},"schema_pin":false}),
            "schema_pin",
        ),
        (
            json!({"name":"tool.withdraw","arguments":{"call_key":"missing"},"call_key":null}),
            "call_key",
        ),
    ] {
        let exchange = route.request(body).await.unwrap();
        assert!(
            matches!(cortexkit_role_tool_provider_conformance::single_terminal(&exchange).unwrap(),ObservedFrame::Error(error) if error.code=="invalid_request" && error.detail.as_ref().unwrap()["field"] == field)
        );
    }
    drop((route, live));
    std::fs::remove_dir_all(root).unwrap();
}
