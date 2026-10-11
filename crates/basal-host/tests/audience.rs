use basal_host::core_host::{CoreHost, IntentContext, intent};
use basal_host::subc_catalog::{CORE, SubcCatalog};
use basal_host::transport::{Transport, WireError};
use basal_host::{CallRequest, Catalog, Dispatched, Host};
use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Wire {
    reply: Mutex<Option<Result<Value, WireError>>>,
    calls: Mutex<Vec<(String, String, Value)>>,
    query: bool,
}
impl Wire {
    fn new(reply: Result<Value, WireError>) -> Arc<Self> {
        Arc::new(Self {
            reply: Mutex::new(Some(reply)),
            calls: Mutex::new(Vec::new()),
            query: true,
        })
    }
}
impl Transport for Wire {
    fn catalog(&self) -> Result<Value, WireError> {
        Ok(json!({"modules":[{"module_id":"entorhinal","roles":[{
            "role":"management_surface", "operations":[{"name":"enumerate","kind":if self.query {"query"} else {"mutate"}}],
            "config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"
        }]}]}))
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        self.calls
            .lock()
            .unwrap()
            .push((module.into(), op.into(), params));
        self.reply
            .lock()
            .unwrap()
            .take()
            .expect("one registry or sink call")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("unexpected tool call")
    }
}

#[test]
fn workspace_registry_uses_entorhinal_management_query_and_exact_id() {
    for (workspaces, exists) in [
        (
            json!([{"workspaceId":"team space","name":"Team","root":null}]),
            true,
        ),
        (json!([]), false),
        (json!([{"workspaceId":"other","name":"team space"}]), false),
    ] {
        let wire = Wire::new(Ok(
            json!({"result":{"workspaces":workspaces,"projects":[],"generation":1}}),
        ));
        let catalog = SubcCatalog::new(wire.clone());
        assert_eq!(catalog.workspace_known("team space").unwrap(), exists);
        assert_eq!(
            *wire.calls.lock().unwrap(),
            vec![(
                "entorhinal".into(),
                "enumerate".into(),
                json!({"workspaceId":"team space"})
            )]
        );
    }
}

#[test]
fn unreadable_workspace_registry_fails_closed() {
    for reply in [
        Err(WireError::NeverSent("offline".into())),
        Ok(json!({})),
        Ok(json!({"result":{"workspaces":null}})),
        Ok(json!({"result":{"workspaces":[{}]}})),
        Ok(json!({"result":{"workspaces":[{"workspaceId":"team"},{}]}})),
    ] {
        let catalog = SubcCatalog::new(Wire::new(reply));
        assert!(catalog.workspace_known("team").is_err());
    }
    let wire = Arc::new(Wire {
        reply: Mutex::new(None),
        calls: Mutex::new(Vec::new()),
        query: false,
    });
    assert!(
        SubcCatalog::new(wire.clone())
            .workspace_known("team")
            .is_err()
    );
    assert!(wire.calls.lock().unwrap().is_empty());
}

#[test]
fn audience_agent_registry_resolves_names_and_ids_and_fails_closed() {
    for reference in ["SYNAPSE", "agent_owner"] {
        let wire = Wire::new(Ok(
            json!({"agents":[{"agent_id":"agent_owner","name":"SYNAPSE"}]}),
        ));
        let catalog = SubcCatalog::new(wire.clone());
        assert_eq!(catalog.agent_id(reference).as_deref(), Some("agent_owner"));
        assert_eq!(
            *wire.calls.lock().unwrap(),
            vec![(CORE.into(), "agent.list".into(), json!({}))]
        );
    }
    for reply in [
        Err(WireError::NeverSent("offline".into())),
        Ok(json!({})),
        Ok(json!({"agents":[{"name":"SYNAPSE"}]})),
    ] {
        assert_eq!(SubcCatalog::new(Wire::new(reply)).agent_id("SYNAPSE"), None);
    }
}

#[test]
fn core_audience_refusal_and_recipient_are_preserved() {
    for kind in [Primitive::SinkDigest, Primitive::SinkStatus] {
        let wire = Wire::new(Err(WireError::Refused {
            code: "sink_target_not_in_audience".into(),
            message: "recipient moved".into(),
        }));
        let host = CoreHost::new(wire.clone());
        let envelope = intent(
            kind,
            &json!({"agent":"outside","item":{},"value":"hello","action":"piggyback"}),
            IntentContext {
                flow_id: "flow",
                version: 1,
                run_id: "run",
                position: 0,
                due_at: 1,
                created_at: 1,
            },
        );
        let request = CallRequest {
            flow_id: "flow".into(),
            run_id: "run".into(),
            position: 0,
            kind: CallKind::Primitive(kind),
            args: JsonText::new(envelope.to_string()).unwrap(),
            idempotency_key: "key".into(),
            attempt: 1,
        };
        let Dispatched::Completed(outcome) = host.dispatch(&request).unwrap() else {
            panic!("not completed")
        };
        assert_eq!(outcome.settlement, Settlement::Rejected);
        assert_eq!(
            serde_json::from_str::<Value>(outcome.value.as_str()).unwrap(),
            json!({"code":"sink_target_not_in_audience","message":"recipient moved"})
        );
        let calls = wire.calls.lock().unwrap();
        assert_eq!(calls[0].0, CORE);
        assert_eq!(calls[0].1, kind.name());
        assert_eq!(calls[0].2["agent"], "outside");
    }
}
