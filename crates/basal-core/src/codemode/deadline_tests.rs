use super::*;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tracing::{Event, Metadata, Subscriber, field::Visit, span};

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<(tracing::Level, String)>>>);

impl Subscriber for Logs {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        struct Fields(String);
        impl Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?};", field.name()));
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0
            .lock()
            .unwrap()
            .push((*event.metadata().level(), fields.0));
    }
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}

fn attested_logs(expiry: Option<u64>) -> Vec<(tracing::Level, String)> {
    let host = basal_host::mock::MockHost::default();
    let owner = Principal::Reserved {
        module_id: "prefrontal-core".into(),
    };
    let mut scope = json!({"owner":owner,"ref":"run-scope","scope_epoch":7,"kind":"ephemeral",
        "attributes":{"agent_id":"agent","run_id":"run"},"owner_authorized":true});
    if let Some(expiry) = expiry {
        scope["expires_at_ms"] = json!(expiry);
    }
    host.set_scope_description(
        owner.clone(),
        "run-scope",
        Ok(serde_json::from_value(json!({
        "status":"live","scope_epoch":7,"daemon_incarnation":"daemon","owner_synced":true,
        "owner_configured":true,"scope":scope}))
        .unwrap()),
    );
    let logs = Logs::default();
    let _subscriber = tracing::subscriber::set_default(logs.clone());
    assert_eq!(
        attest(
            &host,
            &FlowScope {
                owner,
                scope_ref: "run-scope".into(),
                epoch: 7
            },
            &json!({"agent_id":"agent","run_id":"run"}),
            Some((100, 200))
        )
        .unwrap(),
        "agent"
    );
    logs.0.lock().unwrap().clone()
}

#[test]
fn out_of_range_stamp_deadlines_warn_without_refusing() {
    for expiry in [99, 201, u64::MAX] {
        let logs = attested_logs(Some(expiry));
        assert_eq!(logs.len(), 1, "{logs:?}");
        assert_eq!(logs[0].0, tracing::Level::WARN);
        assert!(logs[0].1.contains("run scope stamp deadline mismatch"));
        assert!(logs[0].1.contains(&format!("stamp_expires_at_ms={expiry}")));
        assert!(logs[0].1.contains("requested_expires_at_ms=100"));
        assert!(logs[0].1.contains("core_expires_at_ms=200"));
    }
    for expiry in [100, 150, 200] {
        assert!(attested_logs(Some(expiry)).is_empty());
    }
}

#[test]
fn absent_legacy_stamp_deadline_logs_debug_without_refusing() {
    let logs = attested_logs(None);
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert_eq!(logs[0].0, tracing::Level::DEBUG);
    assert!(logs[0].1.contains("run scope stamp deadline absent"));
}

#[test]
fn tool_scope_open_checks_the_attested_deadline() {
    use basal_host::{
        run_scope,
        transport::{Transport, WireError},
    };
    struct Core;
    impl Transport for Core {
        fn catalog(&self) -> std::result::Result<Value, WireError> {
            Ok(json!({"modules":[]}))
        }
        fn management(
            &self,
            _: &str,
            op: &str,
            params: Value,
        ) -> std::result::Result<Value, WireError> {
            assert_eq!(op, "codemode.run_scope.open");
            Ok(json!({"scope":{"ref":"run-scope","epoch":7},
                "expires_at_ms":params["expires_at_ms"],"catalog":[]}))
        }
        fn tool(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> std::result::Result<Value, WireError> {
            panic!("admission does not call tools")
        }
    }
    let store = super::super::store::tests::temp_store();
    let clock = Clock::manual(100);
    let host = basal_host::mock::MockHost::default();
    let owner = Principal::Reserved {
        module_id: "prefrontal-core".into(),
    };
    host.set_scope_description(
        owner,
        "run-scope",
        Ok(serde_json::from_value(json!({
        "status":"live","scope_epoch":7,"daemon_incarnation":"daemon","owner_synced":true,
        "owner_configured":true,"scope":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},
            "ref":"run-scope","scope_epoch":7,"kind":"ephemeral","owner_authorized":true,
            "expires_at_ms":99 + run_scope::SCOPE_LIFETIME_MS,
            "attributes":{"agent_id":"agent","run_id":"run"}}}))
        .unwrap()),
    );
    let logs = Logs::default();
    let _subscriber = tracing::subscriber::set_default(logs.clone());
    let bind = subc_protocol::BindIdentity::new("/work", "test", "session");
    let result = admit_tool(
        &store.store,
        &host,
        &Core,
        &clock,
        Platform::current(),
        &ToolInvocation {
            run_id: "run",
            agent_id: "agent",
            invoking_scope: run_scope::Scope {
                reference: "parent".into(),
                epoch: 1,
            },
            carrier: "reserved:broca",
            call_key: Some("key"),
            record_scope: "parent",
            custodian: "reserved:prefrontal-core",
            bind_identity: &bind,
        },
        &json!({"code":"return 42;"}),
    )
    .unwrap();
    assert!(matches!(result, Admission::Admitted { .. }), "{result:?}");
    let logs = logs.0.lock().unwrap();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert_eq!(logs[0].0, tracing::Level::WARN);
    assert!(logs[0].1.contains("run scope stamp deadline mismatch"));
}
