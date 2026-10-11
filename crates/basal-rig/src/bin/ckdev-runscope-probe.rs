//! A tool provider for the isolated ckdev-flows rig, never production.
//! Identity comes only from the daemon's bind stamp, not tool arguments.
//! The daemon owns scope expiry and closes routes when their scope ends.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;
use subc_client_rs::{
    BindDecision, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest, RouteHandle,
};
use subc_protocol::manifest::{
    CapabilityDeclarations, Concurrency, ExecutionMode, ModuleManifest, ProviderRole, Tool,
};
use subc_protocol::scope::ScopeStamp;
use subc_protocol::tool_call::ToolCallRequest;
use subc_protocol::{PROTOCOL_VERSION, Principal};

fn manifest() -> ModuleManifest {
    ModuleManifest::builder("ckdev-runscope-probe", env!("CARGO_PKG_VERSION"))
        .protocol_ver(PROTOCOL_VERSION)
        .provides(vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "probe_echo".into(),
                description: Some("Echo text and the daemon-attested run identity (rig only).".into()),
                execution_mode: ExecutionMode::Unfenceable,
                schema: json!({"type":"object","required":["text"],"properties":{"text":{"type":"string"}},"additionalProperties":false}),
            }],
            identity_scope: vec![],
            concurrency: Concurrency::ModuleManaged,
            emits_push: false,
            sub_supervises: false,
        }])
        .capabilities(Some(CapabilityDeclarations {
            provides: vec!["agent-run-scopes/v1".into()],
            requires: vec![],
            must_never_reach: vec![],
        }))
        .build()
}

fn identity(scope: Option<&ScopeStamp>) -> Result<(&str, &str), &'static str> {
    let scope = scope.ok_or("run_scope_required")?;
    let run_id = scope
        .attributes
        .run_id
        .as_deref()
        .ok_or("run_scope_required")?;
    let core_owner = scope.owner
        == (Principal::Reserved {
            module_id: "prefrontal-core".into(),
        });
    if !core_owner {
        return Err("scope_mismatch");
    }
    if !scope.owner_authorized {
        return Err("scope_mismatch");
    }
    if scope.attributes.delegates {
        return Err("scope_mismatch");
    }
    if scope.attributes.flow_id.is_some() {
        return Err("scope_mismatch");
    }
    let agent_id = scope
        .attributes
        .agent_id
        .as_deref()
        .ok_or("scope_mismatch")?;
    Ok((agent_id, run_id))
}

#[derive(Default)]
struct Probe {
    scopes: Mutex<HashMap<(u16, u32), ScopeStamp>>,
}

#[async_trait]
impl ModuleHandler for Probe {
    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        if let Some(scope) = &request.scope {
            self.scopes.lock().unwrap().insert(
                (request.handle.channel, request.handle.epoch),
                scope.clone(),
            );
        }
        BindDecision::accept()
    }

    async fn on_route_gone(&self, handle: &RouteHandle) {
        self.scopes
            .lock()
            .unwrap()
            .remove(&(handle.channel, handle.epoch));
    }

    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let scopes = self.scopes.lock().unwrap();
        let handle = ctx.route_handle();
        let scope = scopes.get(&(handle.channel, handle.epoch));
        let (agent_id, run_id) = match identity(scope) {
            Ok(identity) => identity,
            Err(code) => {
                return HandlerOutcome::Error {
                    code: code.into(),
                    message: "probe needs a core-owned agent run scope".into(),
                };
            }
        };
        let request: ToolCallRequest = match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(e) => {
                return HandlerOutcome::Error {
                    code: "invalid_request".into(),
                    message: e.to_string(),
                };
            }
        };
        if request.name != "probe_echo" || !request.arguments["text"].is_string() {
            return HandlerOutcome::Error {
                code: "invalid_request".into(),
                message: "expected probe_echo with string text".into(),
            };
        }
        HandlerOutcome::Response(
            serde_json::to_vec(
                &json!({"text":request.arguments["text"],"agent_id":agent_id,"run_id":run_id}),
            )
            .unwrap(),
        )
    }
}

fn main() -> std::process::ExitCode {
    let runtime = tokio::runtime::Runtime::new().expect("probe runtime");
    match runtime.block_on(subc_client_rs::serve(manifest(), Probe::default())) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ckdev-runscope-probe: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp() -> ScopeStamp {
        serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"run:rig","scope_epoch":3,"kind":"ephemeral","attributes":{"agent_id":"agent-rig","run_id":"run-rig"},"owner_authorized":true})).unwrap()
    }

    #[test]
    fn accepts_core_owned_run_identity() {
        let stamp = stamp();
        assert_eq!(identity(Some(&stamp)), Ok(("agent-rig", "run-rig")));
    }

    #[test]
    fn requires_run_id() {
        let mut stamp = stamp();
        stamp.attributes.run_id = None;
        assert_eq!(identity(Some(&stamp)), Err("run_scope_required"));
        assert_eq!(identity(None), Err("run_scope_required"));
    }

    #[test]
    fn requires_agent_id() {
        let mut stamp = stamp();
        stamp.attributes.agent_id = None;
        assert_eq!(identity(Some(&stamp)), Err("scope_mismatch"));
    }

    #[test]
    fn requires_core_owner() {
        let mut stamp = stamp();
        stamp.owner = Principal::Reserved {
            module_id: "other".into(),
        };
        assert_eq!(identity(Some(&stamp)), Err("scope_mismatch"));
    }

    #[test]
    fn requires_authorized_owner() {
        let mut stamp = stamp();
        stamp.owner_authorized = false;
        assert_eq!(identity(Some(&stamp)), Err("scope_mismatch"));
    }

    #[test]
    fn refuses_delegation() {
        let mut stamp = stamp();
        stamp.attributes.delegates = true;
        assert_eq!(identity(Some(&stamp)), Err("scope_mismatch"));
    }

    #[test]
    fn refuses_flow_scope() {
        let mut stamp = stamp();
        stamp.attributes.flow_id = Some("flow-rig".into());
        assert_eq!(identity(Some(&stamp)), Err("scope_mismatch"));
    }
}
