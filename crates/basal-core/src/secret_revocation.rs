//! Durable vault cleanup. A failed remote revoke never discards its obligation.
use crate::{CoreError, Result, Runtime};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};

const KIND: &str = "secret.revoke_flow_grants";

pub(crate) fn enqueue(
    tx: &Transaction<'_>,
    flow: &str,
    version: Option<u32>,
    retirement: bool,
    now: i64,
) -> Result<()> {
    let mut statement = tx.prepare("SELECT c.install_id,i.version,i.manifest FROM install_cards c JOIN installs i USING(flow_id,version) WHERE c.flow_id=?1 AND (?2 IS NULL OR c.version=?2) AND c.install_id IS NOT NULL AND (instr(i.manifest,'secrets')>0 OR instr(i.manifest,char(92))>0)")?;
    let rows = statement
        .query_map(params![flow, version], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, installed_version, manifest) in rows {
        if !crate::stored_secret::has_secrets(&manifest, flow, installed_version) {
            continue;
        }
        enqueue_install(
            tx,
            flow,
            &id,
            installed_version,
            version.is_some(),
            retirement,
            now,
        )?;
    }
    Ok(())
}

fn enqueue_install(
    tx: &Transaction<'_>,
    flow: &str,
    id: &str,
    version: u32,
    version_specific: bool,
    retirement: bool,
    now: i64,
) -> Result<()> {
    let mut request = json!({"flow_install_id":id});
    if version_specific {
        request["flow_version"] = json!(version);
    }
    let body = json!({"request":request,"retirement":retirement,"version":version}).to_string();
    // A pending card can be approved after uninstall and mint fresh grants
    // under its retained install id. Deduplicate outstanding work only.
    tx.execute("INSERT INTO outbox(at,kind,flow_id,body,retry_at) SELECT ?1,?2,?3,?4,?1 WHERE NOT EXISTS (SELECT 1 FROM outbox WHERE kind=?2 AND flow_id=?3 AND body=?4 AND delivered_at IS NULL AND failed_at IS NULL)", params![now, KIND, flow, body])?;
    Ok(())
}

pub(crate) fn retire_approved(tx: &Transaction<'_>, flow: &str, now: i64) -> Result<()> {
    let versions = tx
        .prepare("SELECT version FROM installs WHERE flow_id=?1 AND state='approved'")?
        .query_map([flow], |row| row.get::<_, u32>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for version in versions {
        enqueue(tx, flow, Some(version), true, now)?;
    }
    Ok(())
}

impl Runtime {
    /// Retire an unconfirmed identity only after core reports no install/card.
    /// The status probe holds no database lock; the write then atomically
    /// rechecks the card's eligibility.
    pub fn abandon_prepared_secret_install(
        &self,
        flow: &str,
        version: u32,
        install_id: &str,
    ) -> Result<bool> {
        let eligible = self.store().read(|connection| Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM install_cards WHERE flow_id=?1 AND version=?2 AND state='pending' AND attempt IS NULL AND install_id=?3)",
            params![flow,version,install_id], |row| row.get::<_, bool>(0))?))?;
        if !eligible
            || !self
                .shared
                .host
                .install_status(flow, version)
                .is_ok_and(|status| matches!(status, basal_host::InstallStatus::Unknown))
        {
            return Ok(false);
        }
        let changed = self.store().write(|tx| {
            let changed = tx.execute("UPDATE install_cards SET install_id=NULL,attempt=NULL WHERE flow_id=?1 AND version=?2 AND state='pending' AND attempt IS NULL AND install_id=?3", params![flow,version,install_id])?;
            if changed != 1 { return Ok(false); }
            enqueue_install(tx, flow, install_id, version, true, false, self.config().clock.now_ms())?;
            Ok(true)
        })?;
        if changed {
            self.shared.signal.bump();
        }
        Ok(changed)
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn enqueue_secret_install_cleanup(&self, flow: &str, version: u32) -> Result<()> {
        self.store()
            .write(|tx| enqueue(tx, flow, Some(version), false, self.config().clock.now_ms()))?;
        self.shared.signal.bump();
        Ok(())
    }
    /// Send due revokes without a SQLite lock. Retired versions first wait for
    /// all admitted runs, durable obligations and executing dispatches to end.
    pub fn drain_secret_revocations(&self) -> Result<()> {
        let now = self.config().clock.now_ms();
        let rows = self.store().read(|connection| {
            Ok(connection.prepare("SELECT seq,flow_id,body,attempts FROM outbox WHERE kind=?1 AND delivered_at IS NULL AND failed_at IS NULL AND retry_at<=?2 ORDER BY seq LIMIT 64")?
                .query_map(params![KIND, now], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, u32>(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for (seq, flow, body, attempts) in rows {
            let body: Value = serde_json::from_str(&body).map_err(|_| {
                CoreError::Corrupt("secret revoke outbox body does not decode".into())
            })?;
            let version = body["version"]
                .as_u64()
                .ok_or_else(|| CoreError::Corrupt("secret revoke version missing".into()))?;
            let mut busy = false;
            if body["retirement"] == true {
                busy = self.store().read(|connection| {
                    Ok(connection.query_row(&format!("SELECT EXISTS(SELECT 1 FROM runs r WHERE r.flow_id=?1 AND r.flow_version=?2 AND (r.state IN ('pending','running','suspended','needs_reconcile') OR ({})))", crate::retention::OBLIGATIONS), params![flow, version], |row| row.get::<_, bool>(0))?)
                })?;
                busy |= self
                    .shared
                    .inflight
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .values()
                    .any(|route| {
                        route.flow_id == flow && route.version.map(u64::from) == Some(version)
                    });
            }
            let result = (!busy).then(|| self.shared.host.secret_revoke(body["request"].clone()));
            let success = result.as_ref().is_some_and(|result| {
                result.as_ref().is_ok_and(|reply| {
                    reply["status"] == "revoked" && reply["grants"].as_u64().is_some()
                })
            });
            let permanent = match &result {
                Some(Err(
                    basal_host::transport::WireError::Refused { code, .. }
                    | basal_host::transport::WireError::RefusedDetails { code, .. },
                )) if code != "store_error" => Some(code.clone()),
                Some(Err(basal_host::transport::WireError::Typed(refusal)))
                    if refusal.readiness()
                        || matches!(
                            refusal.reason,
                            basal_host::flow_refusal::RefusalReason::ResourceBusy
                                | basal_host::flow_refusal::RefusalReason::SecretVaultUnavailable
                        )
                        || refusal.reason.as_str().starts_with("scope_") =>
                {
                    None
                }
                Some(Err(basal_host::transport::WireError::Typed(refusal))) => {
                    Some(refusal.reason.as_str().to_owned())
                }
                _ => None,
            };
            let warned = self.store().write(|tx| {
                if let Some(code) = &permanent {
                    let changed = tx.execute("UPDATE outbox SET failed_at=?2,failure_code=?3 WHERE seq=?1 AND delivered_at IS NULL AND failed_at IS NULL", params![seq, now, code])?;
                    return Ok(changed != 0);
                }
                if success {
                    tx.execute(
                        "UPDATE outbox SET delivered_at=?2 WHERE seq=?1",
                        params![seq, now],
                    )?;
                } else {
                    let delay = if busy {
                        1000
                    } else {
                        1000_i64 * (1_i64 << attempts.min(8))
                    };
                    tx.execute(
                        "UPDATE outbox SET attempts=attempts+1,retry_at=?2 WHERE seq=?1",
                        params![seq, now.saturating_add(delay)],
                    )?;
                }
                Ok(false)
            })?;
            if warned {
                tracing::warn!(target:"consent", seq, "vault permanently refused secret cleanup; obligation marked failed");
            }
        }
        Ok(())
    }

    pub(crate) fn secret_revocation_next_wake(&self) -> Result<Option<i64>> {
        self.store().read(|connection| {
            Ok(connection.query_row(
                "SELECT MIN(retry_at) FROM outbox WHERE kind=?1 AND delivered_at IS NULL AND failed_at IS NULL",
                [KIND],
                |row| row.get(0),
            )?)
        })
    }
}
