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

use basal_host::core_consent::DECISION_EXPIRES_IN_MS;
use basal_host::{
    DecisionAnswer, DecisionCard, DecisionContext, DecisionKind, DecisionOption, DisabledReason,
    UnknownReason,
};
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

/// Reconcile card option: leave the call unresolved (the default).
pub const LEAVE: &str = "leave";
/// Reconcile card option: the call never ran; send it again.
pub const NOT_APPLIED: &str = "not_applied";
/// Reconcile card option: the call ran; continue without its result.
pub const APPLIED: &str = "applied";
/// Reconcile card option: cancel the run.
pub const CANCEL: &str = "cancel";
/// Re-enable card option: keep the flow disabled (the default).
pub const KEEP: &str = "keep";
/// Re-enable card option: enable the flow again.
pub const REENABLE: &str = "reenable";

fn option(id: &str, label: &str, decline: bool) -> DecisionOption {
    DecisionOption {
        id: id.to_owned(),
        label: label.to_owned(),
        decline,
    }
}

/// The options of a card, each action labelled as the decision itself (a
/// phone arms an action on its first tap and sends it on the second), and
/// the declining default last, as in core's test vectors.
pub fn options(kind: DecisionKind) -> Vec<DecisionOption> {
    match kind {
        DecisionKind::Reconcile => vec![
            option(NOT_APPLIED, "Send the call again", false),
            option(APPLIED, "Continue as applied", false),
            option(CANCEL, "Cancel the run", false),
            option(LEAVE, "Leave it unresolved", true),
        ],
        DecisionKind::Reenable => vec![
            option(REENABLE, "Re-enable the flow", false),
            option(KEEP, "Keep disabled", true),
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
    /// What the card is about, as stored: the typed context (see
    /// [`context_json`]), plus `args_digest` on a reconcile card.
    pub card: String,
    pub revision: u64,
    pub raised_revision: Option<u64>,
    pub elicitation_id: Option<String>,
    pub state: CardState,
    pub choice: Option<String>,
}

impl DecisionRecord {
    /// The card's typed context, as stored.
    pub fn context(&self) -> Result<DecisionContext> {
        let corrupt = |why: &str| CoreError::Corrupt(format!("decision card {}: {why}", self.seq));
        let v: Value = serde_json::from_str(&self.card).map_err(|e| corrupt(&e.to_string()))?;
        let int = |name: &str| v[name].as_i64().ok_or_else(|| corrupt(name));
        let small = |name: &str| {
            v[name]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| corrupt(name))
        };
        let text = |name: &str| {
            v[name]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| corrupt(name))
        };
        Ok(match self.kind {
            DecisionKind::Reconcile => DecisionContext::Reconcile {
                run_id: text("run_id")?,
                run_admitted_at_ms: int("run_admitted_at_ms")?,
                call_key: text("call_key")?,
                op: text("op")?,
                attempts: small("attempts")?,
                unknown_reason: UnknownReason::parse(&text("unknown_reason")?)
                    .ok_or_else(|| corrupt("unknown_reason"))?,
            },
            DecisionKind::Reenable => DecisionContext::Reenable {
                disabled_at_ms: int("disabled_at_ms")?,
                disabled_reason: DisabledReason::parse(&text("disabled_reason")?)
                    .ok_or_else(|| corrupt("disabled_reason"))?,
                limit: small("limit")?,
                window_ms: v["window_ms"]
                    .as_u64()
                    .ok_or_else(|| corrupt("window_ms"))?,
                saturated_windows: small("saturated_windows")?,
            },
        })
    }

    /// The card to raise on the consent plane: the typed context, and the
    /// title, prompt and facts written from it.
    pub fn to_card(&self) -> Result<DecisionCard> {
        let context = self.context()?;
        let args_digest = serde_json::from_str::<Value>(&self.card)
            .ok()
            .and_then(|v| v["args_digest"].as_str().map(str::to_owned));
        Ok(DecisionCard {
            dedup_key: Some(self.dedup_key.clone()),
            flow_id: self.flow_id.clone(),
            version: self.version,
            title: title(&self.flow_id, &context),
            prompt: prompt(&self.flow_id, &context),
            facts: facts(&context),
            context,
            args_digest,
            options: options(self.kind),
            expires_in_ms: DECISION_EXPIRES_IN_MS,
        })
    }
}

/// A stored context: the typed fields of the card's body, as JSON.
pub fn context_json(context: &DecisionContext) -> Value {
    match context {
        DecisionContext::Reconcile {
            run_id,
            run_admitted_at_ms,
            call_key,
            op,
            attempts,
            unknown_reason,
        } => json!({
            "run_id": run_id,
            "run_admitted_at_ms": run_admitted_at_ms,
            "call_key": call_key,
            "op": op,
            "attempts": attempts,
            "unknown_reason": unknown_reason.as_str(),
        }),
        DecisionContext::Reenable {
            disabled_at_ms,
            disabled_reason,
            limit,
            window_ms,
            saturated_windows,
        } => json!({
            "disabled_at_ms": disabled_at_ms,
            "disabled_reason": disabled_reason.as_str(),
            "limit": limit,
            "window_ms": window_ms,
            "saturated_windows": saturated_windows,
        }),
    }
}

/// A time as the card shows it, in UTC.
fn clock(ms: i64, pattern: &str) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::UTC)
                .strftime(pattern)
                .to_string()
        })
        .unwrap_or_else(|_| format!("{ms} ms"))
}

fn window(ms: u64) -> String {
    if ms % 1000 == 0 {
        format!("{}-second", ms / 1000)
    } else {
        format!("{ms}-millisecond")
    }
}

/// What follows "its call to <op> was sent and then" in a reconcile prompt.
fn what_happened(reason: UnknownReason) -> &'static str {
    match reason {
        UnknownReason::BasalRestarted => "basal restarted before the reply was saved",
        UnknownReason::ConnectionLost => "the connection closed before a reply came",
        UnknownReason::ReplyTimeout => "no reply came within the call's deadline",
        UnknownReason::ReplyUnreadable => "the reply that came could not be read",
        UnknownReason::RetriesExhausted => "every retry ended without a clear answer",
        UnknownReason::ProviderLostRun => "Broca lost track of the run it had accepted",
    }
}

/// The unit of a per-window limit.
fn unit(reason: DisabledReason) -> &'static str {
    match reason {
        DisabledReason::RunLimitSaturated => "runs",
        DisabledReason::DispatchLimitSaturated => "calls",
    }
}

fn title(flow_id: &str, context: &DecisionContext) -> String {
    match context {
        DecisionContext::Reconcile { .. } => format!("A call of {flow_id} may not have finished"),
        DecisionContext::Reenable { .. } => format!("{flow_id} was disabled"),
    }
}

/// The card's prompt: one readable sentence written from its context.
pub fn prompt(flow_id: &str, context: &DecisionContext) -> String {
    match context {
        DecisionContext::Reconcile {
            run_admitted_at_ms,
            op,
            unknown_reason,
            ..
        } => format!(
            "Run {} of {flow_id} may not have finished: its call to {op} was sent and then {}.",
            clock(*run_admitted_at_ms, "%H:%M UTC"),
            what_happened(*unknown_reason)
        ),
        DecisionContext::Reenable {
            disabled_at_ms,
            disabled_reason,
            limit,
            window_ms,
            saturated_windows,
        } => format!(
            "basal disabled {flow_id} at {}: it reached its limit of {limit} {} per {} window \
             in {saturated_windows} windows in a row.",
            clock(*disabled_at_ms, "%H:%M UTC"),
            unit(*disabled_reason),
            window(*window_ms)
        ),
    }
}

/// The card's facts beside the flow, version and decision, which core
/// shows itself.
fn facts(context: &DecisionContext) -> Vec<(String, String)> {
    let fact = |label: &str, value: String| (label.to_owned(), value);
    match context {
        DecisionContext::Reconcile {
            run_id,
            run_admitted_at_ms,
            op,
            attempts,
            unknown_reason,
            ..
        } => vec![
            fact("Run", run_id.clone()),
            fact(
                "Run admitted",
                clock(*run_admitted_at_ms, "%Y-%m-%d %H:%M:%S UTC"),
            ),
            fact("Call", op.clone()),
            fact("Sends", attempts.to_string()),
            fact("Why unknown", unknown_reason.as_str().to_owned()),
        ],
        DecisionContext::Reenable {
            disabled_at_ms,
            disabled_reason,
            limit,
            window_ms,
            saturated_windows,
        } => vec![
            fact(
                "Disabled at",
                clock(*disabled_at_ms, "%Y-%m-%d %H:%M:%S UTC"),
            ),
            fact("Why disabled", disabled_reason.as_str().to_owned()),
            fact(
                "Limit",
                format!(
                    "{limit} {} per {} window",
                    unit(*disabled_reason),
                    window(*window_ms)
                ),
            ),
            fact("Saturated windows in a row", saturated_windows.to_string()),
        ],
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

fn pos(p: u64) -> Result<i64> {
    i64::try_from(p).map_err(|_| CoreError::Invalid(format!("position {p}")))
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
/// open card is updated when its context changed (another send of the call
/// ended unknown, for its own reason); a decision already made for this
/// send of the call is never asked again.
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
        let reason: String = tx.query_row(
            "SELECT unknown_reason FROM journal WHERE run_id = ?1 AND position = ?2",
            params![run_id, pos(position)?],
            |r| r.get(0),
        )?;
        let unknown_reason = UnknownReason::parse(&reason)
            .ok_or_else(|| CoreError::Corrupt(format!("unknown reason {reason:?}")))?;
        let context = DecisionContext::Reconcile {
            run_id: run_id.to_owned(),
            run_admitted_at_ms: run.admitted_at,
            call_key: row.idempotency_key.clone(),
            op: basal_host::op_label(&row.kind),
            attempts: row.attempts,
            unknown_reason,
        };
        let mut stored = context_json(&context);
        stored["args_digest"] = json!(hex(&row.args_digest.0));
        let card = stored.to_string();
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
/// disabled, in the transaction that disabled it, so the rule's numbers as
/// they were when it tripped are kept with the disable. Each auto-disable
/// is a new episode with its own card.
pub fn record_auto_disable(
    tx: &Transaction,
    flow_id: &str,
    rule: &DecisionContext,
    now_ms: i64,
) -> Result<()> {
    if rule.kind() != DecisionKind::Reenable {
        return Err(CoreError::Invalid(
            "an auto-disable records a re-enable context".into(),
        ));
    }
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
    tx.execute(
        "INSERT INTO decision_cards (dedup_key, kind, flow_id, version, instance, card, state, \
         created_at) VALUES (?1, 'reenable', ?2, ?3, ?4, ?5, 'open', ?6)",
        params![
            reenable_key(flow_id, episode_u),
            flow_id,
            version,
            episode,
            context_json(rule).to_string(),
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
