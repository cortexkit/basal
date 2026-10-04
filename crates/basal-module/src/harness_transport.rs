//! A fake provider backed by MockHost's persistent effect records. The real
//! adapters see provider replies while process kills retain prior effects.
use basal_host::mock::MockHost;
use basal_host::transport::{Transport, WireError};
use basal_host::{CallRequest, Dispatched, Host};
use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use serde_json::{Value, json};

pub(super) struct HarnessTransport(pub MockHost);
impl HarnessTransport {
    fn invoke(&self, kind: CallKind, args: Value, key: String) -> Result<Value, WireError> {
        let request = CallRequest {
            flow_id: args["flow_id"].as_str().unwrap_or("harness").into(),
            run_id: args["run_id"].as_str().unwrap_or("harness").into(),
            position: args["call_position"].as_u64().unwrap_or(0),
            kind,
            args: JsonText::new(args.to_string())
                .map_err(|e| WireError::NeverSent(e.to_string()))?,
            idempotency_key: key,
            attempt: 1,
        };
        match self.0.dispatch(&request) {
            Err(basal_host::TransportError::Refused(refusal)) => Err(WireError::Typed(refusal)),
            Ok(Dispatched::Completed(outcome)) if outcome.settlement == Settlement::Fulfilled => {
                serde_json::from_str(outcome.value.as_str())
                    .map_err(|e| WireError::Unreadable(e.to_string()))
            }
            Ok(Dispatched::Completed(outcome)) => Err(WireError::Refused {
                code: "mock_refused".into(),
                message: outcome.value.as_str().to_owned(),
            }),
            Ok(Dispatched::Accepted { .. }) => Err(WireError::Unreadable(
                "harness provider accepted a long-running call".into(),
            )),
            // The mock host's ambiguous failures stand for a reply lost with
            // its connection.
            Err(basal_host::TransportError::Unavailable { sent, detail }) => match sent {
                basal_host::Sent::Never => Err(WireError::NeverSent(detail)),
                basal_host::Sent::Maybe(_) => Err(WireError::Unknown(detail)),
            },
        }
    }
}
impl Transport for HarnessTransport {
    fn catalog(&self) -> Result<Value, WireError> {
        Ok(
            json!({"generation":1,"modules":[{"module_id":"mock","roles":[{"role":"management_surface","operations":[{"name":"echo","kind":"query"},{"name":"post","kind":"mutate"}],"identity_scope":[],"config_schema":{},"observability":[],"concurrency":"serial"},{"role":"tool_provider","tools":[{"name":"send","execution_mode":"mutating","schema":{}}],"identity_scope":[],"concurrency":"serial","emits_push":false,"sub_supervises":false}],"control_ops":[]}]}),
        )
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        if module == "prefrontal-core" && op == "sink.digest" {
            let flow = params["flow_id"]
                .as_str()
                .ok_or_else(|| WireError::NeverSent("missing flow id".into()))?;
            let run = params["run_id"]
                .as_str()
                .ok_or_else(|| WireError::NeverSent("missing run id".into()))?;
            let pos = params["call_position"]
                .as_u64()
                .ok_or_else(|| WireError::NeverSent("missing position".into()))?;
            let key = basal_core::ids::idempotency_key(flow, run, pos);
            let replayed = self.0.send_count(&key) > 0;
            self.invoke(
                CallKind::Primitive(Primitive::SinkDigest),
                params,
                key.clone(),
            )?;
            return Ok(
                json!({"disposition":"stored","fire_id":format!("wf_{key}"),"replayed":replayed}),
            );
        }
        self.invoke(
            CallKind::Op {
                module: module.into(),
                op: op.into(),
            },
            params,
            "harness-query".into(),
        )
    }
    fn tool(&self, module: &str, name: &str, args: Value, key: &str) -> Result<Value, WireError> {
        self.invoke(
            CallKind::Op {
                module: module.into(),
                op: name.into(),
            },
            args,
            key.into(),
        )
    }
}
