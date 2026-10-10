//! Store lost grant references and disable evidence. Read existing permissions
//! on core-registered flow routes, and resume only the lost-grant pause when
//! permission returns; this module never grants provider access.
use basal_host::{InstallStatus, flow_refusal::FlowRefusal};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CoreError, Result, Runtime, install};

const RETRY_MS: i64 = 1_000;
const RETRY_MAX_MS: i64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retirement {
    pub agent_id: Option<String>,
    pub agent_reference: Option<String>,
    pub provider: String,
    pub action: String,
    pub at_ms: i64,
}

pub(crate) fn record_retirement(
    tx: &Transaction,
    request: &basal_host::CallRequest,
    refusal: &FlowRefusal,
    now: i64,
    catalog: &dyn basal_host::Catalog,
) -> Result<()> {
    let run_author: Option<String> = tx.query_row(
        "SELECT i.author FROM installs i JOIN runs r ON r.flow_id=i.flow_id AND r.flow_version=i.version WHERE r.run_id=?1",
        [&request.run_id], |r| r.get(0),
    ).optional()?;
    let flow = install::flow(tx, &request.flow_id)?;
    let owner = run_author
        .or_else(|| flow.and_then(|f| f.owner))
        .filter(|o| o != crate::decisions::OPERATOR_ACTOR && o != "local:unverified");
    let args: Value = serde_json::from_str(request.args.as_str())
        .map_err(|e| CoreError::Corrupt(e.to_string()))?;
    let target = args["agent"].as_str().map(str::to_owned);
    // An agent_retired refusal need not name the retired agent. Agent-owned
    // flows can target only their author; global sink/facts intents carry
    // their recipient in the agent argument.
    let agent_id = owner.or_else(|| {
        target
            .as_deref()
            .map(|t| catalog.agent_id(t).unwrap_or_else(|| t.to_owned()))
    });
    let retirement = Retirement {
        agent_id,
        agent_reference: target,
        provider: refusal.provider.clone(),
        action: refusal.action.clone(),
        at_ms: now,
    };
    tx.execute(
        "UPDATE flows SET agent_retirement=?2 WHERE flow_id=?1",
        params![
            request.flow_id,
            serde_json::to_string(&retirement).map_err(|e| CoreError::Invalid(e.to_string()))?
        ],
    )?;
    tx.execute("UPDATE flow_grant_losses SET state='cleared',revision=revision+1 WHERE flow_id=?1 AND state='polling'", [&request.flow_id])?;
    crate::decisions::close_stale_grants(tx, &request.flow_id, now)?;
    Ok(())
}

pub(crate) fn retirement_cleared_by(
    tx: &Transaction,
    flow_id: &str,
    author: &str,
    manifest: &str,
    catalog: Option<&dyn basal_host::Catalog>,
) -> Result<bool> {
    let body: Option<String> = tx.query_row(
        "SELECT agent_retirement FROM flows WHERE flow_id=?1",
        [flow_id],
        |r| r.get(0),
    )?;
    let Some(body) = body else { return Ok(false) };
    let retirement: Retirement =
        serde_json::from_str(&body).map_err(|e| CoreError::Corrupt(e.to_string()))?;
    let Some(retired) = retirement.agent_id.as_deref() else {
        return Ok(false);
    };
    let manifest: crate::Manifest =
        serde_json::from_str(manifest).map_err(|e| CoreError::Corrupt(e.to_string()))?;
    let agent_owned = author != crate::decisions::OPERATOR_ACTOR && author != "local:unverified";
    let different_owner = !agent_owned || author != retired;
    let no_target = manifest.agents().into_iter().all(|(_, target)| {
        if target == retired || retirement.agent_reference.as_deref() == Some(target) {
            return false;
        }
        if target == "$self" {
            return agent_owned && author != retired;
        }
        // A target alias missing from the catalog cannot prove that the new
        // manifest no longer involves the retired recipient.
        catalog
            .and_then(|c| c.agent_id(target))
            .is_some_and(|id| id != retired)
    });
    Ok(different_owner && no_target)
}

/// One stable subject for a future operator decision. Repeated losses update
/// this row and revision rather than creating a second subject for the grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantLoss {
    pub flow_id: String,
    pub provider: String,
    pub grant: String,
    pub grant_label: String,
    pub echoable: bool,
    pub run_id: String,
    pub version: u32,
    pub revision: i64,
    pub state: String,
    pub failures: u32,
    pub next_poll_at_ms: i64,
    pub lost_at_ms: i64,
}

/// Strings remain byte-for-byte opaque. Structured references use recursively
/// sorted object keys and compact JSON, independent of map insertion order.
pub fn grant_key(grant: &Value) -> Result<(String, bool)> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let ordered: std::collections::BTreeMap<_, _> = object
                    .iter()
                    .map(|(key, value)| (key.clone(), sorted(value)))
                    .collect();
                Value::Object(ordered.into_iter().collect())
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    let bytes = match grant {
        Value::String(text) => text.clone(),
        other => {
            serde_json::to_string(&sorted(other)).map_err(|e| CoreError::Invalid(e.to_string()))?
        }
    };
    if bytes.len() > 4096 {
        Ok((blake3::hash(bytes.as_bytes()).to_hex().to_string(), false))
    } else {
        Ok((bytes, true))
    }
}

pub(crate) fn record(
    tx: &Transaction,
    flow_id: &str,
    run_id: &str,
    refusal: &FlowRefusal,
    now: i64,
) -> Result<()> {
    let (grant, label) = refusal.lost_grant().map_err(CoreError::Invalid)?;
    let (key, echoable) = grant_key(grant)?;
    let version: u32 = tx.query_row(
        "SELECT flow_version FROM runs WHERE run_id=?1",
        [run_id],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO flow_grant_losses(flow_id,provider,grant_key,grant_label,echoable,run_id,version,state,next_poll_at,lost_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,'polling',?8,?8) \
         ON CONFLICT(flow_id,provider,grant_key) DO UPDATE SET grant_label=excluded.grant_label,echoable=excluded.echoable, \
         run_id=excluded.run_id,version=excluded.version,revision=revision+1,state='polling',failures=0,next_poll_at=excluded.next_poll_at,lost_at=excluded.lost_at",
        params![flow_id, refusal.provider, key, label, echoable, run_id, version, now],
    )?;
    install::disable(tx, flow_id, &install::Actor::Core, "grant_lost", now)
        .map_err(|e| CoreError::Invalid(e.to_string()))?;
    if echoable {
        let loss = losses(tx, Some(flow_id))?
            .into_iter()
            .find(|loss| loss.provider == refusal.provider && loss.grant == key)
            .expect("loss inserted in this transaction");
        crate::decisions::record_grant_loss(tx, &loss)?;
    }
    Ok(())
}

pub fn losses(conn: &Connection, flow_id: Option<&str>) -> Result<Vec<GrantLoss>> {
    let mut stmt = conn.prepare_cached("SELECT flow_id,provider,grant_key,grant_label,echoable,run_id,version,revision,state,failures,next_poll_at,lost_at \
        FROM flow_grant_losses WHERE (?1 IS NULL OR flow_id=?1) ORDER BY flow_id,provider,grant_key")?;
    Ok(stmt
        .query_map([flow_id], |r| {
            Ok(GrantLoss {
                flow_id: r.get(0)?,
                provider: r.get(1)?,
                grant: r.get(2)?,
                grant_label: r.get(3)?,
                echoable: r.get(4)?,
                run_id: r.get(5)?,
                version: r.get(6)?,
                revision: r.get(7)?,
                state: r.get(8)?,
                failures: r.get(9)?,
                next_poll_at_ms: r.get(10)?,
                lost_at_ms: r.get(11)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn clear(tx: &Transaction, flow_id: &str, now: i64) -> Result<()> {
    tx.execute("UPDATE flow_grant_losses SET state='cleared',revision=revision+1 WHERE flow_id=?1 AND state IN ('polling','stopped')", [flow_id])?;
    tx.execute("UPDATE journal SET retry_not_before=?2 WHERE dispatch='deferred' AND refusal IN ('module_grant_absent','agent_grant_absent') \
        AND run_id IN (SELECT run_id FROM runs WHERE flow_id=?1)", params![flow_id, now])?;
    crate::decisions::close_stale_grants(tx, flow_id, now)?;
    Ok(())
}

pub(crate) fn has_polling(conn: &Connection, flow_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM flow_grant_losses WHERE flow_id=?1 AND state='polling')",
        [flow_id],
        |r| r.get(0),
    )?)
}

pub(crate) fn stop_polling(
    tx: &Transaction,
    flow_id: &str,
    provider: &str,
    grant: &str,
) -> Result<bool> {
    Ok(tx.execute("UPDATE flow_grant_losses SET state='stopped',revision=revision+1 WHERE flow_id=?1 AND provider=?2 AND grant_key=?3 AND state='polling'", params![flow_id,provider,grant])? > 0)
}

pub(crate) fn check_now(
    tx: &Transaction,
    flow_id: &str,
    provider: &str,
    grant: &str,
    now: i64,
) -> Result<bool> {
    Ok(tx.execute("UPDATE flow_grant_losses SET next_poll_at=?4 WHERE flow_id=?1 AND provider=?2 AND grant_key=?3 AND state='polling' AND echoable=1", params![flow_id,provider,grant,now])? > 0)
}

pub(crate) fn eligible(conn: &Connection, loss: &GrantLoss) -> Result<bool> {
    let Some(flow) = install::flow(conn, &loss.flow_id)? else {
        return Ok(false);
    };
    let retired: bool = conn.query_row(
        "SELECT agent_retirement IS NOT NULL FROM flows WHERE flow_id=?1",
        [&loss.flow_id],
        |r| r.get(0),
    )?;
    Ok(flow.approved_version == Some(loss.version)
        && !retired
        && !crate::packages::removed(conn, &loss.flow_id)?
        && install::revocation(conn, &loss.flow_id, loss.version)?.is_none())
}

impl Runtime {
    /// Read all stored grant losses, including their latest identity, revision
    /// and restoration state, without performing a permission check.
    pub fn grant_losses(&self) -> Result<Vec<GrantLoss>> {
        self.store().read(|conn| losses(conn, None))
    }

    /// Stop restoration without granting anything or enabling the flow.
    pub fn stop_grant_polling(&self, flow_id: &str, provider: &str, grant: &str) -> Result<bool> {
        let changed = self.store().write(|tx| {
            let changed = stop_polling(tx, flow_id, provider, grant)?;
            crate::decisions::close_stale_grants(tx, flow_id, self.config.clock.now_ms())?;
            Ok(changed)
        })?;
        self.shared.signal.bump();
        Ok(changed)
    }

    /// Schedule a read now; a stopped subject stays stopped until explicit enable.
    pub fn check_grant_now(&self, flow_id: &str, provider: &str, grant: &str) -> Result<bool> {
        let changed = self
            .store()
            .write(|tx| check_now(tx, flow_id, provider, grant, self.config.clock.now_ms()))?;
        self.shared.signal.bump();
        Ok(changed)
    }

    /// One deterministic maintenance pass. Claim each read's next timer before
    /// leaving the store, so concurrent passes cannot hammer the provider.
    pub fn poll_lost_grants(&self) -> Result<usize> {
        let now = self.config.clock.now_ms();
        let due = self.store().write(|tx| {
            let mut due = Vec::new();
            for loss in losses(tx, None)? {
                if loss.state != "polling" { continue; }
                if !eligible(tx, &loss)? {
                    tx.execute("UPDATE flow_grant_losses SET state='cleared',revision=revision+1 WHERE flow_id=?1 AND provider=?2 AND grant_key=?3", params![loss.flow_id,loss.provider,loss.grant])?;
                    crate::decisions::close_stale_grants(tx, &loss.flow_id, now)?;
                    continue;
                }
                if !loss.echoable || loss.next_poll_at_ms > now { continue; }
                let delay = RETRY_MS.saturating_mul(1i64 << loss.failures.min(16)).min(RETRY_MAX_MS);
                tx.execute("UPDATE flow_grant_losses SET failures=MIN(failures+1,4294967295),next_poll_at=?4 WHERE flow_id=?1 AND provider=?2 AND grant_key=?3", params![loss.flow_id,loss.provider,loss.grant,now.saturating_add(delay)])?;
                due.push(loss);
            }
            Ok(due)
        })?;
        for loss in &due {
            let restored = self.probe_grant(loss)?;
            if !restored {
                continue;
            }
            self.store().write(|tx| {
                if !eligible(tx, loss)? { return Ok(()); }
                let changed = tx.execute("UPDATE flow_grant_losses SET state='restored' WHERE flow_id=?1 AND provider=?2 AND grant_key=?3 AND revision=?4 AND state='polling'", params![loss.flow_id,loss.provider,loss.grant,loss.revision])?;
                if changed == 0 { return Ok(()); }
                let remaining: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM flow_grant_losses WHERE flow_id=?1 AND state IN ('polling','stopped'))", [&loss.flow_id], |r| r.get(0))?;
                if !remaining && install::flow(tx, &loss.flow_id)?.is_some_and(|f| f.disabled_by.as_deref()==Some(install::CORE_ACTOR) && f.disabled_reason.as_deref()==Some("grant_lost")) {
                    install::enable(tx, &loss.flow_id, now).map_err(|e| CoreError::Invalid(e.to_string()))?;
                }
                crate::decisions::close_stale_grants(tx, &loss.flow_id, now)?;
                Ok(())
            })?;
            self.shared.signal.bump();
        }
        Ok(due.len())
    }

    fn probe_grant(&self, loss: &GrantLoss) -> Result<bool> {
        let Some(approved) = self.store().read(|c| install::approved(c, &loss.flow_id))? else {
            return Ok(false);
        };
        let expected = crate::ids::hex(&approved.code_hash);
        match self.shared.host.install_status(&loss.flow_id, loss.version) {
            Ok(InstallStatus::Active {
                code_hash,
                scope: Some(scope),
            }) if code_hash == expected => {
                let flow = self.store().read(|c| install::flow(c, &loss.flow_id))?;
                let agent = flow
                    .as_ref()
                    .and_then(|f| f.owner.as_deref())
                    .filter(|owner| {
                        *owner != crate::decisions::OPERATOR_ACTOR && *owner != "local:unverified"
                    });
                self.shared
                    .host
                    .configure_flow(&loss.flow_id, agent.is_some(), Some(scope));
                Ok(self
                    .shared
                    .host
                    .grant_would_ask(&loss.flow_id, agent, &loss.provider, &loss.grant)
                    .unwrap_or(false))
            }
            Ok(InstallStatus::Active { code_hash, .. }) if code_hash == expected => Ok(false),
            Ok(_) => {
                self.store().write(|tx| {
                    if !eligible(tx, loss)? { return Ok(()); }
                    install::revoke(tx, &loss.flow_id, loss.version, "grant restoration found install revoked or changed", self.config.clock.now_ms())?;
                    tx.execute("UPDATE flow_grant_losses SET state='cleared',revision=revision+1 WHERE flow_id=?1 AND version=?2 AND state='polling'", params![loss.flow_id,loss.version])?;
                    crate::decisions::close_stale_grants(tx, &loss.flow_id, self.config.clock.now_ms())?;
                    Ok(())
                })?;
                Ok(false)
            }
            Err(_) => Ok(false),
        }
    }
}
