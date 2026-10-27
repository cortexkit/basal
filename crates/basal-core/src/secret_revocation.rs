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
    let mut statement = tx.prepare("SELECT c.install_id,i.version,i.manifest FROM install_cards c JOIN installs i USING(flow_id,version) WHERE c.flow_id=?1 AND (?2 IS NULL OR c.version=?2) AND c.install_id IS NOT NULL")?;
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
        let manifest = crate::manifest::Manifest::parse_stored(&manifest)
            .map_err(|_| CoreError::Corrupt("secret revoke manifest does not decode".into()))?;
        if manifest.secrets.is_none() {
            continue;
        }
        let mut request = json!({"flow_install_id":id});
        if version.is_some() {
            request["flow_version"] = json!(installed_version);
        }
        let body = json!({"request":request,"retirement":retirement,"version":installed_version})
            .to_string();
        // A pending card can be approved after uninstall and mint fresh grants
        // under its retained install id. Deduplicate outstanding work only.
        tx.execute("INSERT INTO outbox(at,kind,flow_id,body,retry_at) SELECT ?1,?2,?3,?4,?1 WHERE NOT EXISTS (SELECT 1 FROM outbox WHERE kind=?2 AND flow_id=?3 AND body=?4 AND delivered_at IS NULL)", params![now, KIND, flow, body])?;
    }
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
    /// Send due revokes without a SQLite lock. Retired versions first wait for
    /// all admitted runs, durable obligations and executing dispatches to end.
    pub fn drain_secret_revocations(&self) -> Result<()> {
        let now = self.config().clock.now_ms();
        let rows = self.store().read(|connection| {
            Ok(connection.prepare("SELECT seq,flow_id,body,attempts FROM outbox WHERE kind=?1 AND delivered_at IS NULL AND retry_at<=?2 ORDER BY seq LIMIT 64")?
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
            let success = !busy
                && self
                    .shared
                    .host
                    .secret_revoke(body["request"].clone())
                    .is_ok_and(|reply| {
                        reply["status"] == "revoked" && reply["grants"].as_u64().is_some()
                    });
            self.store().write(|tx| {
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
                Ok(())
            })?;
        }
        Ok(())
    }

    pub(crate) fn secret_revocation_next_wake(&self) -> Result<Option<i64>> {
        self.store().read(|connection| {
            Ok(connection.query_row(
                "SELECT MIN(retry_at) FROM outbox WHERE kind=?1 AND delivered_at IS NULL",
                [KIND],
                |row| row.get(0),
            )?)
        })
    }
}
