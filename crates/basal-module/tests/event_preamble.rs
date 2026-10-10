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
