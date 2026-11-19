use super::*;
use crate::module::{Hosts, ModuleConfig};
use crate::pool::{PoolConfig, ProcessSpawner};
use crate::process::WorkerLaunch;
use crate::serve::BasalHandler;
use basal_core::{Clock, Config, Durability, InstallGate, NoHooks};
use basal_host::{MockCatalog, MockConsent, mock::MockHost};
use std::sync::atomic::Ordering;
use subc_client_rs::test_support::{BindIdentity, Frame, FrameType, ModuleHarness, RouteTarget};
use subc_client_rs::{ModuleHandler, RouteBindRequest};
use subc_protocol::Principal;

const T0: i64 = 1_777_593_600_000;
const WINDOW: Duration = Duration::from_secs(3);
const FLOW: &str = "flow-watch-synthetic";

struct Fixture {
    module: Arc<Module>,
    registry: Arc<Registry>,
    handler: BasalHandler,
    clock: Clock,
}

impl Fixture {
    fn new() -> Self {
        let dir = basal_core::test_support::timestamp_scratch("overview-watch", "sdk");
        let clock = Clock::manual(T0);
        let mut pool = PoolConfig::new(dir.join("unused-worker"), WorkerLaunch::Plain);
        pool.warm_spares = 0;
        let spawner = Arc::new(ProcessSpawner::new(&pool));
        let catalog = MockCatalog::standard();
        catalog.add_named_agent("agent_watch", "Synthetic agent");
        let module = Arc::new(
            Module::start(
                ModuleConfig {
                    store_path: dir.join("store.db"),
                    durability: Durability::throwaway(),
                    runtime: Config {
                        clock: clock.clone(),
                        install_gate: InstallGate::Off,
                        ..Config::default()
                    },
                    pool,
                    engine: Default::default(),
                    dry_run: crate::dryrun::DryRunConfig::new(dir.join("dry-run")),
                },
                Hosts {
                    host: Arc::new(MockHost::new()),
                    transport: Arc::new(crate::unconfigured::UnconfiguredTransport),
                    catalog: Arc::new(catalog),
                    consent: Arc::new(MockConsent::new()),
                    hooks: Arc::new(NoHooks),
                },
                spawner,
            )
            .unwrap(),
        );
        let manifest = json!({"id":FLOW,"version":1,"title":"Synthetic watch flow","purpose":"A test flow.","explanation":"Exercises overview watches without external effects.","trigger":{"events":[{"module":"plexus","name":"pull_request_review","version":1}]},"audience":{"kind":"global"},"sinks":[{"agent":"$audience","digest_max":"piggyback"}],"ops":[]});
        module.rt.store().write(|tx| {
            tx.execute("INSERT INTO flows(flow_id,owner,state,approved_version,created_at) VALUES (?1,'operator','enabled',1,?2)", rusqlite::params![FLOW,T0])?;
            tx.execute("INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,installed_at,approved_at,approval_ref,approval_kind) VALUES (?1,1,zeroblob(32),?2,'return 1;','operator','approved',?3,?3,'synthetic','card')",rusqlite::params![FLOW,manifest.to_string(),T0])?;
            Ok(())
        }).unwrap();
        let registry = Arc::new(Registry::default());
        let handler = BasalHandler::overview_test(module.clone(), registry.clone());
        Self {
            module,
            registry,
            handler,
            clock,
        }
    }

    async fn harness(&self) -> ModuleHarness {
        ModuleHarness::start(|_| self.handler.clone(), []).await
    }

    fn read(&self, caller: &Caller, params: Value) -> Value {
        self.module.handle(caller, "flow.overview", params).unwrap()
    }

    async fn evaluated(&self, count: usize) {
        while self.registry.evaluations.load(Ordering::SeqCst) < count {
            self.registry.evaluated.notified().await;
        }
    }
}

async fn route(
    harness: &mut ModuleHarness,
    channel: u16,
    principal: Principal,
    harness_name: &str,
) -> RouteHandle {
    harness
        .bind_route(
            RouteBindRequest::new(
                RouteHandle::detached(channel, 1),
                RouteTarget::ManagementSurface {
                    module_id: "basal".into(),
                },
                BindIdentity::new(harness_name, "watch-test"),
            )
            .with_principal(principal),
        )
        .await
}

async fn request(
    harness: &mut ModuleHarness,
    route: RouteHandle,
    method: &str,
    params: Value,
) -> u64 {
    harness
        .send_request(
            route,
            serde_json::to_vec(&json!({"method":method,"params":params})).unwrap(),
        )
        .await
}

async fn next(harness: &mut ModuleHarness) -> Frame {
    tokio::time::timeout(Duration::from_secs(60), harness.next_frame())
        .await
        .expect("watch frame deadline")
        .expect("SDK connection open")
}

async fn snapshot(harness: &mut ModuleHarness, corr: u64) -> Value {
    let frame = next(harness).await;
    assert_eq!(frame.header.corr, corr);
    assert_eq!(frame.header.ty, FrameType::StreamData, "{frame:?}");
    let value: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(value["frame"], "snapshot");
    let bytes = serde_json::to_vec(&value["body"]).unwrap();
    assert_eq!(value["digest"], format!("{:x}", Sha256::digest(bytes)));
    value
}

async fn cancel(harness: &mut ModuleHarness, route: RouteHandle, corr: u64) -> Frame {
    harness.cancel_request(route, corr).await;
    let end = next(harness).await;
    assert_eq!(end.header.corr, corr);
    assert_eq!(end.header.ty, FrameType::StreamEnd);
    assert!(end.body.is_empty());
    end
}

#[tokio::test]
async fn overview_watch_open_matches_read_and_admission() {
    let f = Fixture::new();
    let mut harness = f.harness().await;
    for (index, principal, name, caller) in [
        (
            1,
            Principal::Reserved {
                module_id: "callosum".into(),
            },
            "phone",
            Caller::Operator,
        ),
        (2, Principal::Direct, "ckdesk", Caller::DevOperatorApp),
    ] {
        let route = route(&mut harness, index, principal, name).await;
        for params in [
            Value::Null,
            json!({"include_removed":true}),
            json!({"agent_id":"agent_watch","include_removed":true}),
            json!({"workspace_id":"workspace_watch"}),
        ] {
            let expected = f.read(&caller, params.clone());
            let corr = request(&mut harness, route, "flow.overview_watch", params).await;
            let actual = snapshot(&mut harness, corr).await;
            assert_eq!(
                serde_json::to_vec(&actual["body"]).unwrap(),
                serde_json::to_vec(&expected).unwrap()
            );
            cancel(&mut harness, route, corr).await;
        }
    }
    for (index, principal, name) in [
        (3, Principal::Direct, "fed-desktop"),
        (
            4,
            Principal::Reserved {
                module_id: "prefrontal".into(),
            },
            "core",
        ),
        (5, Principal::Unverified, "unknown"),
    ] {
        let route = route(&mut harness, index, principal, name).await;
        let read = request(&mut harness, route, "flow.overview", json!({"extra":true})).await;
        let expected = harness.read_reply(route, read).await;
        let watch = request(
            &mut harness,
            route,
            "flow.overview_watch",
            json!({"extra":true}),
        )
        .await;
        let actual = harness.read_reply(route, watch).await;
        assert_eq!(actual.header.ty, FrameType::Error);
        let expected_code = if index == 3 {
            "operator_attestation_required"
        } else {
            "not_permitted"
        };
        assert_eq!(
            serde_json::from_slice::<Value>(&actual.body).unwrap()["code"],
            expected_code
        );
        assert_eq!(
            actual.body, expected.body,
            "admission precedes parameter validation"
        );
    }
    let route = route(&mut harness, 6, Principal::Direct, "ckdesk").await;
    for params in [
        json!({"extra":true}),
        json!({"agent_id":"Display name"}),
        json!({"include_removed":"true"}),
    ] {
        let read = request(&mut harness, route, "flow.overview", params.clone()).await;
        let expected = harness.read_reply(route, read).await;
        let watch = request(&mut harness, route, "flow.overview_watch", params).await;
        assert_eq!(harness.read_reply(route, watch).await.body, expected.body);
    }
    let manifest = crate::manifest::manifest();
    assert_eq!(
        manifest.operation_delivery.get("flow.overview_watch"),
        Some(&subc_protocol::manifest::OperationDelivery::WATCH)
    );
    harness.shutdown().await;
}

async fn one_change_emits_once(change: &str) {
    let f = Fixture::new();
    let mut harness = f.harness().await;
    let route = route(&mut harness, 1, Principal::Direct, "ckdesk").await;
    let corr = request(
        &mut harness,
        route,
        "flow.overview_watch",
        json!({"include_removed":true}),
    )
    .await;
    snapshot(&mut harness, corr).await;
    if change == "run" {
        f.module
            .rt
            .fixture_run(
                "run-watch",
                FLOW,
                1,
                "return 1;",
                "{}",
                basal_core::run_records::State::new(T0),
            )
            .unwrap();
        f.module
            .rt
            .fixture_finish("run-watch", basal_core::RunState::Succeeded, None, None)
            .unwrap();
    } else {
        f.module
            .handle(&Caller::Operator, change, json!({"flow_id":FLOW}))
            .unwrap();
    }
    tokio::time::advance(WINDOW).await;
    let changed = snapshot(&mut harness, corr).await;
    match change {
        "run" => assert_eq!(
            changed["body"]["flows"][0]["last_run"],
            json!({"at_ms":T0,"outcome":"succeeded"})
        ),
        "flow.disable" => assert_eq!(changed["body"]["flows"][0]["state"], "disabled"),
        "flow.uninstall" => assert_eq!(changed["body"]["flows"][0]["state"], "removed"),
        _ => unreachable!(),
    }
    cancel(&mut harness, route, corr).await;
    assert_eq!(
        harness
            .observed_frames()
            .iter()
            .filter(|f| f.header.ty == FrameType::StreamData)
            .count(),
        2
    );
    harness.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn overview_watch_run_disable_uninstall_each_emit_once() {
    one_change_emits_once("run").await;
    one_change_emits_once("flow.disable").await;
    one_change_emits_once("flow.uninstall").await;
}

#[tokio::test(start_paused = true)]
async fn overview_watch_coalesces_three_changes_and_suppresses_identical_content() {
    let f = Fixture::new();
    let mut harness = f.harness().await;
    let route = route(&mut harness, 1, Principal::Direct, "ckdesk").await;
    let corr = request(&mut harness, route, "flow.overview_watch", json!({})).await;
    let opened = snapshot(&mut harness, corr).await;
    f.registry.parked.notified().await;
    for title in ["First change", "Second change", "Final change"] {
        f.module
            .rt
            .store()
            .write(|tx| {
                let text: String = tx.query_row(
                    "SELECT manifest FROM installs WHERE flow_id=?1",
                    [FLOW],
                    |r| r.get(0),
                )?;
                let mut manifest: Value = serde_json::from_str(&text).unwrap();
                manifest["title"] = json!(title);
                tx.execute(
                    "UPDATE installs SET manifest=?2 WHERE flow_id=?1",
                    rusqlite::params![FLOW, manifest.to_string()],
                )?;
                Ok(())
            })
            .unwrap();
    }
    f.registry.woken.notified().await;
    let (sent_at, due) = lock(&f.registry.window).expect("first snapshot scheduled a window");
    assert_eq!(due.duration_since(sent_at), WINDOW);
    assert_eq!(
        f.registry.starts.load(Ordering::SeqCst),
        1,
        "dirty watch parks until its deadline"
    );
    tokio::time::advance(WINDOW - Duration::from_millis(1)).await;
    assert_eq!(
        f.registry.starts.load(Ordering::SeqCst),
        1,
        "no read before three seconds"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    let changed = snapshot(&mut harness, corr).await;
    assert_eq!(changed["body"]["flows"][0]["title"], "Final change");
    assert_ne!(opened["body"], changed["body"]);
    f.registry.parked.notified().await;
    f.clock.set(T0 + 99);
    // A table touch can leave the served content unchanged. The clock change
    // must not turn that false-positive invalidation into a snapshot.
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute("UPDATE flows SET state=state WHERE flow_id=?1", [FLOW])?;
            Ok(())
        })
        .unwrap();
    f.registry.woken.notified().await;
    tokio::time::advance(WINDOW).await;
    f.evaluated(3).await;
    f.registry.parked.notified().await;
    cancel(&mut harness, route, corr).await;
    assert_eq!(
        harness
            .observed_frames()
            .iter()
            .filter(|f| f.header.ty == FrameType::StreamData)
            .count(),
        2
    );
    harness.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn overview_watch_idle_does_not_recompute() {
    let f = Fixture::new();
    let mut harness = f.harness().await;
    let route = route(&mut harness, 1, Principal::Direct, "ckdesk").await;
    let corr = request(&mut harness, route, "flow.overview_watch", json!({})).await;
    snapshot(&mut harness, corr).await;
    f.registry.parked.notified().await;
    assert_eq!(f.registry.evaluations.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.module
            .rt
            .store()
            .overview_changes()
            .waiting_subscriptions_for_test(),
        1,
        "an idle watch is parked on its change subscription"
    );
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO meta(key,value) VALUES ('watch-unobserved','synthetic')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    f.clock.set(T0 + 3_600_000);
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert_eq!(
        f.registry.starts.load(Ordering::SeqCst),
        1,
        "idle clock advances must not start a recompute"
    );
    assert_eq!(f.registry.evaluations.load(Ordering::SeqCst), 1);
    cancel(&mut harness, route, corr).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn overview_watch_ninth_refused_cancel_and_route_close_free_slots() {
    let f = Fixture::new();
    let mut harness = f.harness().await;
    let first = route(&mut harness, 1, Principal::Direct, "ckdesk").await;
    let second = route(&mut harness, 2, Principal::Direct, "ckdesk").await;
    let mut correlations = Vec::new();
    for _ in 0..8 {
        let corr = request(&mut harness, first, "flow.overview_watch", json!({})).await;
        snapshot(&mut harness, corr).await;
        correlations.push(corr);
    }
    let corr = request(&mut harness, second, "flow.overview_watch", json!({})).await;
    let refused = next(&mut harness).await;
    assert_eq!(refused.header.corr, corr);
    assert_eq!(refused.header.ty, FrameType::Error);
    let error: Value = serde_json::from_slice(&refused.body).unwrap();
    assert_eq!(error["code"], "watch_limit_reached");
    assert_eq!(
        error["message"],
        "basal already serves 8 overview watches; close one first"
    );
    cancel(&mut harness, first, correlations.remove(0)).await;
    let corr = request(&mut harness, second, "flow.overview_watch", json!({})).await;
    snapshot(&mut harness, corr).await;
    cancel(&mut harness, second, corr).await;
    f.handler.on_route_gone(&first).await;
    assert!(
        lock(&f.registry.state).open.is_empty(),
        "route close releases capacity immediately"
    );
    for _ in correlations {
        let end = next(&mut harness).await;
        assert_eq!(end.header.ty, FrameType::StreamEnd);
        assert!(end.body.is_empty());
    }
    let corr = request(&mut harness, second, "flow.overview_watch", json!({})).await;
    snapshot(&mut harness, corr).await;
    f.registry.stop();
    let end = next(&mut harness).await;
    assert_eq!(end.header.ty, FrameType::StreamEnd);
    assert!(end.body.is_empty());
    harness.shutdown().await;
}

#[test]
fn overview_watch_large_answer_uses_hint_digest() {
    let answer = json!({"as_of_ms":T0,"flows":[{"title":"x".repeat(NOTICE_BOUND)}]});
    let expected = format!("{:x}", Sha256::digest(serde_json::to_vec(&answer).unwrap()));
    let encoded = frame(answer);
    assert!(encoded.len() <= NOTICE_BOUND);
    assert_eq!(
        serde_json::from_slice::<Value>(&encoded).unwrap(),
        json!({"frame":"hint","digest":expected})
    );
}

#[tokio::test(start_paused = true)]
async fn overview_watch_vectors_match_real_handler_and_sha256sums() {
    let f = Fixture::new();
    let mut harness = f.harness().await;
    let route = route(&mut harness, 1, Principal::Direct, "ckdesk").await;
    let params = json!({"include_removed":true});
    let read = f.read(&Caller::DevOperatorApp, params.clone());
    let corr = request(&mut harness, route, "flow.overview_watch", params.clone()).await;
    let opened = snapshot(&mut harness, corr).await;
    assert_eq!(opened["body"], read);
    f.clock.set(T0 + 1);
    f.module
        .handle(&Caller::Operator, "flow.disable", json!({"flow_id":FLOW}))
        .unwrap();
    tokio::time::advance(WINDOW).await;
    let changed = snapshot(&mut harness, corr).await;
    let changed_read = f.read(&Caller::DevOperatorApp, params.clone());
    assert_eq!(changed["body"], changed_read);
    let end = cancel(&mut harness, route, corr).await;
    let value = json!({
        "request":{"method":"flow.overview_watch","params":params},
        "read":read,"snapshot":opened,"changedRead":changed_read,"changed":changed,
        "end":{"type":format!("{:?}",end.header.ty),"body":end.body},
    });
    harness.shutdown().await;
    let name = "overview-watch.json";
    let actual = serde_json::to_string_pretty(&value).unwrap() + "\n";
    if std::env::var_os("BASAL_PRINT_PHONE_VECTORS").is_some() {
        println!(
            "PHONE_VECTORS={}",
            serde_json::to_string(&BTreeMap::from([(name, actual)])).unwrap()
        );
        return;
    }
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/flows-phone-v1");
    let write = std::env::var_os("BASAL_WRITE_VECTORS").is_some();
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap();
    let mut entries: BTreeMap<String, String> = sums
        .lines()
        .map(|line| {
            let (digest, file) = line.split_once("  ").expect("checksum entry");
            (file.into(), digest.into())
        })
        .collect();
    let digest = format!("{:x}", Sha256::digest(actual.as_bytes()));
    if write {
        std::fs::write(dir.join(name), &actual).unwrap();
        entries.insert(name.into(), digest.clone());
        let sums: String = entries
            .iter()
            .map(|(file, digest)| format!("{digest}  {file}\n"))
            .collect();
        std::fs::write(dir.join("SHA256SUMS"), sums).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(dir.join(name)).unwrap_or_default(),
        actual,
        "handler vector drift: {name}"
    );
    assert_eq!(
        entries.get(name),
        Some(&digest),
        "watch vector checksum drift"
    );
}
