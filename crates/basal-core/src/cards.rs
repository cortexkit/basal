//! Install cards: the consent step between installing a version and
//! approving it.
//!
//! `flow.install` validates a version, records it, runs its capture-only dry
//! run and raises a card on the consent plane. The decision comes back later,
//! through the consent interface, maybe after a restart, maybe more than
//! once. So the card is a durable record keyed by a card id derived from the
//! exact version it shows (flow, version and code hash): raising the same
//! version again finds the same card, and a decision is applied at most once,
//! in the same transaction as the approval it causes.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::{CoreError, Result};
use crate::install::{self, InstallError};
use crate::runtime::Runtime;
use crate::schedule::SchedulerConfig;

/// Where a card stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    /// Raised; no decision yet.
    Pending,
    /// Approved: the version it shows is the flow's approved version (or was,
    /// until a newer one was approved).
    Approved,
    Rejected,
    /// An approval that arrived after a newer version of the flow had
    /// already been approved. Approving it would roll the flow back, so it
    /// changed nothing.
    Stale,
}

impl CardState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Stale => "stale",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            "stale" => Ok(Self::Stale),
            other => Err(CoreError::Corrupt(format!("card state {other:?}"))),
        }
    }
}

/// One card as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRecord {
    pub card_id: String,
    pub flow_id: String,
    pub version: u32,
    pub code_hash: [u8; 32],
    /// The agent (or the operator) that installed the version; it is told
    /// the decision.
    pub author: String,
    /// The card's fields as JSON, exactly as raised.
    pub card: String,
    pub state: CardState,
    pub decided_by: Option<String>,
}

/// The decision on a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject,
}

/// What applying a decision did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decided {
    /// The decision was applied now and left the card in this state.
    Applied(CardState),
    /// The card had been decided before; nothing changed. A repeated
    /// delivery of the same decision lands here, and so does a contradictory
    /// one: the first decision stands.
    AlreadyDecided(CardState),
}

/// The card id for a version: derived from what the card shows, so the same
/// version always maps to the same card, and a different code hash (which
/// can only be a different version) never does.
pub fn card_id(flow_id: &str, version: u32, code_hash: &[u8; 32]) -> String {
    let hex: String = code_hash[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("card:{flow_id}:v{version}:{hex}")
}

const COLUMNS: &str = "card_id, flow_id, version, code_hash, author, card, state, decided_by";

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<CardRecord>> {
    let card_id: String = row.get(0)?;
    let flow_id: String = row.get(1)?;
    let version: i64 = row.get(2)?;
    let hash: Vec<u8> = row.get(3)?;
    let author: String = row.get(4)?;
    let card: String = row.get(5)?;
    let state: String = row.get(6)?;
    let decided_by: Option<String> = row.get(7)?;
    Ok((|| {
        Ok(CardRecord {
            card_id,
            flow_id,
            version: u32::try_from(version)
                .map_err(|_| CoreError::Corrupt(format!("card version {version}")))?,
            code_hash: crate::model::digest(hash)?,
            author,
            card,
            state: CardState::parse(&state)?,
            decided_by,
        })
    })())
}

/// The card with this id, if any.
pub fn load(conn: &Connection, card_id: &str) -> Result<Option<CardRecord>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM install_cards WHERE card_id = ?1"),
        [card_id],
        from_row,
    )
    .optional()?
    .transpose()
}

/// Every card of a flow, oldest version first.
pub fn for_flow(conn: &Connection, flow_id: &str) -> Result<Vec<CardRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM install_cards WHERE flow_id = ?1 ORDER BY version"
    ))?;
    let rows = stmt
        .query_map([flow_id], from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().collect()
}

/// Records the card for an installed version, or returns the one already
/// recorded for it. The card text of an existing card is kept: it is what
/// was raised, and the decision refers to it.
pub fn record(
    tx: &Transaction,
    flow_id: &str,
    version: u32,
    code_hash: &[u8; 32],
    author: &str,
    card: &str,
    now_ms: i64,
) -> Result<CardRecord> {
    let id = card_id(flow_id, version, code_hash);
    if let Some(existing) = load(tx, &id)? {
        return Ok(existing);
    }
    tx.execute(
        "INSERT INTO install_cards (card_id, flow_id, version, code_hash, author, card, state, \
         created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
        params![
            id,
            flow_id,
            i64::from(version),
            code_hash.as_slice(),
            author,
            card,
            now_ms
        ],
    )?;
    load(tx, &id)?.ok_or_else(|| CoreError::Store(format!("card {id} vanished after insert")))
}

/// Applies a decision to a pending card. Approval approves the card's
/// version, bound to its code hash, in the same transaction, so a crash can
/// never leave a card approved without its version approved or the reverse.
/// The author is told either way, through the owner outbox.
pub fn decide(
    tx: &Transaction,
    card_id: &str,
    decision: Decision,
    decided_by: &str,
    now_ms: i64,
    schedule: &SchedulerConfig,
) -> std::result::Result<Decided, InstallError> {
    let Some(card) = load(tx, card_id)? else {
        return Err(InstallError::NoSuchCard(card_id.to_owned()));
    };
    if card.state != CardState::Pending {
        return Ok(Decided::AlreadyDecided(card.state));
    }
    let state = match decision {
        Decision::Reject => CardState::Rejected,
        Decision::Approve => match install::approve(
            tx,
            &card.flow_id,
            card.version,
            &card.code_hash,
            card_id,
            now_ms,
            schedule,
        ) {
            Ok(_) => CardState::Approved,
            Err(InstallError::StaleVersion { .. }) => CardState::Stale,
            Err(e) => return Err(e),
        },
    };
    tx.execute(
        "UPDATE install_cards SET state = ?2, decided_by = ?3, decided_at = ?4 \
         WHERE card_id = ?1 AND state = 'pending'",
        params![card_id, state.as_str(), decided_by, now_ms],
    )?;
    let body = serde_json::json!({
        "flow_id": card.flow_id,
        "version": card.version,
        "card_id": card_id,
        "decision": state.as_str(),
        "by": decided_by,
    });
    // The author may not own the flow yet (a first version has no owner
    // until it is approved), so the recipient is named here rather than
    // looked up from the flow.
    tx.execute(
        "INSERT INTO outbox (at, kind, flow_id, recipient, body) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            now_ms,
            format!("flow.install_{}", state.as_str()),
            card.flow_id,
            card.author,
            body.to_string()
        ],
    )?;
    Ok(Decided::Applied(state))
}

impl Runtime {
    /// Records (or finds) the card for an installed version.
    pub fn record_card(
        &self,
        flow_id: &str,
        version: u32,
        code_hash: &[u8; 32],
        author: &str,
        card: &str,
    ) -> Result<CardRecord> {
        let now = self.config().clock.now_ms();
        self.store()
            .write(|tx| record(tx, flow_id, version, code_hash, author, card, now))
    }

    pub fn card(&self, card_id: &str) -> Result<Option<CardRecord>> {
        self.store().read(|c| load(c, card_id))
    }

    pub fn cards(&self, flow_id: &str) -> Result<Vec<CardRecord>> {
        self.store().read(|c| for_flow(c, flow_id))
    }

    /// Applies a card's decision. A store failure is returned as
    /// [`InstallError::Store`] and leaves the card pending, so the consent
    /// plane can deliver the decision again.
    pub fn decide_card(
        &self,
        card_id: &str,
        decision: Decision,
        decided_by: &str,
    ) -> std::result::Result<Decided, InstallError> {
        let now = self.config().clock.now_ms();
        let schedule = self.config().schedule.clone();
        let mut outcome = None;
        self.store()
            .write(|tx| {
                // A refusal is a decision about the card, not a store
                // failure: report it without rolling anything back.
                match decide(tx, card_id, decision, decided_by, now, &schedule) {
                    Err(InstallError::Store(e)) => Err(e),
                    other => {
                        outcome = Some(other);
                        Ok(())
                    }
                }
            })
            .map_err(InstallError::Store)?;
        self.shared.signal.bump();
        outcome.unwrap_or_else(|| {
            Err(InstallError::Store(CoreError::Store(
                "the card decision produced no outcome".into(),
            )))
        })
    }
}
