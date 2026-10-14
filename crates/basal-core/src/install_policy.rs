//! One-shot confirmation of core's install decisions and crash recovery.

use basal_host::{InstallApproval, InstallStatus};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::{CoreError, Result, Runtime, install};

pub fn approval(conn: &Connection, flow_id: &str, version: Option<u32>) -> Result<Option<Value>> {
    let Some(version) = version else {
        return Ok(None);
    };
    let (kind, rule): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT approval_kind, approval_rule FROM installs WHERE flow_id=?1 AND version=?2",
            params![flow_id, version],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or_default();
    Ok(Some(match kind.as_deref() {
        Some("policy") => json!({"kind":"policy", "rule":rule}),
        Some("card") => json!({"kind":"card"}),
        _ => json!({"kind":"unknown"}),
    }))
}

pub fn check_errors(conn: &Connection, flow_id: &str) -> Result<Vec<Value>> {
    Ok(conn.prepare("SELECT version, install_check_error FROM installs WHERE flow_id=?1 AND install_check_error IS NOT NULL ORDER BY version")?
        .query_map([flow_id], |r| Ok(json!({"version":r.get::<_, u32>(0)?, "error":r.get::<_, String>(1)?})))?
        .collect::<rusqlite::Result<_>>()?)
}

impl Runtime {
    /// A locally saved install request does not prove core displayed a consent
    /// card. Record that ownership only after core replies `card_raised`.
    pub fn confirm_install_card(&self, flow_id: &str, version: u32, attempt: i64) -> Result<()> {
        self.store().write(|tx| {
            tx.execute("UPDATE install_cards SET attempt=?3 WHERE flow_id=?1 AND version=?2 AND state='pending'", params![flow_id, version, attempt])?;
            tx.execute("UPDATE installs SET install_check_error=NULL WHERE flow_id=?1 AND version=?2", params![flow_id, version])?;
            Ok(())
        })
    }

    pub fn record_install_check_error(
        &self,
        flow_id: &str,
        version: u32,
        error: &str,
    ) -> Result<()> {
        tracing::warn!(target: "consent", flow_id, version, error, "install approval remains pending");
        self.store().write(|tx| {
            tx.execute("UPDATE installs SET install_check_error=?3 WHERE flow_id=?1 AND version=?2 AND state='validated'", params![flow_id, version, error])?;
            Ok(())
        })
    }

    /// A policy reply alone does not approve a version: first compare its hash
    /// to the installed version, then ask core once to confirm durable approval.
    pub fn confirm_policy_install(
        &self,
        flow_id: &str,
        version: u32,
        hash: &[u8; 32],
        reply_hash: &str,
        rule: &str,
    ) -> Result<bool> {
        if reply_hash != crate::ids::hex(hash) {
            self.record_install_check_error(
                flow_id,
                version,
                "policy reply code hash does not match the installed version",
            )?;
            return Ok(false);
        }
        self.confirm_active_install(
            flow_id,
            version,
            hash,
            Some(InstallApproval::Policy { rule: rule.into() }),
        )
    }

    fn confirm_active_install(
        &self,
        flow_id: &str,
        version: u32,
        hash: &[u8; 32],
        provenance: Option<InstallApproval>,
    ) -> Result<bool> {
        let expected = crate::ids::hex(hash);
        let approval = match self
            .shared
            .host
            .install_status_with_approval(flow_id, version)
        {
            Ok((InstallStatus::Active { code_hash, .. }, approval)) if code_hash == expected => {
                provenance.or(approval)
            }
            other => {
                self.record_install_check_error(
                    flow_id,
                    version,
                    &format!(
                        "install_status did not confirm an active matching version: {other:?}"
                    ),
                )?;
                return Ok(false);
            }
        };
        self.approve_confirmed_install(flow_id, version, hash, approval)
    }

    fn approve_confirmed_install(
        &self,
        flow_id: &str,
        version: u32,
        hash: &[u8; 32],
        approval: Option<InstallApproval>,
    ) -> Result<bool> {
        if approval.is_none() {
            tracing::warn!(target: "consent", flow_id, version, "core confirmed the install without approval provenance");
        }
        let now = self.config().clock.now_ms();
        let schedule = &self.config().schedule;
        let result = self.store().write(|tx| {
            let eligible: bool = tx.query_row("SELECT state='validated' AND revoked_at IS NULL AND NOT EXISTS (SELECT 1 FROM install_cards c WHERE c.flow_id=i.flow_id AND c.version=i.version AND c.state<>'pending') FROM installs i WHERE flow_id=?1 AND version=?2", params![flow_id, version], |r| r.get(0))?;
            if !eligible {
                return Ok(());
            }
            if let Some(InstallApproval::Policy { rule }) = &approval {
                let manifest: String = tx.query_row("SELECT manifest FROM installs WHERE flow_id=?1 AND version=?2", params![flow_id, version], |r| r.get(0))?;
                let manifest = crate::manifest::Manifest::parse(&manifest)
                    .map_err(|e| CoreError::Corrupt(format!("installed manifest: {e}")))?;
                // Global reach always needs a card answer, even if core reports
                // a matching active version approved by a configurable policy.
                if matches!(manifest.audience, Some(crate::manifest::Audience::Global {})) {
                    tracing::error!(target: "consent", flow_id, version, rule, "refusing policy approval: global audience requires a card approval");
                    return Err(CoreError::Invalid(format!("global audience requires a card approval; policy rule {rule} cannot approve flow {flow_id} version {version}")));
                }
            }
            let reference = match &approval {
                Some(InstallApproval::Policy { rule }) => format!("policy:{rule}"),
                Some(InstallApproval::Card) => "core:card".into(),
                None => "core:unknown".into(),
            };
            // Approving the exact bytes also transfers ownership, initializes
            // schedules and clears a core-imposed disable. Commit those changes
            // with provenance and card settlement, so a crash cannot split them.
            install::approve(tx, flow_id, version, hash, &reference, now, schedule).map_err(|e| match e {
                install::InstallError::Store(e) => e,
                other => CoreError::Invalid(other.to_string()),
            })?;
            let (kind, rule) = match &approval {
                Some(InstallApproval::Policy { rule }) => ("policy", Some(rule.as_str())),
                Some(InstallApproval::Card) => ("card", None),
                None => ("unknown", None),
            };
            tx.execute("UPDATE installs SET approval_kind=?3, approval_rule=?4, install_check_error=NULL WHERE flow_id=?1 AND version=?2", params![flow_id, version, kind, rule])?;
            tx.execute("UPDATE install_cards SET state='approved', decided_by=?3, decided_at=?4 WHERE flow_id=?1 AND version=?2 AND state='pending'", params![flow_id, version, reference, now])?;
            tx.execute("INSERT INTO outbox(at,kind,flow_id,recipient,body) SELECT ?3,'flow.install_approved',flow_id,author,?4 FROM installs WHERE flow_id=?1 AND version=?2", params![flow_id, version, now, json!({"flow_id":flow_id,"version":version,"decision":"approved","approval":{"kind":kind,"rule":rule}}).to_string()])?;
            Ok(())
        });
        match result {
            Err(CoreError::Invalid(error)) => {
                self.record_install_check_error(flow_id, version, &error)?;
                Ok(false)
            }
            Err(error) => Err(error),
            Ok(()) => {
                self.shared.signal.bump();
                Ok(true)
            }
        }
    }

    /// A hint carries identity only. Unknown and already settled cards are
    /// acknowledged without querying core; every open card gets one status read.
    pub fn install_decided(&self, flow_id: &str, version: u32, attempt: i64) -> Result<()> {
        let card = self.store().read(|c| {
            let id: Option<String> = c.query_row("SELECT card_id FROM install_cards WHERE flow_id=?1 AND version=?2 AND attempt=?3 AND state='pending'", params![flow_id, version, attempt], |r| r.get(0)).optional()?;
            id.map(|id| crate::cards::load(c, &id)).transpose().map(Option::flatten)
        })?;
        if let Some(card) = card {
            self.reconcile_install_card(&card)?;
        }
        Ok(())
    }

    fn reconcile_install_card(&self, card: &crate::cards::CardRecord) -> Result<()> {
        let flow_id = &card.flow_id;
        let version = card.version;
        match self
            .shared
            .host
            .install_status_with_approval(flow_id, version)
        {
            Ok((InstallStatus::Active { code_hash, .. }, approval))
                if code_hash == crate::ids::hex(&card.code_hash) =>
            {
                self.approve_confirmed_install(flow_id, version, &card.code_hash, approval)?;
            }
            Ok((InstallStatus::Declined { .. }, _)) => {
                self.settle_install_card(card, "declined")?
            }
            Ok((InstallStatus::Expired { .. }, _)) => self.settle_install_card(card, "expired")?,
            Ok((InstallStatus::Pending { .. }, _)) => {}
            Ok((InstallStatus::Unknown, _)) => {
                tracing::warn!(target: "consent", flow_id, version, attempt = ?card.attempt, "core holds no install record for an open card");
            }
            other => self.record_install_check_error(
                flow_id,
                version,
                &format!("install_status did not confirm an active matching version: {other:?}"),
            )?,
        }
        Ok(())
    }

    fn settle_install_card(&self, card: &crate::cards::CardRecord, state: &str) -> Result<()> {
        let now = self.config().clock.now_ms();
        self.store().write(|tx| {
            let changed = tx.execute("UPDATE install_cards SET state=?2, decided_by='core:card', decided_at=?3 WHERE card_id=?1 AND state='pending' AND attempt IS ?4", params![card.card_id, state, now, card.attempt])?;
            if changed == 0 { return Ok(()); }
            tx.execute("UPDATE installs SET install_check_error=NULL WHERE flow_id=?1 AND version=?2", params![card.flow_id, card.version])?;
            tx.execute("INSERT INTO outbox(at,kind,flow_id,recipient,body) VALUES (?1,?2,?3,?4,?5)", params![now, format!("flow.install_{state}"), card.flow_id, card.author, json!({"flow_id":card.flow_id,"version":card.version,"attempt":card.attempt,"decision":state}).to_string()])?;
            Ok(())
        })?;
        self.shared.signal.bump();
        Ok(())
    }

    /// At startup, check each undecided card and each pending policy request
    /// once. Lost hints and lost request replies are recovered without polling.
    pub fn recover_cardless_installs(&self) -> Result<()> {
        let pending = self.store().read(|c| {
            let mut stmt = c.prepare("SELECT i.flow_id,i.version,i.code_hash FROM installs i WHERE i.state='validated' AND i.revoked_at IS NULL AND NOT EXISTS (SELECT 1 FROM install_cards c WHERE c.flow_id=i.flow_id AND c.version=i.version AND c.state<>'pending') ORDER BY i.flow_id,i.version")?;
            Ok(stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?, r.get::<_, Vec<u8>>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for (flow_id, version, hash) in pending {
            let card = self
                .cards(&flow_id)?
                .into_iter()
                .find(|c| c.version == version);
            if let Some(card) = card {
                self.reconcile_install_card(&card)?;
            } else {
                self.confirm_active_install(&flow_id, version, &crate::model::digest(hash)?, None)?;
            }
        }
        Ok(())
    }
}
