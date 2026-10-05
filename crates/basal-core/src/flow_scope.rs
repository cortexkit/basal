//! Health retained after a readiness rejection, independently of whether the
//! script caught it. The call's actual outcome still uses the issue log/mailbox.
use crate::error::Result;
use basal_host::flow_refusal::FlowRefusal;
use rusqlite::{Transaction, params};

pub(crate) fn health(tx: &Transaction, flow: &str, refusal: &FlowRefusal) -> Result<()> {
    let value = serde_json::json!({"reason":refusal.reason,"provider":refusal.provider,"action":refusal.action,"message":refusal.message(),"detail":refusal.detail});
    tx.execute(
        "UPDATE flows SET scope_health=?2 WHERE flow_id=?1",
        params![flow, value.to_string()],
    )?;
    Ok(())
}
