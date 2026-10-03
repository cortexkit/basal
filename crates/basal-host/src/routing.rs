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
}
impl ModuleOpsHost {
    pub fn new(transport: Arc<dyn Transport>, catalog: Arc<SubcCatalog>) -> Self {
        Self {
            transport,
            catalog,
            keyed: BTreeSet::new(),
        }
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
            let decl = self
                .catalog
                .resolve(module, op)
                .map_err(|e| {
                    WireError::NeverSent(format!("catalog lookup before dispatch: {e:?}"))
                })?
                .ok_or_else(|| WireError::Refused {
                    code: "op_missing".into(),
                    message: "operation is not catalogued".into(),
                })?;
            if decl.declaration.shell_capable {
                return Err(WireError::Refused {
                    code: "shell_forbidden".into(),
                    message: "flows cannot run shell operations".into(),
                });
            }
            if expected.is_some_and(|class| class != self.declared_class(module, op, &decl)) {
                return Err(WireError::Refused {
                    code: "op_kind_changed".into(),
                    message: "catalog declaration changed after the retry policy was journaled"
                        .into(),
                });
            }
            let args = serde_json::from_str(request.args.as_str())
                .map_err(|e| WireError::NeverSent(e.to_string()))?;
            match decl.surface {
                Surface::Management => self.transport.management(module, op, args),
                Surface::Tool => self
                    .transport
                    .tool(module, op, args, &request.idempotency_key),
            }
        })();
        outcome(result)
    }
}
impl Host for ModuleOpsHost {
    fn classify(&self, kind: &CallKind) -> CallClass {
        if let CallKind::Op { module, op } = kind {
            if let Some(decl) = self.catalog.resolve(module, op).ok().flatten() {
                return self.declared_class(module, op, &decl);
            }
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
    fn install_status(
        &self,
        flow_id: &str,
        version: u32,
    ) -> Result<crate::InstallStatus, TransportError> {
        self.core.install_status(flow_id, version)
    }
}
