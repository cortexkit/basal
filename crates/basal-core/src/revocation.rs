//! Verified push hints and the common dispatch fence for flow versions.

use basal_host::InstallStatus;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{CoreError, Result, Runtime, install};

/// Every effect path uses the run's pinned version, not today's approved pointer.
/// Reading under the store lock linearizes authorization with `install::revoke`.
pub(crate) fn dispatch_check(conn: &Connection, run_id: &str) -> Result<Option<String>> {
    let (flow, version): (String, Option<u32>) = conn.query_row(
        "SELECT flow_id,flow_version FROM runs WHERE run_id=?1",
        [run_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    match version {
        Some(version) => install::revocation(conn, &flow, version),
        None => Ok(None),
    }
}

fn approved_hash(conn: &Connection, flow: &str, version: u32) -> Result<Option<String>> {
    let hash: Option<Vec<u8>> = conn.query_row(
        "SELECT code_hash FROM installs WHERE flow_id=?1 AND version=?2 AND approval_ref IS NOT NULL \
         UNION ALL SELECT p.code_hash FROM flows f JOIN package_versions p ON p.package=f.package \
         WHERE f.flow_id=?1 AND p.version=?2 AND (f.approved_version=?2 OR EXISTS \
         (SELECT 1 FROM runs r WHERE r.flow_id=f.flow_id AND r.flow_version=?2)) LIMIT 1",
        params![flow, version],
        |r| r.get(0),
    ).optional()?;
    Ok(hash.map(|h| crate::ids::hex(&h)))
}

impl Runtime {
    /// A hint has no authority: compare one core reply with locally approved bytes.
    /// Unknown, never-approved and already-revoked versions need no status read.
    pub fn install_revoked(&self, flow: &str, version: u32, _hint_hash: &str) -> Result<()> {
        let hash = self.store().read(|c| {
            if install::revocation(c, flow, version)?.is_some() {
                return Ok(None);
            }
            approved_hash(c, flow, version)
        })?;
        let Some(hash) = hash else {
            return Ok(());
        };
        // The network read must not hold SQLite's writer: running calls need it
        // to journal their outcomes even when core is slow or unreachable.
        let status = self
            .shared
            .host
            .install_status(flow, version)
            .map_err(|e| {
                CoreError::Unavailable(format!("no install status for {flow} v{version}: {e}"))
            })?;
        let reason = match status {
            InstallStatus::Active { code_hash, .. } if code_hash == hash => return Ok(()),
            InstallStatus::Active { code_hash, .. } => format!(
                "core approved code hash {code_hash} for {flow} v{version}, but basal holds code hash {hash}"
            ),
            InstallStatus::Revoked { .. } => format!("core revoked {flow} v{version}"),
            InstallStatus::Unknown
            | InstallStatus::Pending { .. }
            | InstallStatus::Declined { .. }
            | InstallStatus::Expired { .. }
            | InstallStatus::Rejected { .. } => {
                format!("core holds no install of {flow} v{version}")
            }
        };
        self.store().write(|tx| {
            if approved_hash(tx, flow, version)?.as_deref() == Some(&hash) {
                install::revoke(tx, flow, version, &reason, self.config.clock.now_ms())?;
            }
            Ok(())
        })?;
        self.shared.signal.bump();
        self.release_unused_flow_routes_for(flow)?;
        Ok(())
    }
}
