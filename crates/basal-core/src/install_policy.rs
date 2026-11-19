//! Confirmation of core's install decisions and durable pending-card rechecks.

use basal_host::{InstallApproval, InstallStatus};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::cards::CardRecord;
use crate::{CoreError, Result, Runtime, install};

const FIRST_CHECK_RETRY_MS: i64 = 30_000;
const MAX_CHECK_RETRY_MS: i64 = 15 * 60_000;
const UNKNOWN_CARD_EXPIRY_MS: i64 = 24 * 60 * 60_000;

struct CardCheck {
    card: CardRecord,
    created_at: i64,
    checked_at: i64,
    failures: u32,
}

pub(crate) fn next_card_check_at(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT MIN(retry_at) FROM install_cards WHERE state='pending'",
        [],
        |r| r.get(0),
    )?)
}

pub(crate) fn mark_install_cards_due(tx: &Transaction<'_>, flow: &str, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE install_cards SET retry_at=?2 WHERE flow_id=?1 AND state='pending' AND retry_at IS NOT ?2",
        params![flow, now],
    )?;
    Ok(())
}

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
        let approval = match self.core_install_status_with_approval(flow_id, version) {
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

    pub(crate) fn approve_confirmed_install(
        &self,
        flow_id: &str,
        version: u32,
        hash: &[u8; 32],
        approval: Option<InstallApproval>,
    ) -> Result<bool> {
        let now = self.config().clock.now_ms();
        let schedule = &self.config().schedule;
        let mut superseded = false;
        let result = self.store().write(|tx| {
            let eligible: bool = tx.query_row("SELECT state='validated' AND revoked_at IS NULL AND NOT EXISTS (SELECT 1 FROM install_cards c WHERE c.flow_id=i.flow_id AND c.version=i.version AND c.state<>'pending') FROM installs i WHERE flow_id=?1 AND version=?2", params![flow_id, version], |r| r.get(0))?;
            if !eligible {
                return Ok(());
            }
            let manifest: String = tx.query_row("SELECT manifest FROM installs WHERE flow_id=?1 AND version=?2", params![flow_id, version], |r| r.get(0))?;
            if crate::stored_secret::has_secrets(&manifest, flow_id, version) && !matches!(approval, Some(InstallApproval::Card)) {
                let reason = "secrets require explicit card approval provenance; policy or missing provenance cannot approve this version";
                let previous: Option<String> = tx.query_row("SELECT install_check_error FROM installs WHERE flow_id=?1 AND version=?2", params![flow_id, version], |r| r.get(0))?;
                if previous.as_deref() != Some(reason) {
                    tracing::warn!(target:"consent", flow_id, version, "refusing secret install without explicit card approval provenance; version remains pending");
                }
                return Err(CoreError::Invalid(reason.into()));
            }
            if approval.is_none() {
                tracing::warn!(target: "consent", flow_id, version, "core confirmed the install without approval provenance");
            }
            if let Some(InstallApproval::Policy { rule }) = &approval {
                // Global reach always needs a card answer, even if core reports
                // a matching active version approved by a configurable policy.
                if crate::stored_secret::global_audience(&manifest) {
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
            match install::approve(tx, flow_id, version, hash, &reference, now, schedule) {
                Ok(_) => {}
                Err(install::InstallError::StaleVersion { approved, .. }) => {
                    if approved > version {
                        install::supersede_older_pending_cards(tx, flow_id, approved, now)?;
                    } else {
                        install::supersede_pending_card(tx, flow_id, version, now)?;
                    }
                    superseded = true;
                    return Ok(());
                }
                Err(install::InstallError::Store(error)) => return Err(error),
                Err(other) => return Err(CoreError::Invalid(other.to_string())),
            }
            let (kind, rule) = match &approval {
                Some(InstallApproval::Policy { rule }) => ("policy", Some(rule.as_str())),
                Some(InstallApproval::Card) => ("card", None),
                None => ("unknown", None),
            };
            tx.execute("UPDATE installs SET approval_kind=?3, approval_rule=?4, install_check_error=NULL WHERE flow_id=?1 AND version=?2", params![flow_id, version, kind, rule])?;
            tx.execute("UPDATE install_cards SET state='approved', decided_by=?3, decided_at=?4, retry_at=NULL, retry_failures=NULL WHERE flow_id=?1 AND version=?2 AND state='pending'", params![flow_id, version, reference, now])?;
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
                self.release_unused_flow_routes_for(flow_id)?;
                Ok(!superseded)
            }
        }
    }

    /// Make an install hint durable without reading core. With no pending card,
    /// the activation gate and flow.install_revoked still handle revocation.
    pub fn install_changed(&self, flow_id: &str) -> Result<()> {
        self.admit_events_with_install_changes(&[], &[flow_id])?;
        Ok(())
    }

    /// Commit install hints in the same transaction as admission, before the
    /// event batch is acknowledged. The caller deduplicates flow ids.
    pub fn admit_events_with_install_changes(
        &self,
        notices: &[crate::events::Notice],
        install_changes: &[&str],
    ) -> Result<crate::events::Report> {
        if notices.is_empty() {
            let any = self.store().read(|c| {
                let mut pending = c.prepare("SELECT EXISTS(SELECT 1 FROM install_cards WHERE flow_id=?1 AND state='pending')")?;
                for flow in install_changes {
                    if pending.query_row([flow], |r| r.get::<_, bool>(0))? {
                        return Ok(true);
                    }
                }
                Ok(false)
            })?;
            if !any {
                return Ok(crate::events::Report::default());
            }
        }
        self.staged_admit_event_batch(notices, install_changes)
    }

    /// Read at most eight due cards per flow on this engine pass. An answer for
    /// the newest pending version can supersede every older card. After that
    /// version gets first look, the least recently checked cards get the slots.
    pub fn check_due_install_cards(&self) -> Result<()> {
        let now = self.config().clock.now_ms();
        let due = self.store().read(|c| {
            Ok(c.prepare(
                "WITH pending AS (
                    SELECT card_id, flow_id, version, retry_at, checked_at,
                           MAX(version) OVER (PARTITION BY flow_id) AS newest
                    FROM install_cards WHERE state='pending'
                ), due AS (
                    SELECT card_id, flow_id, ROW_NUMBER() OVER (
                        PARTITION BY flow_id ORDER BY (version=newest) DESC, checked_at, version
                    ) AS priority
                    FROM pending WHERE retry_at<=?1
                )
                SELECT card_id FROM due WHERE priority<=8 ORDER BY priority, flow_id",
            )?
            .query_map([now], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for id in due {
            self.reconcile_install_card(&id, true, "engine")?;
        }
        Ok(())
    }

    fn start_card_check(&self, id: &str, due_only: bool) -> Result<Option<CardCheck>> {
        let now = self.config().clock.now_ms();
        self.store().write(|tx| {
            // Consume the wake before reading core. A notice committed during
            // the read can then set a fresh wake that its result won't erase.
            // Startup rechecks pending cards with no wake, including a read
            // interrupted by a process exit.
            let claimed = tx
                .query_row(
                    "UPDATE install_cards SET retry_at=NULL, checked_at=?2
                     WHERE card_id=?1 AND state='pending'
                       AND (retry_at<=?2 OR (NOT ?3 AND retry_at IS NULL))
                     RETURNING created_at, COALESCE(retry_failures,0)",
                    params![id, now, due_only],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((created_at, failures)) = claimed else {
                return Ok(None);
            };
            let card = crate::cards::load(tx, id)?
                .ok_or_else(|| CoreError::Store(format!("claimed install card {id} vanished")))?;
            Ok(Some(CardCheck {
                card,
                created_at,
                checked_at: now,
                failures,
            }))
        })
    }

    fn reconcile_install_card(&self, id: &str, due_only: bool, signal: &str) -> Result<()> {
        let Some(check) = self.start_card_check(id, due_only)? else {
            return Ok(());
        };
        let card = &check.card;
        let flow_id = &card.flow_id;
        let version = card.version;
        let resolved = |outcome: &str| {
            tracing::info!(target: "consent", flow_id, version, attempt = ?card.attempt, signal, outcome, "install card resolved");
        };
        match self.core_install_status_with_approval(flow_id, version) {
            Ok((
                InstallStatus::Active { code_hash, .. } | InstallStatus::Held { code_hash, .. },
                approval,
            )) if code_hash == crate::ids::hex(&card.code_hash) => {
                if self.approve_confirmed_install(flow_id, version, &card.code_hash, approval)? {
                    resolved("approved");
                }
            }
            Ok((InstallStatus::Declined { .. }, _)) => {
                if self.settle_install_card(card, "declined", None, None)? {
                    resolved("declined");
                }
            }
            Ok((InstallStatus::Revoked { .. }, _)) => {
                if self.settle_install_card(card, "declined", Some("revoked".into()), None)? {
                    resolved("revoked");
                }
            }
            Ok((InstallStatus::Expired { .. }, _)) => {
                if self.settle_install_card(card, "expired", None, None)? {
                    resolved("expired");
                }
            }
            Ok((
                InstallStatus::Rejected {
                    rejected_reason,
                    disposition,
                    ..
                },
                _,
            )) => {
                if self.settle_install_card(card, "rejected", rejected_reason, disposition)? {
                    resolved("rejected");
                }
            }
            Ok((InstallStatus::Pending { .. }, _)) => {
                if check.failures > 0 {
                    self.store().write(|tx| {
                        tx.execute("UPDATE install_cards SET retry_failures=NULL WHERE card_id=?1 AND state='pending' AND attempt IS ?2 AND checked_at=?3",
                            params![card.card_id, card.attempt, check.checked_at])?;
                        Ok(())
                    })?;
                }
            }
            Ok((InstallStatus::Unknown, _)) => {
                if self
                    .config()
                    .clock
                    .now_ms()
                    .saturating_sub(check.created_at)
                    > UNKNOWN_CARD_EXPIRY_MS
                {
                    if self.settle_install_card(card, "expired", None, None)? {
                        resolved("expired: core holds no install record");
                    }
                } else {
                    self.retry_card_check(&check, None)?;
                }
            }
            other => self.retry_card_check(
                &check,
                Some(&format!(
                    "install_status did not confirm an active matching version: {other:?}"
                )),
            )?,
        }
        Ok(())
    }

    fn retry_card_check(&self, check: &CardCheck, error: Option<&str>) -> Result<()> {
        let card = &check.card;
        let failures = check.failures.saturating_add(1).min(6);
        let delay = (FIRST_CHECK_RETRY_MS << (failures - 1)).min(MAX_CHECK_RETRY_MS);
        let retry_at = self.config().clock.now_ms().saturating_add(delay);
        let committed = self.store().write(|tx| {
            let changed = tx.execute(
                "UPDATE install_cards SET retry_at=COALESCE(retry_at,?4), retry_failures=?5
                 WHERE card_id=?1 AND state='pending' AND attempt IS ?2 AND checked_at=?3",
                params![card.card_id, card.attempt, check.checked_at, retry_at, failures],
            )?;
            if changed > 0 && let Some(error) = error {
                tx.execute("UPDATE installs SET install_check_error=?3 WHERE flow_id=?1 AND version=?2 AND state='validated'", params![card.flow_id, card.version, error])?;
            }
            Ok(changed > 0)
        })?;
        if committed && let Some(error) = error {
            tracing::warn!(target: "consent", flow_id=card.flow_id, version=card.version, error, "install approval remains pending");
        }
        Ok(())
    }

    fn settle_install_card(
        &self,
        card: &CardRecord,
        state: &str,
        rejected_reason: Option<String>,
        disposition: Option<String>,
    ) -> Result<bool> {
        let now = self.config().clock.now_ms();
        let settled = self.store().write(|tx| {
            let changed = tx.execute("UPDATE install_cards SET state=?2, decided_by='core:card', decided_at=?3, rejected_reason=?5, disposition=?6, retry_at=NULL, retry_failures=NULL WHERE card_id=?1 AND state='pending' AND attempt IS ?4", params![card.card_id, state, now, card.attempt, rejected_reason.as_deref(), disposition.as_deref()])?;
            if changed == 0 { return Ok(false); }
            crate::secret_revocation::enqueue(tx, &card.flow_id, Some(card.version), false, now)?;
            tx.execute("UPDATE installs SET install_check_error=NULL WHERE flow_id=?1 AND version=?2", params![card.flow_id, card.version])?;
            Ok(true)
        })?;
        if settled {
            self.shared.signal.bump();
        }
        Ok(settled)
    }

    /// At startup, recover missed hints and lost policy replies. A card with
    /// a future retry keeps its deadline and will be checked by the engine.
    pub fn recover_cardless_installs(&self) -> Result<()> {
        self.supersede_older_pending_cards()?;
        let pending = self.store().read(|c| {
            let mut stmt = c.prepare(
                "SELECT i.flow_id,i.version,i.code_hash,c.card_id FROM installs i
                 LEFT JOIN install_cards c USING(flow_id,version)
                 WHERE i.state='validated' AND i.revoked_at IS NULL
                   AND (c.card_id IS NULL OR c.state='pending')
                 ORDER BY i.flow_id,i.version DESC",
            )?;
            Ok(stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for (flow_id, version, hash, card) in pending {
            if let Some(id) = card {
                self.reconcile_install_card(&id, false, "startup_recovery")?;
            } else {
                self.confirm_active_install(&flow_id, version, &crate::model::digest(hash)?, None)?;
            }
        }
        Ok(())
    }

    fn supersede_older_pending_cards(&self) -> Result<()> {
        let now = self.config().clock.now_ms();
        self.store().write(|tx| {
            let approved = {
                let mut stmt = tx.prepare(
                    "SELECT f.flow_id, f.approved_version FROM flows f \
                     WHERE f.approved_version IS NOT NULL AND EXISTS (\
                         SELECT 1 FROM install_cards c \
                         WHERE c.flow_id=f.flow_id AND c.state='pending' \
                           AND c.version < f.approved_version) \
                     ORDER BY f.flow_id",
                )?;
                stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (flow_id, version) in approved {
                install::supersede_older_pending_cards(tx, &flow_id, version, now)?;
            }
            Ok(())
        })
    }
}
