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
    /// The card's fields as JSON (design section 6): flow id, version,
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

/// The consent plane could not take the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentError {
    /// It cannot be reached, or none is configured. Raising again later is
    /// safe: the card id makes it idempotent.
    Unavailable(String),
    /// It refused the card.
    Refused(String),
}

impl fmt::Display for ConsentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(e) => write!(f, "consent plane unavailable: {e}"),
            Self::Refused(e) => write!(f, "consent plane refused the card: {e}"),
        }
    }
}

impl std::error::Error for ConsentError {}

/// Where decisions are delivered. An error means the decision was not
/// recorded; the consent plane keeps it and delivers it again.
pub trait DecisionSink: Send + Sync {
    fn decide(&self, decision: &DecisionEvent) -> Result<(), SinkError>;
}

/// The consent plane, as basal sees it. Implementations must be safe to
/// call from several threads.
pub trait Consent: Send + Sync {
    /// Raises a card. Raising a card id already raised changes nothing.
    fn raise(&self, card: &InstallCard) -> Result<(), ConsentError>;

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

    /// Decisions made and not yet accepted by a sink.
    pub fn undelivered(&self) -> Vec<DecisionEvent> {
        self.lock().undelivered.clone()
    }

    /// Delivers every decision not yet accepted. A sink error leaves it for
    /// the next attempt.
    pub fn redeliver(&self) {
        let (sink, due) = {
            let state = self.lock();
            (state.sink.clone(), state.undelivered.clone())
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
    }
}

impl Consent for MockConsent {
    fn raise(&self, card: &InstallCard) -> Result<(), ConsentError> {
        let mut state = self.lock();
        if state.unavailable {
            return Err(ConsentError::Unavailable("the mock is set unavailable".into()));
        }
        *state.raises.entry(card.card_id.clone()).or_insert(0) += 1;
        state
            .cards
            .entry(card.card_id.clone())
            .or_insert_with(|| card.clone());
        Ok(())
    }

    fn attach(&self, sink: Arc<dyn DecisionSink>) {
        self.lock().sink = Some(sink);
        self.redeliver();
    }
}
