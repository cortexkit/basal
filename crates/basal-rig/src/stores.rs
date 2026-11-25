//! Read-only views of the stores the suite checks against.
//!
//! The stores are SQLite in WAL mode with live writers, so
//! each read opens the file with SQLite's read-only flag and never writes,
//! checkpoints or migrates anything. Core's store is read for records no core
//! management op returns: the `flow_sink_receipt` rows and its `flow_install`
//! records and registered flow selectors. Basal's store is read for its journal,
//! runs and install cards; Broca's index and WAL independently prove admission.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};

fn open(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open {} read-only: {e}", path.display()))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    Ok(connection)
}

fn retained_projection(c: &Connection) -> Result<bool, String> {
    let exists = |name: &str| {
        c.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
            [name],
            |r| r.get::<_, bool>(0),
        )
        .map_err(|e| e.to_string())
    };
    if exists("rig_runs")? {
        return Ok(true);
    }
    if exists("run_history")? {
        return Err("run-log rig evidence requires retain_detail on the disposable runtime".into());
    }
    // Neither rig_runs nor run_history exists, so use the older SQLite
    // execution tables retained during the run-log transition.
    Ok(false)
}

/// prefrontal's store, read-only.
pub struct CoreStore {
    pub path: PathBuf,
}

/// One sink receipt core recorded: its request key and the reply it stored.
#[derive(Debug, Clone)]
pub struct Receipt {
    pub key: Value,
    pub reply: Value,
}

impl CoreStore {
    /// Policy provenance and the absence of a consent intent are durable facts,
    /// independent of the install response returned by basal.
    pub fn policy_install(&self, flow_id: &str, version: i64) -> Result<Option<Value>, String> {
        let c = open(&self.path)?;
        c.query_row("SELECT code_hash,approved_by,author_agent_id,
            (SELECT count(*) FROM flow_install_intent WHERE flow_id=?1 AND version=?2),
            (SELECT count(*) FROM flow_install_decision_hint WHERE flow_id=?1 AND version=?2)
            FROM flow_install WHERE flow_id=?1 AND version=?2 AND revoked_at_ms IS NULL",params![flow_id,version],|r| Ok(json!({
                "code_hash":r.get::<_,String>(0)?,"approved_by":r.get::<_,String>(1)?,"author_agent_id":r.get::<_,Option<String>>(2)?,
                "card_intents":r.get::<_,i64>(3)?,"card_hints":r.get::<_,i64>(4)?
            }))).optional().map_err(|e|e.to_string())
    }

    /// Observe a card's committed decision and its acknowledged hint independently
    /// of the consent API's answer acknowledgement.
    pub fn install_decision(
        &self,
        flow_id: &str,
        version: i64,
        attempt: i64,
    ) -> Result<Option<Value>, String> {
        let c = open(&self.path)?;
        c.query_row(
            "SELECT t.state,t.code_hash,i.code_hash,i.approved_at_ms,h.terminal_state,h.created_at,h.delivered_at
             FROM flow_install_intent t
             LEFT JOIN flow_install i ON i.flow_id=t.flow_id AND i.version=t.version AND i.revoked_at_ms IS NULL
             LEFT JOIN flow_install_decision_hint h ON h.flow_id=t.flow_id AND h.version=t.version AND h.attempt=t.attempt AND h.terminal_state=t.state
             WHERE t.flow_id=?1 AND t.version=?2 AND t.attempt=?3",
            params![flow_id,version,attempt],
            |r| Ok(json!({"state":r.get::<_,String>(0)?,"code_hash":r.get::<_,String>(1)?,
                "installed_hash":r.get::<_,Option<String>>(2)?,"approved_at_ms":r.get::<_,Option<i64>>(3)?,
                "hint_state":r.get::<_,Option<String>>(4)?,"hint_created_at_ms":r.get::<_,Option<i64>>(5)?,
                "hint_delivered_at_ms":r.get::<_,Option<i64>>(6)?})),
        ).optional().map_err(|e| e.to_string())
    }

    /// Read the run scope core stored for this run instead of trusting basal's
    /// reply to prove that core closed the scope.
    pub fn codemode_scope(&self, run_id: &str) -> Result<Option<Value>, String> {
        let c = open(&self.path)?;
        let row = c.query_row("SELECT agent_id,scope_ref,scope_epoch,catalog_json,lifecycle,end_settled FROM codemode_run_scope WHERE run_id=?1", [run_id], |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, u64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, bool>(5)?
        ))).optional().map_err(|e| e.to_string())?;
        row.map(|(agent,reference,epoch,catalog,lifecycle,settled)| {
            let catalog: Value = serde_json::from_str(&catalog).map_err(|e| e.to_string())?;
        // The synthesized owner mirrors the rig core's module id. Change it
        // when the rig's core is renamed; the stored scope row has no owner.
        Ok(json!({"agent_id":agent,"selector":{"owner":{"kind":"reserved","module_id":"prefrontal"},"ref":reference,"scope_epoch":epoch},"catalog":catalog,"lifecycle":lifecycle,"end_settled":settled}))
        }).transpose()
    }

    /// Read core's install intent for the flow/version/attempt/hash returned by
    /// basal. Its consent-card ID identifies that request even if other attempts
    /// have pending cards for the same flow version.
    pub fn install_intent(
        &self,
        flow_id: &str,
        version: i64,
        attempt: i64,
        code_hash: &str,
    ) -> Result<Option<Value>, String> {
        let c = open(&self.path)?;
        let row = c.query_row("SELECT provider,request_json,state,elicitation_id FROM flow_install_intent WHERE flow_id=?1 AND version=?2 AND attempt=?3 AND code_hash=?4", params![flow_id,version,attempt,code_hash], |r| Ok((
            r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?
        ))).optional().map_err(|e| e.to_string())?;
        row.map(|(provider,request,state,id)| {
            let request: Value = serde_json::from_str(&request).map_err(|e| e.to_string())?;
            Ok(json!({"flow_id":flow_id,"version":version,"attempt":attempt,"code_hash":code_hash,"provider":provider,"request":request,"state":state,"elicitation_id":id}))
        }).transpose()
    }

    /// Core persists the rendered package request before sending it to consent.
    pub fn package_intent(&self, package: &str) -> Result<Option<Value>, String> {
        let c = open(&self.path)?;
        let row = c.query_row("SELECT version,attempt,provider,request_json,state,elicitation_id FROM package_consent_intent WHERE package=?1 ORDER BY attempt DESC LIMIT 1",[package], |r| Ok((
            r.get::<_, i64>(0)?,r.get::<_, i64>(1)?,r.get::<_, String>(2)?,r.get::<_, String>(3)?,r.get::<_, String>(4)?,r.get::<_, Option<String>>(5)?
        ))).optional().map_err(|e| e.to_string())?;
        row.map(|(version,attempt,provider,request,state,id)| {
            let request: Value = serde_json::from_str(&request).map_err(|e| e.to_string())?;
            Ok(json!({"version":version,"attempt":attempt,"provider":provider,"request":request,"state":state,"elicitation_id":id}))
        }).transpose()
    }

    /// Digest writes are checked independently of receipts, including refused calls.
    pub fn digest_rows(&self, agent_id: &str, flow_id: &str) -> Result<Value, String> {
        let c = open(&self.path)?;
        let mut s = c.prepare("SELECT fire_id, agent_id, origin_flow_id, origin_flow_version FROM wake_fire WHERE agent_id=?1 AND origin_flow_id=?2 ORDER BY rowid")
            .map_err(|e| e.to_string())?;
        s.query_map(params![agent_id, flow_id], |r| {
            Ok(json!({
                "fire_id": r.get::<_, String>(0)?, "agent_id": r.get::<_, String>(1)?,
                "flow_id": r.get::<_, String>(2)?, "version": r.get::<_, i64>(3)?
            }))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
        .map_err(|e| e.to_string())
    }

    /// Every sink receipt of one flow. Core keys a digest receipt
    /// `["sink.digest", flow id, run id, call position]` and a status receipt
    /// `["sink.status", flow id, agent name, status revision]`.
    pub fn receipts(&self, flow_id: &str) -> Result<Vec<Receipt>, String> {
        let c = open(&self.path)?;
        let mut statement = c
            .prepare("SELECT request_key, reply_json FROM flow_sink_receipt ORDER BY rowid")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        let mut receipts = Vec::new();
        for row in rows {
            let (key, reply) = row.map_err(|e| e.to_string())?;
            let key: Value = serde_json::from_str(&key).map_err(|e| e.to_string())?;
            if key.get(1).and_then(Value::as_str) == Some(flow_id) {
                receipts.push(Receipt {
                    key,
                    reply: serde_json::from_str(&reply).map_err(|e| e.to_string())?,
                });
            }
        }
        Ok(receipts)
    }

    /// Core's install records for one flow: version, whether revoked, and
    /// the author it recorded.
    pub fn installs(&self, flow_id: &str) -> Result<Value, String> {
        let c = open(&self.path)?;
        let mut statement = c
            .prepare(
                "SELECT version, revoked_at_ms IS NOT NULL FROM flow_install
                  WHERE flow_id = ?1 ORDER BY version",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![flow_id], |r| {
                Ok(json!({ "version": r.get::<_, i64>(0)?, "revoked": r.get::<_, bool>(1)? }))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map(Value::Array)
            .map_err(|e| e.to_string())
    }
}

/// basal's store, read-only.
pub struct BasalStore {
    pub path: PathBuf,
}

/// The journal's kind codes for the primitives the suite checks
/// (`basal_proto::types::Primitive::code`).
pub const KIND_SINK_DIGEST: i64 = 6;
pub const KIND_SINK_STATUS: i64 = 7;

/// One journaled call of a run.
#[derive(Debug, Clone)]
pub struct Call {
    pub run_id: String,
    pub position: u64,
    pub kind_code: i64,
    pub dispatch: String,
    pub attempts: i64,
    pub settlement: Option<String>,
    pub value: Option<Value>,
    pub request: Option<Value>,
}

impl BasalStore {
    /// The card decision timestamp is written by basal, not inferred from core's
    /// answer. The flow's enable switch alone is not approval: an undecided or
    /// declined version can have an enabled switch and no approved version.
    pub fn install_decision(
        &self,
        flow_id: &str,
        version: i64,
        attempt: i64,
    ) -> Result<Option<Value>, String> {
        let c = open(&self.path)?;
        c.query_row(
            "SELECT c.state,c.decided_at,f.state,f.approved_version,i.approved_at
             FROM install_cards c JOIN flows f ON f.flow_id=c.flow_id
             JOIN installs i ON i.flow_id=c.flow_id AND i.version=c.version
             WHERE c.flow_id=?1 AND c.version=?2 AND c.attempt=?3",
            params![flow_id, version, attempt],
            |r| {
                Ok(
                    json!({"state":r.get::<_,String>(0)?,"decided_at_ms":r.get::<_,Option<i64>>(1)?,
                "flow_state":r.get::<_,String>(2)?,"approved_version":r.get::<_,Option<i64>>(3)?,
                "approved_at_ms":r.get::<_,Option<i64>>(4)?}),
                )
            },
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    /// Find the run basal recorded for the invoking call key. Reading this id
    /// avoids deriving the expected probe identity with basal's own algorithm.
    pub fn codemode_call(&self, call_key: &str) -> Result<Option<Value>, String> {
        let c = open(&self.path)?;
        c.query_row("SELECT r.run_id,r.agent_id,r.status FROM codemode_runs r JOIN codemode_tool_calls t ON t.run_id=r.run_id WHERE t.call_key=?1", [call_key], |r| Ok(json!({
            "run_id":r.get::<_, String>(0)?, "agent_id":r.get::<_, String>(1)?, "status":r.get::<_, String>(2)?
        }))).optional().map_err(|e| e.to_string())
    }

    /// Join generations on the flow's stored package and owner: the counter is
    /// per package/agent, not version. Keep removal separate from enabled state
    /// so removing an instance cannot be mistaken for disabling it.
    pub fn instance(&self, flow_id: &str) -> Result<Option<Value>, String> {
        let c = open(&self.path)?;
        c.query_row("SELECT f.owner,f.package,f.state,f.approved_version,f.removed,f.disabled_by,f.disabled_reason,f.disabled_at,g.generation,g.operation,g.reply
            FROM flows f JOIN instance_generations g ON g.package=f.package AND g.agent_id=f.owner WHERE f.flow_id=?1",[flow_id],|r|Ok(json!({
            "flow_id":flow_id,"agent_id":r.get::<_,String>(0)?,"package":r.get::<_,String>(1)?,"state":r.get::<_,String>(2)?,
            "version":r.get::<_,i64>(3)?,"removed":r.get::<_,bool>(4)?,"disabled_by":r.get::<_,Option<String>>(5)?,
            "disabled_reason":r.get::<_,Option<String>>(6)?,"disabled_at":r.get::<_,Option<i64>>(7)?,"generation":r.get::<_,i64>(8)?,
            "operation":r.get::<_,String>(9)?,"reply":r.get::<_,String>(10)?
        }))).optional().map_err(|e|e.to_string())
    }

    pub fn flow_enabled(&self, flow_id: &str) -> Result<Option<bool>, String> {
        let c = open(&self.path)?;
        c.query_row(
            "SELECT state = 'enabled' FROM flows WHERE flow_id = ?1",
            [flow_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())
    }
    /// The state of the install card for one flow version, if basal has one.
    pub fn card_state(&self, flow_id: &str, version: i64) -> Result<Option<String>, String> {
        let c = open(&self.path)?;
        c.query_row(
            "SELECT state FROM install_cards WHERE flow_id = ?1 AND version = ?2",
            params![flow_id, version],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    /// Every journaled call of every run of one flow, in order.
    pub fn calls(&self, flow_id: &str) -> Result<Vec<Call>, String> {
        let c = open(&self.path)?;
        let retained = retained_projection(&c)?;
        let mut statement = c
            .prepare(
                if retained { "SELECT j.run_id,j.position,j.kind_code,j.dispatch,j.attempts,j.settlement,j.value,j.request FROM rig_calls j JOIN rig_runs r USING(run_id) WHERE r.flow_id=?1 ORDER BY r.admit_seq,r.run_id,j.position" } else { "SELECT j.run_id, j.position, j.kind_code, j.dispatch, j.attempts,
                        j.settlement, CASE WHEN j.payload_pruned THEN '\"payload pruned\"' ELSE j.value END, CASE WHEN j.payload_pruned THEN '\"payload pruned\"' ELSE j.request END
                   FROM journal_with_payloads j JOIN runs r ON r.run_id = j.run_id
                  WHERE r.flow_id = ?1
                   ORDER BY r.rowid, j.position" },
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![flow_id], |r| {
                Ok((
                    (r.get::<_, String>(0)?, r.get::<_, i64>(1)?),
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut calls = Vec::new();
        for row in rows {
            let ((run_id, position), kind_code, dispatch, attempts, settlement, value, request) =
                row.map_err(|e| e.to_string())?;
            calls.push(Call {
                run_id,
                position: u64::try_from(position)
                    .map_err(|_| format!("negative journal position {position}"))?,
                kind_code,
                dispatch,
                attempts,
                settlement,
                value: value.map(|v| serde_json::from_str(&v).unwrap_or(Value::String(v))),
                request: request
                    .map(|v| serde_json::from_str(&v))
                    .transpose()
                    .map_err(|e| e.to_string())?,
            });
        }
        Ok(calls)
    }

    /// How many activations of a run basal recorded. A claim that basal's
    /// install gate turned back before the run reached a worker is not one.
    pub fn activations(&self, run_id: &str) -> Result<i64, String> {
        let c = open(&self.path)?;
        let retained = retained_projection(&c)?;
        c.query_row(
            if retained {
                "SELECT COUNT(*) FROM rig_activations WHERE run_id=?1"
            } else {
                "SELECT COUNT(*) FROM activations WHERE run_id = ?1"
            },
            params![run_id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())
    }

    /// The runs of one flow, oldest first: id, state, the script's return
    /// value and the error, if any.
    pub fn runs(&self, flow_id: &str) -> Result<Vec<Run>, String> {
        let c = open(&self.path)?;
        let retained = retained_projection(&c)?;
        let mut statement = c
            .prepare(
                if retained { "SELECT run_id,state,result,error_kind,error_detail FROM rig_runs WHERE flow_id=?1 ORDER BY admit_seq,run_id" } else { "SELECT run_id, state, result, error_kind, error_detail FROM runs
                  WHERE flow_id = ?1 ORDER BY rowid" },
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![flow_id], |r| {
                Ok(Run {
                    run_id: r.get(0)?,
                    state: r.get(1)?,
                    result: r
                        .get::<_, Option<String>>(2)?
                        .map(|v| serde_json::from_str(&v).unwrap_or(Value::String(v))),
                    error: match (
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ) {
                        (None, None) => None,
                        (kind, detail) => Some(json!({ "kind": kind, "detail": detail })),
                    },
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
}

/// Broca's independent run index, not basal's assertion about sends. These
/// reads never open a Broca route or initiate a model call. The index stores
/// the full bind triple as JSON; message.session uses the U+001F-joined triple.
pub struct BrocaStore {
    pub path: PathBuf,
}

impl BrocaStore {
    /// Count the real runner index before and after no-model rig cases. Keep
    /// historical runs in the count so any newly started run changes it.
    pub fn run_count(&self) -> Result<i64, String> {
        let c = open(&self.path)?;
        c.query_row("SELECT COUNT(*) FROM run_index", [], |r| r.get(0))
            .map_err(|e| e.to_string())
    }
}
/// One run of a flow, as basal recorded it.
#[derive(Debug, Clone)]
pub struct Run {
    pub run_id: String,
    pub state: String,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

impl Run {
    /// Whether the run has ended, one way or the other.
    pub fn ended(&self) -> bool {
        !matches!(self.state.as_str(), "pending" | "running" | "suspended")
    }
}
