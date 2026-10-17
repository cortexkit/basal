//! Agent flow management through the existing flow.* management handlers, which
//! decide ownership and permissions for each operation.

use base64::{Engine, engine::general_purpose::STANDARD};
use cortexkit_role_tool_provider::{
    CallMode,
    call::ToolCallRequest,
    catalog::{CatalogTool, schema_digest},
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subc_protocol::{
    ErrorBody, Principal,
    scope::{ScopeKind, ScopeSelector},
};

use crate::{caller::Caller, module::Module, tool::Context};

pub const DESCRIPTION: &str = "Manage your own flows; installs need operator approval before running.\ninstall: read script_path and manifest_path from your files and submit the exact bytes for approval, with a capture dry run.\ndry_run: preview flow_id with optional version and window (for example 6h) or a sample trigger; no live effects.\ndisable: stop flow_id, with an optional reason.\nenable: resume flow_id if you disabled it yourself.\nlist: show your flows, optionally filtered by flow_ids.\ninstantiate: create or extend a bounded template with params and expires_in; version is optional and defaults to the newest approved.\nend: end your template instance by flow_id.";

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "action":{"type":"string","enum":["install","dry_run","disable","enable","list","instantiate","end"]},
        "script_path":{"type":"string"},"manifest_path":{"type":"string"},
        "flow_id":{"type":"string"},"version":{"type":"integer","minimum":1,"maximum":u32::MAX},
        "window":{"type":"string"},"trigger":{},"reason":{"type":"string"},
        "flow_ids":{"type":"array","items":{"type":"string"}},
        "template":{"type":"string"},"params":{"type":"object"},"expires_in":{"type":"string"}
    },"required":["action"],"additionalProperties":false})
}

pub fn catalog_tool() -> CatalogTool {
    let schema = schema();
    let mut tool = CatalogTool::new(
        "flow",
        schema_digest(&schema).expect("static schema"),
        1,
        schema,
    )
    .with_capabilities(vec!["basal:flow.manage/v1".into()])
    .with_modes(vec![CallMode::Classic]);
    tool.description = Some(DESCRIPTION.into());
    tool.result_format = Some(cortexkit_role_tool_provider::result::ResultFormat::ContentV1);
    tool
}

fn storage(error: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new("storage", error.to_string())
}

pub(crate) fn wire(error: basal_host::transport::WireError) -> ErrorBody {
    use basal_host::transport::WireError;
    match error {
        WireError::Refused { code, message } => ErrorBody::new(code, message),
        WireError::RefusedDetails {
            code,
            message,
            detail,
        } => ErrorBody::new(code, message).with_detail(detail),
        WireError::Typed(refusal) => {
            let mut body = ErrorBody::new(refusal.reason.as_str(), refusal.message());
            body.detail = refusal.detail;
            body
        }
        other => ErrorBody::new("flow_read_failed", format!("{other:?}")),
    }
}

fn read_scope(context: &Context) -> Result<ScopeSelector, ErrorBody> {
    let scope = context.scope.as_ref().filter(|scope| {
        matches!(&scope.owner, Principal::Reserved { module_id } if module_id == "prefrontal-core")
            && scope.owner_authorized
            && scope.scope_ref.starts_with("flow-read/")
            && scope.scope_ref.len() > "flow-read/".len()
            && scope.kind == ScopeKind::Ephemeral
    }).ok_or_else(|| ErrorBody::new("flow_read_scope_missing", "install needs core's ephemeral flow-read scope"))?;
    Ok(ScopeSelector {
        owner: scope.owner.clone(),
        scope_ref: scope.scope_ref.clone(),
        scope_epoch: Some(scope.scope_epoch),
    })
}

impl Module {
    pub(crate) fn flow_tool(
        &self,
        context: &Context,
        request: &ToolCallRequest,
    ) -> Result<Value, ErrorBody> {
        if let Some(pin) = cortexkit_role_tool_provider::call::check_call(request)
            .map_err(crate::tool::provider_error)?
        {
            let tool = catalog_tool();
            if pin.schema_digest != tool.schema_digest {
                return Err(ErrorBody::new(
                    "tool_schema_changed",
                    "flow input schema changed",
                ));
            }
            if pin.semantics != tool.semantics {
                return Err(ErrorBody::new(
                    "tool_semantics_changed",
                    "flow semantics changed",
                ));
            }
        }
        let action = request.arguments["action"]
            .as_str()
            .ok_or_else(|| crate::tool::invalid("action", "expected an action"))?;
        if action == "install" {
            read_scope(context)?;
        }
        let caller = crate::caller::from_route(Some(&context.principal), context.scope.as_ref());
        let Caller::Agent { agent_id, .. } = &caller else {
            return Err(ErrorBody::new(
                "scope_mismatch",
                "flow needs a core-owned agent scope",
            ));
        };
        let carrier = crate::tool::principal(&context.principal)?;
        // One carrier's call_key identifies one invocation and its saved reply.
        // Serialize duplicate calls without keeping a SQLite transaction open
        // during file reads, dry runs or consent-plane calls.
        let _serial = basal_proto::util::lock(&self.flow_tool_serial);
        if let Some(key) = &request.call_key {
            let record: Option<(String, Option<String>)> = self
                .rt
                .store()
                .read(|c| {
                    Ok(c.query_row(
                        "SELECT agent,result FROM flow_tool_calls WHERE carrier=?1 AND call_key=?2",
                        rusqlite::params![carrier, key],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?)
                })
                .map_err(storage)?;
            if let Some((agent, result)) = record {
                if agent != *agent_id {
                    return Err(crate::tool::invalid(
                        "call_key",
                        "this carrier already used the key for another agent",
                    ));
                }
                return match result {
                    Some(result) => serde_json::from_str::<Result<Value, ErrorBody>>(&result)
                        .map_err(storage)?,
                    None => Err(ErrorBody::new(
                        "outcome_unknown",
                        "the earlier flow call may have taken effect; inspect before repeating",
                    )),
                };
            }
            self.rt
                .store()
                .write(|tx| {
                    tx.execute(
                        "INSERT INTO flow_tool_calls(carrier,call_key,agent) VALUES (?1,?2,?3)",
                        rusqlite::params![carrier, key, agent_id],
                    )?;
                    Ok(())
                })
                .map_err(storage)?;
        }
        let result = self.flow_action(context, &caller, action, request.arguments.clone());
        if let Some(key) = &request.call_key {
            let encoded = serde_json::to_string(&result).map_err(storage)?;
            self.rt
                .store()
                .write(|tx| {
                    tx.execute(
                        "UPDATE flow_tool_calls SET result=?3 WHERE carrier=?1 AND call_key=?2",
                        rusqlite::params![carrier, key, encoded],
                    )?;
                    Ok(())
                })
                .map_err(storage)?;
        }
        result
    }

    fn read_flow_file(
        &self,
        context: &Context,
        path: &str,
        max: usize,
    ) -> Result<String, ErrorBody> {
        let scope = read_scope(context)?;
        let reply = self
            .flow_transport
            .flow_read_exact(&scope, &context.bind_identity, path)
            .map_err(wire)?;
        let malformed =
            || ErrorBody::new("flow_read_bad_response", "invalid file.read_exact response");
        let size = reply["size"].as_u64().ok_or_else(malformed)?;
        if size > max as u64 {
            return Err(ErrorBody::new(
                "file_read_exact_too_large",
                format!("{path} exceeds {max} bytes"),
            ));
        }
        let encoded = reply["bytes_base64"].as_str().ok_or_else(malformed)?;
        if encoded.len() > max.div_ceil(3) * 4 {
            return Err(ErrorBody::new(
                "file_read_exact_too_large",
                format!("{path} exceeds {max} bytes"),
            ));
        }
        let bytes = STANDARD.decode(encoded).map_err(|_| malformed())?;
        if bytes.len() > max {
            return Err(ErrorBody::new(
                "file_read_exact_too_large",
                format!("{path} exceeds {max} bytes"),
            ));
        }
        if size != bytes.len() as u64 {
            return Err(malformed());
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if reply["sha256"].as_str() != Some(digest.as_str()) {
            return Err(ErrorBody::new(
                "flow_read_digest_mismatch",
                format!("digest mismatch reading {path}"),
            ));
        }
        String::from_utf8(bytes)
            .map_err(|_| ErrorBody::new("invalid_params", format!("{path} is not UTF-8")))
    }

    fn flow_action(
        &self,
        context: &Context,
        caller: &Caller,
        action: &str,
        arguments: Value,
    ) -> Result<Value, ErrorBody> {
        let (method, allowed): (&str, &[&str]) = match action {
            "install" => ("flow.install", &["script_path", "manifest_path"]),
            "dry_run" => ("flow.dry_run", &["flow_id", "version", "window", "trigger"]),
            "disable" => ("flow.disable", &["flow_id", "reason"]),
            "enable" => ("flow.enable", &["flow_id"]),
            "list" => ("flow.list", &["flow_ids"]),
            "instantiate" => (
                "flow.instantiate",
                &["template", "version", "params", "expires_in"],
            ),
            "end" => ("flow.instance.end", &["flow_id"]),
            _ => return Err(crate::tool::invalid("action", "unknown flow action")),
        };
        let mut params = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| crate::tool::invalid("arguments", "expected an object"))?;
        params.remove("action");
        if let Some(key) = params.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(crate::tool::invalid(
                key,
                "argument is not accepted for this action",
            ));
        }
        if action == "install" {
            let path = |field: &str| {
                params
                    .get(field)
                    .and_then(Value::as_str)
                    .ok_or_else(|| crate::tool::invalid(field, "expected a path"))
            };
            let script = self.read_flow_file(context, path("script_path")?, 1_048_576)?;
            let manifest = self.read_flow_file(context, path("manifest_path")?, 65_536)?;
            params = serde_json::from_value(json!({"script":script,"manifest":manifest}))
                .expect("install arguments");
        }
        if action == "instantiate" && !params.contains_key("version") {
            let package = params
                .get("template")
                .and_then(Value::as_str)
                .ok_or_else(|| crate::tool::invalid("template", "expected a template name"))?;
            let version = self
                .flow_transport
                .newest_approved_template_version(package)
                .map_err(wire)?
                .ok_or_else(|| ErrorBody::new("package_unknown", "no approved template version"))?;
            params.insert("version".into(), json!(version));
        }
        let result = self
            .handle(caller, method, Value::Object(params))
            .map_err(|error| {
                let mut body = ErrorBody::new(error.code, error.message);
                body.detail = error.detail;
                body
            })?;
        use cortexkit_role_tool_provider::result::{ContentPart, ToolResult};
        let reply = ToolResult::new(vec![ContentPart::text(summary(action, &result))])
            .with_structured_content(
                result
                    .as_object()
                    .cloned()
                    .expect("flow op returns an object"),
            );
        serde_json::to_value(reply).map_err(storage)
    }
}

fn summary(action: &str, result: &Value) -> String {
    match action {
        "install" => format!(
            "Install {}: card {}, code hash {}.\nWarnings: {}\nDry run: {}",
            result["flow_id"].as_str().unwrap_or(""),
            result["card_id"],
            result["code_hash"],
            result["warnings"],
            result["dry_run"]
        ),
        "list" => {
            let lines: Vec<_> = result["flows"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|flow| {
                    format!(
                        "{}: {}; approved version {}; last run {}",
                        flow["flow_id"].as_str().unwrap_or(""),
                        flow["state"].as_str().unwrap_or(""),
                        flow["approved_version"],
                        flow["last_run"]
                    )
                })
                .collect();
            if lines.is_empty() {
                "No flows.".into()
            } else {
                lines.join("\n")
            }
        }
        _ => format!("{action}: {result}"),
    }
}
