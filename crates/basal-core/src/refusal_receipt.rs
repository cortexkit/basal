//! Durable post-commit refusal acknowledgements, independent of execution storage.
use crate::{
    CoreError, Result,
    model::{json, position_i64 as pos, to_u64},
};
use basal_proto::CallKind;

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SavedRefusal {
    flow_id: String,
    run_id: String,
    position: i64,
    kind_code: i64,
    module: Option<String>,
    op: Option<String>,
    args: String,
    key: String,
    attempt: u32,
    detail: String,
    #[serde(default)]
    targeted: bool,
}

impl SavedRefusal {
    pub(crate) fn new(
        request: &basal_host::CallRequest,
        refusal: &basal_host::flow_refusal::FlowRefusal,
    ) -> Result<Self> {
        let (kind_code, module, op) = crate::model::kind_columns(&request.kind)?;
        Ok(Self {
            flow_id: request.flow_id.clone(),
            run_id: request.run_id.clone(),
            position: pos(request.position)?,
            kind_code,
            module: module.map(str::to_owned),
            op: op.map(str::to_owned),
            args: request.args.as_str().into(),
            key: request.idempotency_key.clone(),
            attempt: request.attempt,
            detail: serde_json::to_string(refusal)
                .map_err(|e| CoreError::Invalid(e.to_string()))?,
            targeted: matches!(request.kind, CallKind::OpFor { .. }),
        })
    }
    pub(crate) fn request(&self) -> Result<basal_host::CallRequest> {
        Ok(basal_host::CallRequest {
            flow_version: None,
            flow_id: self.flow_id.clone(),
            run_id: self.run_id.clone(),
            position: to_u64(self.position, "position")?,
            kind: match crate::model::kind_from_columns(
                self.kind_code,
                self.module.clone(),
                self.op.clone(),
            )? {
                CallKind::Op { module, op } if self.targeted => CallKind::OpFor { module, op },
                kind => kind,
            },
            args: json(self.args.clone(), "dispatch args")?,
            idempotency_key: self.key.clone(),
            attempt: self.attempt,
        })
    }

    pub(crate) fn refusal(&self) -> Result<basal_host::flow_refusal::FlowRefusal> {
        if serde_json::from_str::<basal_host::flow_refusal::FlowRefusal>(&self.detail).is_err() {
            tracing::warn!(run_id=%self.run_id,position=self.position,"ignoring malformed optional refusal fields");
        }
        decode_refusal(&self.detail)
    }
}

/// Optional retry/display fields are not part of a refusal's identity. Older
/// stored rows validated reason/provider/action but could retain malformed
/// optional fields; discard only those fields and still validate the identity.
pub(crate) fn decode_refusal(detail: &str) -> Result<basal_host::flow_refusal::FlowRefusal> {
    if let Ok(refusal) = serde_json::from_str(detail) {
        return Ok(refusal);
    }
    let mut value: serde_json::Value =
        serde_json::from_str(detail).map_err(|e| CoreError::Corrupt(e.to_string()))?;
    value["retry_after_ms"] = serde_json::Value::Null;
    value["detail"] = serde_json::Value::Null;
    serde_json::from_value(value).map_err(|e| CoreError::Corrupt(e.to_string()))
}
