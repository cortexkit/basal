//! Operator decision cards: the two questions only the operator may
//! answer, asked on core's consent plane by basal itself
//! (`docs/design.md` section 6, "Operator decisions are cards").
//!
//! - **Reconcile.** A run in `needs_reconcile` gets one card per unknown
//!   call: leave it (the default), it never ran (send it again), it ran
//!   (continue with a [`RECONCILED_AS_APPLIED`] rejection), or cancel the
//!   run.
//! - **Re-enable.** A flow the runtime disabled for sustained saturation
//!   gets one card per auto-disable episode: keep it disabled (the default)
//!   or re-enable it.
//!
//! Every card is a row in `decision_cards` before core is asked to show
//! it: the row is the intent to raise. The raiser sends every open row
//! whose latest revision core has not accepted, under the core
//! deduplication key of its decision, so a crash anywhere between writing
//! the row and recording core's acceptance raises the same card again and
//! core shows it once. An answer is applied in one transaction with the
//! row's move out of `open`, through the same journaled path as the
//! operator's own op, with the elicitation id in the audit row; a second
//! delivery of the answer finds the row answered and changes nothing.
//!
//! A decision settled another way (`flow.reconcile`, the operator
//! enabling the flow, the run cancelled) leaves its card open in core until
//! it expires. Nothing raises it again, and an answer that arrives later
//! is recorded as stale and never applied.

use basal_host::{DecisionAnswer, DecisionCard, DecisionKind, DecisionOption};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::error::{CoreError, Result};
use crate::install;
use crate::journal;
use crate::model::RunState;
use crate::reconcile::{self, Resolution};
use crate::runs;
use crate::runtime::Runtime;

pub use crate::reconcile::RECONCILED_AS_APPLIED;

/// The actor recorded in the audit for an answer to a card. Only the
/// daemon-attested operator (callosum) can answer a card on core's consent
/// plane, so every answer is the operator's; this is the label
/// `flow.reconcile` records for the operator too.
pub const OPERATOR_ACTOR: &str = "operator";

/// Reconcile card options.
pub const LEAVE: &str = "leave";
pub const NOT_APPLIED: &str = "not_applied";
pub const APPLIED: &str = "applied";
pub const CANCEL: &str = "cancel";
/// Re-enable card options.
pub const KEEP: &str = "keep";
pub const REENABLE: &str = "reenable";

fn option(id: &str, label: &str, decline: bool) -> DecisionOption {
    DecisionOption {
        id: id.to_owned(),
        label: label.to_owned(),
        decline,
    }
}

/// The options of a card, the declining default first.
pub fn options(kind: DecisionKind) -> Vec<DecisionOption> {
    match kind {
        DecisionKind::Reconcile => vec![
            option(LEAVE, "Leave it unresolved", true),
            option(NOT_APPLIED, "It never ran: send it again", false),
            option(APPLIED, "It ran: continue", false),
            option(CANCEL, "Cancel the run", false),
        ],
        DecisionKind::Reenable => vec![
            option(KEEP, "Keep disabled", true),
            option(REENABLE, "Re-enable", false),
        ],
    }
}

/// Core's deduplication key for the reconcile decision on one call.
pub fn reconcile_key(flow_id: &str, run_id: &str, position: u64) -> String {
    format!("flow_decision:reconcile:{flow_id}:{run_id}:{position}")
}

/// Core's deduplication key for the re-enable decision on one
/// auto-disable episode of a flow.
pub fn reenable_key(flow_id: &str, episode: u64) -> String {
    format!("flow_decision:reenable:{flow_id}:{episode}")
}

/// Where a card stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    Open,
    Applied,
    Declined,
    Expired,
    Stale,
}

impl CardState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Applied => "applied",
            Self::Declined => "declined",
            Self::Expired => "expired",
            Self::Stale => "stale",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "open" => Self::Open,
            "applied" => Self::Applied,
            "declined" => Self::Declined,
            "expired" => Self::Expired,
            "stale" => Self::Stale,
            other => return Err(CoreError::Corrupt(format!("decision card state {other:?}"))),
        })
    }
}

/// One card as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRecord {
    pub seq: i64,
    pub dedup_key: String,
    pub kind: DecisionKind,
    pub flow_id: String,
    pub version: u32,
    pub run_id: Option<String>,
    pub position: Option<u64>,
    pub call_key: Option<String>,
    /// For a reconcile card, the call's send attempt that ended unknown;
    /// for a re-enable card, the auto-disable episode.
    pub instance: u64,
    /// What the card shows, as JSON: `title`, `prompt`, `args_digest` and
    /// `facts` (a list of [label, value] pairs).
    pub card: String,
    pub revision: u64,
    pub raised_revision: Option<u64>,
    pub elicitation_id: Option<String>,
    pub state: CardState,
    pub choice: Option<String>,
}

impl DecisionRecord {
    /// The card to raise on the consent plane.
    pub fn to_card(&self) -> Result<DecisionCard> {
        let shown: Value = serde_json::from_str(&self.card)
            .map_err(|e| CoreError::Corrupt(format!("decision card {}: {e}", self.seq)))?;
        let text = |name: &str| shown[name].as_str().unwrap_or_default().to_owned();
        let facts = shown["facts"]
            .as_array()
            .map(|facts| {
                facts
                    .iter()
                    .map(|f| {
                        (
                            f[0].as_str().unwrap_or_default().to_owned(),
                            f[1].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(DecisionCard {
            dedup_key: self.dedup_key.clone(),
            flow_id: self.flow_id.clone(),
            version: self.version,
            decision: self.kind,
            run_id: self.run_id.clone(),
            call_key: self.call_key.clone(),
            title: text("title"),
            prompt: text("prompt"),
            args_digest: text("args_digest"),
            facts,
            options: options(self.kind),
        })
    }
}

const COLUMNS: &str = "seq, dedup_key, kind, flow_id, version, run_id, position, call_key, \
                       instance, card, revision, raised_revision, elicitation_id, state, choice";

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<DecisionRecord>> {
    let seq: i64 = row.get(0)?;
    let dedup_key: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let flow_id: String = row.get(3)?;
    let version: i64 = row.get(4)?;
    let run_id: Option<String> = row.get(5)?;
    let position: Option<i64> = row.get(6)?;
    let call_key: Option<String> = row.get(7)?;
    let instance: i64 = row.get(8)?;
    let card: String = row.get(9)?;
    let revision: i64 = row.get(10)?;
    let raised_revision: Option<i64> = row.get(11)?;
    let elicitation_id: Option<String> = row.get(12)?;
    let state: String = row.get(13)?;
    let choice: Option<String> = row.get(14)?;
    let unsigned = |v: i64, what: &str| crate::model::to_u64(v, what);
    Ok((|| {
        Ok(DecisionRecord {
            seq,
            dedup_key,
            kind: DecisionKind::parse(&kind)
                .ok_or_else(|| CoreError::Corrupt(format!("decision kind {kind:?}")))?,
            flow_id,
            version: u32::try_from(version)
                .map_err(|_| CoreError::Corrupt(format!("decision version {version}")))?,
            run_id,
            position: position.map(|p| unsigned(p, "position")).transpose()?,
            call_key,
            instance: unsigned(instance, "instance")?,
            card,
            revision: unsigned(revision, "revision")?,
            raised_revision: raised_revision
                .map(|r| unsigned(r, "raised revision"))
                .transpose()?,
            elicitation_id,
            state: CardState::parse(&state)?,
            choice,
        })
    })())
}

fn select(
    conn: &Connection,
    filter: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<DecisionRecord>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM decision_cards {filter}"))?;
    let rows = stmt
        .query_map(args, from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().collect()
}

fn first(
    conn: &Connection,
    filter: &str,
    args: impl rusqlite::Params,
) -> Result<Option<DecisionRecord>> {
    Ok(select(conn, filter, args)?.into_iter().next())
}

/// Every card, oldest first.
pub fn all(conn: &Connection) -> Result<Vec<DecisionRecord>> {
    select(conn, "ORDER BY seq", [])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn when(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| format!("{ms} ms"))
}

fn pos(p: u64) -> Result<i64> {
    i64::try_from(p).map_err(|_| CoreError::Invalid(format!("position {p}")))
}

fn shown(title: String, prompt: String, args_digest: String, facts: &[(&str, String)]) -> String {
    json!({
        "title": title,
        "prompt": prompt,
        "args_digest": args_digest,
        "facts": facts.iter().map(|(l, v)| json!([l, v])).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Writes or updates the reconcile card intent of every unknown call of
/// every run in `needs_reconcile`. Cheap when nothing changed: it reads the
/// few runs waiting for the operator and writes only what differs.
pub fn refresh(tx: &Transaction, now_ms: i64) -> Result<()> {
    let runs: Vec<String> = {
        let mut stmt =
            tx.prepare("SELECT run_id FROM runs WHERE state = 'needs_reconcile' ORDER BY run_id")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for run_id in runs {
        refresh_run(tx, &run_id, now_ms)?;
    }
    Ok(())
}

/// The reconcile card intents of one run: one per unknown call. A call's
/// open card is updated when what it shows changed (another send of the
/// call ended unknown, with its own error); a decision already made for
/// this send of the call is never asked again.
pub fn refresh_run(tx: &Transaction, run_id: &str, now_ms: i64) -> Result<()> {
    let run = runs::load(tx, run_id)?;
    if run.state != RunState::NeedsReconcile {
        return Ok(());
    }
    let version = run.flow_version.unwrap_or(0);
    for position in journal::unknown_positions(tx, run_id)? {
        let Some(row) = journal::row(tx, run_id, position)? else {
            continue;
        };
        let (detail, op, sent_at): (Option<String>, Option<String>, Option<i64>) = tx.query_row(
            "SELECT j.unknown_detail, a.op, a.at FROM journal j LEFT JOIN call_audit a \
             ON a.run_id = j.run_id AND a.position = j.position \
             WHERE j.run_id = ?1 AND j.position = ?2",
            params![run_id, pos(position)?],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let op = op.unwrap_or_else(|| row.kind.to_string());
        let digest = hex(&row.args_digest.0);
        let error = detail.unwrap_or_else(|| "the outcome could not be established".to_owned());
        let card = shown(
            format!("Reconcile {op} in flow {} v{version}", run.flow_id),
            format!(
                "A call of run {run_id} ended without a provable outcome. Did it take effect? \
                 Until you decide, the run waits."
            ),
            digest.clone(),
            &[
                ("flow", run.flow_id.clone()),
                ("version", version.to_string()),
                ("run", run_id.to_owned()),
                ("op", op),
                ("arguments digest", digest),
                (
                    "sent at",
                    sent_at.map(when).unwrap_or_else(|| "unknown".into()),
                ),
                ("attempt", row.attempts.to_string()),
                ("error", error),
            ],
        );
        let key = reconcile_key(&run.flow_id, run_id, position);
        let instance = i64::from(row.attempts);
        let decided: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM decision_cards WHERE dedup_key = ?1 AND instance = ?2 \
             AND state <> 'open')",
            params![key, instance],
            |r| r.get(0),
        )?;
        if decided {
            continue;
        }
        let open: Option<(i64, i64, String)> = tx
            .query_row(
                "SELECT seq, instance, card FROM decision_cards WHERE dedup_key = ?1 \
                 AND state = 'open'",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match open {
            Some((_, open_instance, open_card))
                if open_instance == instance && open_card == card => {}
            Some((seq, _, _)) => {
                tx.execute(
                    "UPDATE decision_cards SET instance = ?2, card = ?3, revision = revision + 1 \
                     WHERE seq = ?1",
                    params![seq, instance, card],
                )?;
            }
            None => {
                tx.execute(
                    "INSERT INTO decision_cards (dedup_key, kind, flow_id, version, run_id, \
                     position, call_key, instance, card, state, created_at) \
                     VALUES (?1, 'reconcile', ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'open', ?9)",
                    params![
                        key,
                        run.flow_id,
                        i64::from(version),
                        run_id,
                        pos(position)?,
                        row.idempotency_key,
                        instance,
                        card,
                        now_ms
                    ],
                )?;
            }
        }
    }
    Ok(())
}

/// Writes the re-enable card intent for a flow the runtime has just
/// disabled, in the transaction that disabled it. `rule` names the rule
/// that tripped and its numbers. Each auto-disable is a new episode with
/// its own card.
pub fn record_auto_disable(
    tx: &Transaction,
    flow_id: &str,
    rule: &Value,
    now_ms: i64,
) -> Result<()> {
    let episode: i64 = tx.query_row(
        "SELECT COALESCE(MAX(instance), 0) + 1 FROM decision_cards \
         WHERE kind = 'reenable' AND flow_id = ?1",
        [flow_id],
        |r| r.get(0),
    )?;
    let version: i64 = tx.query_row(
        "SELECT COALESCE((SELECT approved_version FROM flows WHERE flow_id = ?1), \
         (SELECT MAX(version) FROM installs WHERE flow_id = ?1), 0)",
        [flow_id],
        |r| r.get(0),
    )?;
    let episode_u = crate::model::to_u64(episode, "episode")?;
    let mut digest = blake3::Hasher::new();
    digest.update(b"basal.flow_decision.reenable.v1\0");
    digest.update(flow_id.as_bytes());
    digest.update(&episode_u.to_be_bytes());
    let number = |name: &str| rule[name].to_string();
    let card = shown(
        format!("Re-enable flow {flow_id}"),
        format!(
            "basal disabled flow {flow_id} after its rate limit saturated {} windows in a row. \
             It stays disabled until you re-enable it.",
            number("saturated_windows")
        ),
        digest.finalize().to_hex().to_string(),
        &[
            ("flow", flow_id.to_owned()),
            ("version", version.to_string()),
            ("rule", rule["rule"].as_str().unwrap_or_default().to_owned()),
            (
                "limit that refused",
                rule["limit"].as_str().unwrap_or_default().to_owned(),
            ),
            ("consecutive saturated windows", number("saturated_windows")),
            ("window", format!("{} ms", number("window_ms"))),
            ("runs allowed per window", number("max_runs")),
            ("dispatches allowed per window", number("max_dispatches")),
            ("disabled at", when(now_ms)),
        ],
    );
    tx.execute(
        "INSERT INTO decision_cards (dedup_key, kind, flow_id, version, instance, card, state, \
         created_at) VALUES (?1, 'reenable', ?2, ?3, ?4, ?5, 'open', ?6)",
        params![
            reenable_key(flow_id, episode_u),
            flow_id,
            version,
            episode,
            card,
            now_ms
        ],
    )?;
    Ok(())
}

/// Whether the decision a card asks for is still waiting for the operator:
/// for a reconcile card, its run is in `needs_reconcile` and this send of
/// the call is still unknown; for a re-enable card, the flow is still
/// disabled by the runtime in this card's episode.
pub fn stands(conn: &Connection, card: &DecisionRecord) -> Result<bool> {
    match (card.kind, &card.run_id, card.position) {
        (DecisionKind::Reconcile, Some(run_id), Some(position)) => {
            let state: Option<String> = conn
                .query_row("SELECT state FROM runs WHERE run_id = ?1", [run_id], |r| {
                    r.get(0)
                })
                .optional()?;
            if state.as_deref() != Some(RunState::NeedsReconcile.as_str()) {
                return Ok(false);
            }
            let attempts: Option<i64> = conn
                .query_row(
                    "SELECT attempts FROM journal WHERE run_id = ?1 AND position = ?2",
                    params![run_id, pos(position)?],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(
                journal::unknown_positions(conn, run_id)?.contains(&position)
                    && attempts.and_then(|a| u64::try_from(a).ok()) == Some(card.instance),
            )
        }
        (DecisionKind::Reenable, None, None) => {
            let disabled_by_runtime: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM flows WHERE flow_id = ?1 AND state = 'disabled' \
                 AND disabled_by = ?2)",
                params![card.flow_id, install::RUNTIME_ACTOR],
                |r| r.get(0),
            )?;
            let latest: i64 = conn.query_row(
                "SELECT COALESCE(MAX(instance), 0) FROM decision_cards \
                 WHERE kind = 'reenable' AND flow_id = ?1",
                [&card.flow_id],
                |r| r.get(0),
            )?;
            Ok(disabled_by_runtime && u64::try_from(latest).ok() == Some(card.instance))
        }
        _ => Err(CoreError::Corrupt(format!(
            "decision card {} names the wrong subject for its kind",
            card.seq
        ))),
    }
}

/// Open cards whose latest revision core has not accepted and whose
/// decision still stands: what the raiser sends.
pub fn due(conn: &Connection) -> Result<Vec<DecisionRecord>> {
    let open = select(
        conn,
        "WHERE state = 'open' AND (raised_revision IS NULL OR raised_revision < revision) \
         ORDER BY seq",
        [],
    )?;
    let mut due = Vec::new();
    for card in open {
        if stands(conn, &card)? {
            due.push(card);
        }
    }
    Ok(due)
}

/// Records that core accepted `revision` of a card under `elicitation_id`.
pub fn mark_raised(tx: &Transaction, seq: i64, revision: u64, elicitation_id: &str) -> Result<()> {
    let revision =
        i64::try_from(revision).map_err(|_| CoreError::Invalid(format!("revision {revision}")))?;
    tx.execute(
        "UPDATE decision_cards SET elicitation_id = ?3, \
         raised_revision = MAX(COALESCE(raised_revision, 0), ?2) WHERE seq = ?1",
        params![seq, revision, elicitation_id],
    )?;
    Ok(())
}

/// What applying an answer did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answered {
    /// The card moved to this state now.
    Now(CardState),
    /// The card had been answered before; nothing changed.
    AlreadyAnswered(CardState),
    /// The answer came from an earlier card under the same key that core
    /// has since replaced; the open card stands and nothing changed.
    Superseded,
    /// No card matches the answer; nothing changed.
    NoSuchCard,
}

/// The answer's card: the one core accepted under the answer's elicitation
/// id, or else (core accepted the card but basal crashed before recording
/// the id) the card the answer's key or typed body names.
fn find(conn: &Connection, answer: &DecisionAnswer) -> Result<Option<DecisionRecord>> {
    if let Some(card) = first(
        conn,
        "WHERE elicitation_id = ?1 ORDER BY seq DESC",
        [&answer.elicitation_id],
    )? {
        return Ok(Some(card));
    }
    if let Some(key) = &answer.dedup_key
        && let Some(card) = first(
            conn,
            "WHERE dedup_key = ?1 ORDER BY state = 'open' DESC, seq DESC",
            [key],
        )?
    {
        return Ok(Some(card));
    }
    match answer.decision {
        DecisionKind::Reconcile => first(
            conn,
            "WHERE kind = 'reconcile' AND flow_id = ?1 AND run_id = ?2 AND call_key = ?3 \
             ORDER BY state = 'open' DESC, seq DESC",
            params![answer.flow_id, answer.run_id, answer.call_key],
        ),
        DecisionKind::Reenable => first(
            conn,
            "WHERE kind = 'reenable' AND flow_id = ?1 ORDER BY instance DESC",
            [&answer.flow_id],
        ),
    }
}

/// Applies an answer, in `tx`. Returns what happened and, when the answer
/// made a run runnable, that run.
pub fn answer(
    tx: &Transaction,
    answer: &DecisionAnswer,
    now_ms: i64,
) -> Result<(Answered, Option<String>)> {
    let Some(card) = find(tx, answer)? else {
        return Ok((Answered::NoSuchCard, None));
    };
    if card.kind != answer.decision || card.flow_id != answer.flow_id {
        return Ok((Answered::NoSuchCard, None));
    }
    if card.state != CardState::Open {
        return Ok((Answered::AlreadyAnswered(card.state), None));
    }
    let audit = |action: &str, detail: &str| {
        reconcile::audit_answer(
            tx,
            OPERATOR_ACTOR,
            action,
            card.run_id.as_deref(),
            card.position,
            detail,
            Some(&answer.elicitation_id),
        )
    };
    if card
        .elicitation_id
        .as_deref()
        .is_some_and(|id| id != answer.elicitation_id)
    {
        audit("decision.superseded", &card.dedup_key)?;
        return Ok((Answered::Superseded, None));
    }
    let options = options(card.kind);
    let declined = options
        .iter()
        .find(|o| o.decline)
        .map(|o| o.id.as_str())
        .unwrap_or_default();
    let mut woke = None;
    let state = match answer.choice.as_deref() {
        None => {
            audit("decision.expired", &card.dedup_key)?;
            CardState::Expired
        }
        Some(choice) if choice == declined => {
            audit("decision.declined", &card.dedup_key)?;
            CardState::Declined
        }
        Some(choice) if !options.iter().any(|o| o.id == choice) => {
            return Err(CoreError::Invalid(format!(
                "decision card {} has no option {choice:?}",
                card.dedup_key
            )));
        }
        Some(_) if !stands(tx, &card)? => {
            audit("decision.stale", &card.dedup_key)?;
            CardState::Stale
        }
        Some(choice) => {
            match (card.kind, &card.run_id, card.position) {
                (DecisionKind::Reconcile, Some(run_id), Some(position)) => {
                    let resolution = match choice {
                        NOT_APPLIED => Resolution::NotApplied,
                        APPLIED => Resolution::ReconciledAsApplied,
                        _ => Resolution::Cancel,
                    };
                    let after = reconcile::resolve(
                        tx,
                        run_id,
                        position,
                        &resolution,
                        OPERATOR_ACTOR,
                        Some(&answer.elicitation_id),
                    )?;
                    if after == RunState::Pending {
                        woke = Some(run_id.clone());
                    }
                }
                _ => {
                    install::enable(tx, &card.flow_id, now_ms).map_err(|e| match e {
                        install::InstallError::Store(e) => e,
                        other => CoreError::Invalid(other.to_string()),
                    })?;
                    audit("flow.reenable", &card.flow_id)?;
                }
            }
            CardState::Applied
        }
    };
    tx.execute(
        "UPDATE decision_cards SET state = ?2, choice = ?3, answered_at = ?4, \
         elicitation_id = COALESCE(elicitation_id, ?5) WHERE seq = ?1 AND state = 'open'",
        params![
            card.seq,
            state.as_str(),
            answer.choice,
            now_ms,
            answer.elicitation_id
        ],
    )?;
    Ok((Answered::Now(state), woke))
}

impl Runtime {
    /// Brings the reconcile card intents up to date and returns the cards
    /// to raise, in one transaction.
    pub fn decisions_due(&self) -> Result<Vec<DecisionRecord>> {
        let now = self.config().clock.now_ms();
        self.store().write(|tx| {
            refresh(tx, now)?;
            due(tx)
        })
    }

    /// Records that core accepted a card's revision.
    pub fn decision_raised(&self, seq: i64, revision: u64, elicitation_id: &str) -> Result<()> {
        self.store()
            .write(|tx| mark_raised(tx, seq, revision, elicitation_id))
    }

    /// Applies an answer to a decision card. A store error leaves the card
    /// open, so the consent plane can deliver the answer again.
    pub fn answer_decision(&self, decision: &DecisionAnswer) -> Result<Answered> {
        let now = self.config().clock.now_ms();
        let (answered, woke) = self.store().write(|tx| answer(tx, decision, now))?;
        self.shared.signal.bump();
        if let Some(run_id) = woke {
            self.wake(&run_id);
        }
        Ok(answered)
    }

    /// Every decision card, oldest first.
    pub fn decision_cards(&self) -> Result<Vec<DecisionRecord>> {
        self.store().read(all)
    }
}
