//! The consent plane: where basal raises a flow's install card, and where
//! the decision on it comes back from.
//!
//! In production this is core's consent plane (a passive elicitation of kind
//! `flow_install`, answered by the operator from a phone or a session).
//! basal only raises cards and applies decisions; who may decide is the
//! consent plane's business. A decision can arrive long after the card was
//! raised and after basal restarted, and can arrive twice, so it is
//! delivered to an attached [`DecisionSink`] until the sink accepts it, the
//! same way completions of long-running calls are.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::SinkError;

/// One install card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallCard {
    /// Stable for the version the card shows: raising the same card twice
    /// shows one card.
    pub card_id: String,
    pub flow_id: String,
    pub version: u32,
    /// The card's fields as JSON (`docs/design.md` section 6): flow id, version,
    /// purpose, author, trigger, sinks, status targets, claims, ops, facts,
    /// token cap, placement, warnings, code hash and code, and the dry run's
    /// summary.
    pub fields: serde_json::Value,
}

/// The decision on a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardDecision {
    Approve,
    Reject,
}

/// A decision as the consent plane delivers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionEvent {
    pub card_id: String,
    pub decision: CardDecision,
    /// Who decided, as the consent plane attests it.
    pub decided_by: String,
}

/// Which operator decision a decision card asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    /// What happened to a call whose outcome basal could not prove.
    Reconcile,
    /// Whether a flow the runtime disabled for sustained saturation runs
    /// again.
    Reenable,
}

impl DecisionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Reenable => "reenable",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "reconcile" => Some(Self::Reconcile),
            "reenable" => Some(Self::Reenable),
            _ => None,
        }
    }
}

/// One option of a decision card. Exactly one option of a card declines:
/// it does nothing and is the card's default, so a card nobody answers
/// leaves everything as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionOption {
    pub id: String,
    pub label: String,
    pub decline: bool,
}

/// One operator decision card: a question only the operator may answer,
/// raised by basal itself (`docs/design.md` section 6, "Operator decisions
/// are cards").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionCard {
    /// The consent plane's deduplication key: raising a card under a key
    /// whose card is still open updates that card instead of adding one.
    pub dedup_key: String,
    pub flow_id: String,
    pub version: u32,
    pub decision: DecisionKind,
    /// The run, for a reconcile card.
    pub run_id: Option<String>,
    /// The unknown call's journal key (its idempotency key), for a
    /// reconcile card.
    pub call_key: Option<String>,
    pub title: String,
    pub prompt: String,
    /// Lowercase hex digest of what is being decided.
    pub args_digest: String,
    /// What the card shows, as (label, value) pairs.
    pub facts: Vec<(String, String)>,
    pub options: Vec<DecisionOption>,
}

impl DecisionCard {
    /// The option taken when nobody answers: the one that declines.
    pub fn default_option(&self) -> Option<&DecisionOption> {
        self.options.iter().find(|o| o.decline)
    }
}

/// An answer to a decision card, as the consent plane delivers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionAnswer {
    pub elicitation_id: String,
    /// The option chosen; `None` when the card expired unanswered, which
    /// is the same as choosing its default.
    pub choice: Option<String>,
    /// The card's deduplication key, when the consent plane echoes it.
    pub dedup_key: Option<String>,
    pub flow_id: String,
    pub version: u32,
    pub decision: DecisionKind,
    pub run_id: Option<String>,
    pub call_key: Option<String>,
}

/// The consent plane could not take the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentError {
    /// It cannot be reached, or none is configured. Raising again later is
    /// safe: the card id makes it idempotent.
    Unavailable(String),
    /// It refused the card.
    Refused(String),
    /// It refused the card's author: the agent scope the card names is not a
    /// live agent session core knows. `code` is core's refusal code
    /// (`flow_install_scope_unknown` or `flow_install_scope_ended`). The
    /// card was received and answered, so this is not a lost reply, and
    /// raising the same card again cannot succeed: the install has to come
    /// from a route under a live scope.
    AuthorScope { code: String, message: String },
}

impl fmt::Display for ConsentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(e) => write!(f, "consent plane unavailable: {e}"),
            Self::Refused(e) => write!(f, "consent plane refused the card: {e}"),
            Self::AuthorScope { code, message } => write!(
                f,
                "consent plane refused the card's author ({code}): the agent scope it names is not a live agent session in core: {message}"
            ),
        }
    }
}

impl std::error::Error for ConsentError {}

/// Where decisions are delivered. An error means the decision was not
/// recorded; the consent plane keeps it and delivers it again.
pub trait DecisionSink: Send + Sync {
    fn decide(&self, decision: &DecisionEvent) -> Result<(), SinkError>;

    /// Applies an answer to a decision card. An error means it was not
    /// recorded; the consent plane keeps it and delivers it again.
    fn answer(&self, answer: &DecisionAnswer) -> Result<(), SinkError>;
}

/// The consent plane, as basal sees it. Implementations must be safe to
/// call from several threads.
pub trait Consent: Send + Sync {
    /// Raises a card. Raising a card id already raised changes nothing.
    fn raise(&self, card: &InstallCard) -> Result<(), ConsentError>;

    /// Raises an operator decision card and returns the consent plane's id
    /// for it. Raising under a key whose card is still open updates that
    /// card and returns its id.
    fn raise_decision(&self, card: &DecisionCard) -> Result<String, ConsentError>;

    /// Tells the consent plane where to deliver decisions. Decisions made
    /// and not yet accepted are delivered again to the new sink.
    fn attach(&self, sink: Arc<dyn DecisionSink>);
}

#[derive(Default)]
struct State {
    cards: BTreeMap<String, InstallCard>,
    raises: BTreeMap<String, usize>,
    /// Decisions made and not yet accepted by the sink.
    undelivered: Vec<DecisionEvent>,
    /// Decision cards by deduplication key, with their elicitation ids.
    decision_cards: BTreeMap<String, (String, DecisionCard)>,
    decision_raises: BTreeMap<String, usize>,
    /// Answers to decision cards not yet accepted by the sink.
    undelivered_answers: Vec<DecisionAnswer>,
    unavailable: bool,
    sink: Option<Arc<dyn DecisionSink>>,
}

/// An in-memory consent plane for tests. Cloning shares the same state.
#[derive(Clone, Default)]
pub struct MockConsent {
    state: Arc<Mutex<State>>,
}

impl MockConsent {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic in another test thread must not make the mock unusable.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Every card raised, by card id.
    pub fn cards(&self) -> Vec<InstallCard> {
        self.lock().cards.values().cloned().collect()
    }

    pub fn card(&self, card_id: &str) -> Option<InstallCard> {
        self.lock().cards.get(card_id).cloned()
    }

    /// How many times a card was raised (the mock shows it once).
    pub fn raise_count(&self, card_id: &str) -> usize {
        self.lock().raises.get(card_id).copied().unwrap_or(0)
    }

    /// Makes raising fail as unreachable until set back.
    pub fn set_unavailable(&self, unavailable: bool) {
        self.lock().unavailable = unavailable;
    }

    /// Decides a raised card, as the operator would from the card, and
    /// delivers the decision to the attached sink. Returns false for a card
    /// never raised.
    pub fn decide(&self, card_id: &str, decision: CardDecision, decided_by: &str) -> bool {
        {
            let mut state = self.lock();
            if !state.cards.contains_key(card_id) {
                return false;
            }
            state.undelivered.push(DecisionEvent {
                card_id: card_id.to_owned(),
                decision,
                decided_by: decided_by.to_owned(),
            });
        }
        self.redeliver();
        true
    }

    /// Every decision card raised, as last raised.
    pub fn decision_cards(&self) -> Vec<DecisionCard> {
        self.lock()
            .decision_cards
            .values()
            .map(|(_, c)| c.clone())
            .collect()
    }

    /// How many times a decision card was raised under `dedup_key`.
    pub fn decision_raise_count(&self, dedup_key: &str) -> usize {
        self.lock()
            .decision_raises
            .get(dedup_key)
            .copied()
            .unwrap_or(0)
    }

    /// Answers the decision card raised under `dedup_key` with `choice`
    /// (`None` for an expiry) and delivers the answer. Returns false for a
    /// card never raised.
    pub fn answer_decision(&self, dedup_key: &str, choice: Option<&str>) -> bool {
        {
            let mut state = self.lock();
            let Some((elicitation_id, card)) = state.decision_cards.get(dedup_key).cloned() else {
                return false;
            };
            state.undelivered_answers.push(DecisionAnswer {
                elicitation_id,
                choice: choice.map(str::to_owned),
                dedup_key: Some(card.dedup_key.clone()),
                flow_id: card.flow_id,
                version: card.version,
                decision: card.decision,
                run_id: card.run_id,
                call_key: card.call_key,
            });
        }
        self.redeliver();
        true
    }

    /// Decisions made and not yet accepted by a sink.
    pub fn undelivered(&self) -> Vec<DecisionEvent> {
        self.lock().undelivered.clone()
    }

    /// Delivers every decision not yet accepted. A sink error leaves it for
    /// the next attempt.
    pub fn redeliver(&self) {
        let (sink, due, answers) = {
            let state = self.lock();
            (
                state.sink.clone(),
                state.undelivered.clone(),
                state.undelivered_answers.clone(),
            )
        };
        let Some(sink) = sink else {
            return;
        };
        for decision in due {
            // The sink is called without the lock held: it may raise cards
            // or read the mock itself.
            if sink.decide(&decision).is_ok() {
                let mut state = self.lock();
                if let Some(i) = state.undelivered.iter().position(|d| *d == decision) {
                    state.undelivered.remove(i);
                }
            }
        }
        for answer in answers {
            if sink.answer(&answer).is_ok() {
                let mut state = self.lock();
                if let Some(i) = state.undelivered_answers.iter().position(|a| *a == answer) {
                    state.undelivered_answers.remove(i);
                }
            }
        }
    }
}

impl Consent for MockConsent {
    fn raise(&self, card: &InstallCard) -> Result<(), ConsentError> {
        let mut state = self.lock();
        if state.unavailable {
            return Err(ConsentError::Unavailable(
                "the mock is set unavailable".into(),
            ));
        }
        *state.raises.entry(card.card_id.clone()).or_insert(0) += 1;
        state
            .cards
            .entry(card.card_id.clone())
            .or_insert_with(|| card.clone());
        Ok(())
    }

    fn raise_decision(&self, card: &DecisionCard) -> Result<String, ConsentError> {
        let mut state = self.lock();
        if state.unavailable {
            return Err(ConsentError::Unavailable(
                "the mock is set unavailable".into(),
            ));
        }
        *state
            .decision_raises
            .entry(card.dedup_key.clone())
            .or_insert(0) += 1;
        let next = format!("mock-el-{}", state.decision_cards.len() + 1);
        let entry = state
            .decision_cards
            .entry(card.dedup_key.clone())
            .or_insert_with(|| (next, card.clone()));
        // A raise under an open key updates the card the operator sees.
        entry.1 = card.clone();
        Ok(entry.0.clone())
    }

    fn attach(&self, sink: Arc<dyn DecisionSink>) {
        self.lock().sink = Some(sink);
        self.redeliver();
    }
}
