//! Send module ops to their providers and grant-checked facts and sinks to
//! core. Model calls use a separate host that can be replaced independently.
use crate::builtins::BuiltinHost;
use crate::core_host::{outcome, sample, system_now};
use crate::subc_catalog::{SubcCatalog, Surface};
use crate::transport::{Transport, WireError};
use crate::{CallClass, CallRequest, CompletionSink, Dispatched, Host, OpKind, TransportError};
use basal_proto::{CallKind, Primitive};
use std::collections::BTreeSet;
use std::sync::Arc;

pub struct ModuleOpsHost {
    transport: Arc<dyn Transport>,
    catalog: Arc<SubcCatalog>,
    keyed: BTreeSet<(String, String)>,
    scope_describer: Option<crate::scope_describe::ModuleScopeDescriber>,
}
impl ModuleOpsHost {
    pub fn new(transport: Arc<dyn Transport>, catalog: Arc<SubcCatalog>) -> Self {
        Self {
            transport,
            catalog,
            keyed: BTreeSet::new(),
            scope_describer: None,
        }
    }
    /// Attach the registered module connection that may read daemon scopes.
    /// The module's serve future must keep running on `runtime`; admission
    /// invokes this blocking host from a separate thread.
    pub fn with_scope_describer(
        mut self,
        handle: subc_client_rs::ModuleHandle,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        self.scope_describer = Some(crate::scope_describe::ModuleScopeDescriber::new(
            handle, runtime,
        ));
        self
    }
    /// Only operator-approved tool contracts may make a mutation repeatable.
    /// Management calls have no typed call-key field in this protocol.
    pub fn with_idempotent_ops(mut self, ops: BTreeSet<(String, String)>) -> Self {
        self.keyed = ops;
        self
    }
    fn declared_class(
        &self,
        module: &str,
        op: &str,
        decl: &crate::subc_catalog::CatalogOp,
    ) -> CallClass {
        if decl.declaration.kind == Some(OpKind::Query) {
            return CallClass::Query;
        }
        let unfenceable =
            decl.execution_mode == Some(subc_protocol::manifest::ExecutionMode::Unfenceable);
        CallClass::Mutation {
            honours_idempotency_keys: decl.surface == Surface::Tool
                && !unfenceable
                && self.keyed.contains(&(module.to_owned(), op.to_owned())),
        }
    }
    fn dispatch_checked(
        &self,
        request: &CallRequest,
        expected: Option<CallClass>,
    ) -> Result<Dispatched, TransportError> {
        let CallKind::Op { module, op } = &request.kind else {
            return outcome(Err(WireError::NeverSent("not a module op".into())));
        };
        let result = (|| {
            let (decl, flow_capable) =
                self.catalog.resolve_for_dispatch(module, op).map_err(|e| {
                    WireError::NeverSent(format!("catalog lookup before dispatch: {e:?}"))
                })?;
            if module == crate::subc_catalog::CORE && !flow_capable {
                return Err(WireError::Refused {
                    code: "basal_scope_bug".into(),
                    message: format!("core flow op {op} cannot use the carrier route"),
                });
            }
            let decl = decl.ok_or_else(|| WireError::Refused {
                code: "op_missing".into(),
                message: "operation is not catalogued".into(),
            })?;
            if decl.declaration.shell_capable {
                return Err(WireError::Refused {
                    code: "shell_forbidden".into(),
                    message: "flows cannot run shell operations".into(),
                });
            }
            // The call's class was journaled when it was issued, and the
            // retry policy for an unknown outcome was chosen from it. If the
            // catalog could not resolve the op then, the call was journaled
            // as a mutation that does not honour idempotency keys, the class
            // that is never repeated; whatever the catalog declares now,
            // that stricter policy stays safe, so the call proceeds. A call
            // journaled as safe to repeat (a query, or a mutation that
            // deduplicates on its key) is refused if the op's declared class
            // has changed since: its policy may resend it, and that is safe
            // only while the op still behaves as it was declared.
            if expected.is_some_and(|class| {
                class.safe_to_repeat() && class != self.declared_class(module, op, &decl)
            }) {
                return Err(WireError::Refused {
                    code: "op_kind_changed".into(),
                    message: "catalog declaration changed after the retry policy was journaled"
                        .into(),
                });
            }
            let args = serde_json::from_str(request.args.as_str())
                .map_err(|e| WireError::NeverSent(e.to_string()))?;
            match decl.surface {
                Surface::Management => {
                    self.transport
                        .management_for_flow(&request.flow_id, module, op, args)
                }
                Surface::Tool => self.transport.tool_for_flow(
                    &request.flow_id,
                    module,
                    op,
                    args,
                    &request.idempotency_key,
                ),
            }
        })();
        outcome(result.map_err(|e| crate::transport::contextual(e, module, op)))
    }
}
impl Host for ModuleOpsHost {
    fn scope_describe(
        &self,
        owner: &subc_protocol::Principal,
        scope_ref: &str,
    ) -> Result<crate::ScopeDescription, crate::ScopeDescribeError> {
        self.scope_describer
            .as_ref()
            .ok_or(crate::ScopeDescribeError::Unavailable)?
            .describe(owner, scope_ref)
    }
    fn provider_ready(
        &self,
        flow_id: &str,
        kind: &CallKind,
    ) -> Result<(), crate::flow_refusal::FlowRefusal> {
        if let CallKind::Op { module, op } = kind {
            self.transport.provider_ready(flow_id, module, op)
        } else {
            Ok(())
        }
    }
    fn configure_flow(
        &self,
        flow_id: &str,
        agent_owned: bool,
        scope: Option<crate::flow_scope::RegisteredScope>,
    ) {
        self.transport.configure_flow(flow_id, agent_owned, scope);
    }
    fn classify(&self, kind: &CallKind) -> CallClass {
        if let CallKind::Op { module, op } = kind
            && let Some(decl) = self.catalog.resolve(module, op).ok().flatten()
        {
            return self.declared_class(module, op, &decl);
        }
        CallClass::Mutation {
            honours_idempotency_keys: false,
        }
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        self.dispatch_checked(request, None)
    }
    fn dispatch_classified(
        &self,
        request: &CallRequest,
        class: CallClass,
    ) -> Result<Dispatched, TransportError> {
        self.dispatch_checked(request, Some(class))
    }
    fn now_ms(&self) -> f64 {
        system_now()
    }
    fn random(&self) -> f64 {
        sample()
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
}

pub struct RoutingHost {
    ops: Arc<dyn Host>,
    core: Arc<dyn Host>,
    model: Arc<dyn Host>,
    builtins: Arc<dyn Host>,
}
impl RoutingHost {
    /// Routes the file, git and network built-ins to basal's own
    /// [`BuiltinHost`] with production settings.
    pub fn new(ops: Arc<dyn Host>, core: Arc<dyn Host>, model: Arc<dyn Host>) -> Self {
        Self {
            ops,
            core,
            model,
            builtins: Arc::new(BuiltinHost::default()),
        }
    }
    /// Replaces the host the built-ins go to.
    pub fn with_builtins(mut self, builtins: Arc<dyn Host>) -> Self {
        self.builtins = builtins;
        self
    }
    fn target(&self, kind: &CallKind) -> &dyn Host {
        match kind {
            CallKind::Op { .. } => self.ops.as_ref(),
            CallKind::Primitive(
                Primitive::SinkDigest | Primitive::SinkStatus | Primitive::Facts,
            ) => self.core.as_ref(),
            CallKind::Primitive(p) if p.is_builtin() => self.builtins.as_ref(),
            _ => self.model.as_ref(),
        }
    }
}
impl Host for RoutingHost {
    fn scope_describe(
        &self,
        owner: &subc_protocol::Principal,
        scope_ref: &str,
    ) -> Result<crate::ScopeDescription, crate::ScopeDescribeError> {
        self.ops.scope_describe(owner, scope_ref)
    }
    fn provider_ready(
        &self,
        flow_id: &str,
        kind: &CallKind,
    ) -> Result<(), crate::flow_refusal::FlowRefusal> {
        // kv, now and random are answered inside basal and reach no provider,
        // so they never need a flow scope. Without this, a flow with no model
        // grant (and so no Broca in its scope) would be refused its own kv.
        if let CallKind::Primitive(p) = kind
            && !p.is_builtin()
            && !matches!(
                p,
                Primitive::Llm
                    | Primitive::Classify
                    | Primitive::SinkDigest
                    | Primitive::SinkStatus
                    | Primitive::Facts
            )
        {
            return Ok(());
        }
        self.target(kind).provider_ready(flow_id, kind)
    }
    fn refusal_committed(&self, request: &CallRequest, refusal: &crate::flow_refusal::FlowRefusal) {
        self.target(&request.kind)
            .refusal_committed(request, refusal);
    }
    fn scope_checks(&self, enabled: bool) {
        self.model.scope_checks(enabled);
    }
    fn configure_flow(
        &self,
        flow_id: &str,
        agent_owned: bool,
        scope: Option<crate::flow_scope::RegisteredScope>,
    ) {
        self.ops.configure_flow(flow_id, agent_owned, scope.clone());
        self.model.configure_flow(flow_id, agent_owned, scope);
    }
    fn classify(&self, kind: &CallKind) -> CallClass {
        self.target(kind).classify(kind)
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        self.target(&request.kind).dispatch(request)
    }
    fn dispatch_classified(
        &self,
        request: &CallRequest,
        class: CallClass,
    ) -> Result<Dispatched, TransportError> {
        self.target(&request.kind)
            .dispatch_classified(request, class)
    }
    fn dispatch_committed(&self, request: &CallRequest) {
        self.target(&request.kind).dispatch_committed(request);
    }
    fn now_ms(&self) -> f64 {
        self.ops.now_ms()
    }
    fn random(&self) -> f64 {
        self.ops.random()
    }
    fn attach(&self, sink: Arc<dyn CompletionSink>) {
        self.ops.attach(sink.clone());
        self.core.attach(sink.clone());
        self.model.attach(sink);
    }
    fn bind_fs_temps(&self, ledger: Arc<dyn crate::builtins::fs::TempLedger>) {
        self.builtins.bind_fs_temps(ledger);
    }
    fn install_status(
        &self,
        flow_id: &str,
        version: u32,
    ) -> Result<crate::InstallStatus, TransportError> {
        self.core.install_status(flow_id, version)
    }
}
