//! Before script startup, the parent fetches and hashes the event body; the
//! real worker receives the verified JSON rather than an unresolved notice.
mod common;
use basal_core::{InstallRequest, RunState};
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, MockCatalog,
    MockConsent, TransportError,
};
use basal_module::{caller::Caller, module::Hosts};
use basal_proto::{CallKind, JsonText};
use common::{Options, fixture};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct BodySource(AtomicUsize);
impl Host for BodySource {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        assert_eq!(request.flow_id, "flow");
        assert_eq!(
            request.kind,
            CallKind::Op {
                module: "plexus".into(),
                op: basal_host::catalog::EVENT_BODY_OP.into()
            }
        );
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(Dispatched::Completed(HostOutcome::fulfilled(JsonText::new(json!({"body":"{\"verified\":true}","digest":format!("{:x}",sha2::Sha256::digest(b"{\"verified\":true}")),"event_key":"0".repeat(64),"event_name":"github_pr_changed"}).to_string()).unwrap())))
    }
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.5
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
}
use sha2::Digest;

#[test]
fn worker_preamble_supplies_verified_json_and_synthetic_capture_calls_nothing_live() {
    let source = Arc::new(BodySource(AtomicUsize::new(0)));
    let catalog = MockCatalog::new();
    catalog.set_event(
        "plexus",
        "github_pr_changed",
        1,
        basal_host::EventDecl {
            origin: basal_host::EventOrigin::External,
            body: basal_host::EventBody::Resolved {
                resolve_op: basal_host::catalog::EVENT_BODY_OP.into(),
            },
        },
    );
    catalog.set_op(
        "plexus",
        basal_host::catalog::EVENT_BODY_OP,
        basal_host::OpDecl {
            kind: Some(basal_host::OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    let f = fixture(
        "event-body-worker",
        Options {
            hosts: Some(Hosts {
                host: source.clone(),
                transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
                catalog: Arc::new(catalog),
                consent: Arc::new(MockConsent::new()),
                hooks: Arc::new(basal_core::NoHooks),
            }),
            ..Options::with_worker()
        },
    );
    let manifest=json!({"id":"flow","version":1,"purpose":"Receive verified body","trigger":{"events":[{"module":"plexus","name":"github_pr_changed","version":1}]},"ops":[{"module":"plexus","op":basal_host::catalog::EVENT_BODY_OP}]}).to_string();
    let installed = f
        .module
        .rt
        .install(&InstallRequest {
            script: "return trigger.event.body.verified;".into(),
            manifest,
            author: "operator".into(),
            loop_override: false,
        })
        .unwrap();
    f.module
        .rt
        .approve("flow", 1, &installed.code_hash, "test")
        .unwrap();
    let notice = basal_core::events::Notice {
        published_at_ms: common::T0,
        subject: "ck.account.event.plexus.github_pr_changed.v1".into(),
        event_key: "0".repeat(64),
        digest: format!("{:x}", sha2::Sha256::digest(b"{\"verified\":true}")),
        headers: Default::default(),
    };
    f.module.rt.admit_events(&[notice]).unwrap();
    let (id, _) = f.module.rt.startable().unwrap().remove(0);
    f.module.engine.run_until_idle(50).unwrap();
    let run = f.module.rt.run(&id).unwrap();
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(run.result.as_deref(), Some("true"));
    assert_eq!(source.0.load(Ordering::Relaxed), 1);
    let dry=f.module.handle(&Caller::Operator,"flow.dry_run",json!({"flow_id":"flow","trigger":{"event":{"module":"plexus","name":"github_pr_changed","version":1,"event_key":"0".repeat(64),"headers":{},"body":{"verified":true}}}})).unwrap();
    assert_eq!(
        source.0.load(Ordering::Relaxed),
        1,
        "capture cannot resolve live"
    );
    assert_eq!(
        dry["summary"]["runs"][0]["state"],
        Value::String("succeeded".into()),
        "{dry}"
    );
}

struct DeliveredBodySource {
    body: String,
    calls: AtomicUsize,
}
impl Host for DeliveredBodySource {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        assert_eq!(request.flow_id, "flow");
        assert_eq!(
            request.kind,
            CallKind::Op {
                module: "prefrontal-core".into(),
                op: basal_host::catalog::EVENT_BODY_OP.into(),
            }
        );
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(Dispatched::Completed(HostOutcome::fulfilled(JsonText::new(json!({
            "body":self.body,"digest":format!("{:x}", sha2::Sha256::digest(self.body.as_bytes())),
            "event_key":"0".repeat(64),"event_name":"digest_delivered",
        }).to_string()).unwrap())))
    }
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.5
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
}

#[test]
fn delivered_body_flow_mismatch_fails_run_before_script() {
    for body_flow in [Some("other"), None, Some("flow")] {
        let mut body = json!({"idempotency_key":"delivery-key","agent_id":"ag_synapse","delivered_at":common::T0});
        if let Some(flow) = body_flow {
            body["flow_id"] = json!(flow);
        }
        let body = body.to_string();
        let source = Arc::new(DeliveredBodySource {
            body: body.clone(),
            calls: AtomicUsize::new(0),
        });
        let catalog = MockCatalog::new();
        catalog.set_flow_capable("prefrontal-core", true);
        catalog.set_event(
            "prefrontal-core",
            "digest_delivered",
            1,
            basal_host::EventDecl {
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
        let f = fixture(
            "delivered-body",
            Options {
                hosts: Some(Hosts {
                    host: source.clone(),
                    transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
                    catalog: Arc::new(catalog),
                    consent: Arc::new(MockConsent::new()),
                    hooks: Arc::new(basal_core::NoHooks),
                }),
                ..Options::with_worker()
            },
        );
        let installed = f.module.rt.install(&InstallRequest {
            script: "await kv.set('script-ran', true); return trigger.event.body.flow_id;".into(),
            manifest: json!({"id":"flow","version":1,"purpose":"Receive digest delivery",
                "trigger":{"events":[{"module":"prefrontal-core","name":"digest_delivered","version":1}]},
                "ops":[{"module":"prefrontal-core","op":basal_host::catalog::EVENT_BODY_OP}]}).to_string(),
            author: "operator".into(), loop_override: false,
        }).unwrap();
        f.module
            .rt
            .approve("flow", 1, &installed.code_hash, "test")
            .unwrap();
        f.module
            .rt
            .admit_events(&[basal_core::events::Notice {
                subject: "ck.account.event.prefrontal-core.digest_delivered.v1".into(),
                event_key: "0".repeat(64),
                digest: format!("{:x}", sha2::Sha256::digest(body.as_bytes())),
                headers: std::collections::BTreeMap::from([("flow_id".into(), "flow".into())]),
                published_at_ms: common::T0,
            }])
            .unwrap();
        let (id, _) = f.module.rt.startable().unwrap().remove(0);
        f.module.engine.run_until_idle(50).unwrap();
        let run = f.module.rt.run(&id).unwrap();
        assert_eq!(source.calls.load(Ordering::Relaxed), 1);
        let kv_count: i64 = f
            .module
            .rt
            .store()
            .read(|c| {
                Ok(
                    c.query_row("SELECT COUNT(*) FROM kv WHERE flow_id='flow'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        if body_flow == Some("flow") {
            assert_eq!(run.state, RunState::Succeeded);
            assert_eq!(run.result.as_deref(), Some("\"flow\""));
            assert_eq!(kv_count, 1);
        } else {
            assert_eq!(run.state, RunState::Failed);
            assert_eq!(run.error_kind.as_deref(), Some("event_body_mismatch"));
            assert_eq!(kv_count, 0, "the script never started");
        }
    }
}
