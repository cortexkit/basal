//! Decision cards use the catalogued consent provider; install cards stay on
//! prefrontal-core. A stored endpoint owns updates, withdrawals and answers even
//! if the daemon catalog later changes.
use crate::core_consent::{CoreConsent, decision_request, error};
use crate::subc_catalog::CORE;
use crate::transport::Transport;
use crate::{Consent, ConsentError, DecisionCard, DecisionPath, DecisionSink, InstallCard};
use serde_json::json;
use std::sync::Arc;

pub struct CingulateConsent {
    transport: Arc<dyn Transport>,
    core: CoreConsent,
}

impl CingulateConsent {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            core: CoreConsent::for_catalog(transport.clone()),
            transport,
        }
    }

    pub fn with_polling(mut self) -> Self {
        self.core = self.core.with_polling();
        self
    }

    /// Read and apply settlement pages immediately, without a polling timer.
    /// Saved providers remain pollable when absent from the daemon catalog.
    pub fn poll_once(&self) -> Result<(), ConsentError> {
        self.core.poll_once()
    }
}

impl Consent for CingulateConsent {
    fn raise(&self, card: &InstallCard) -> Result<(), ConsentError> {
        self.core.raise(card)
    }

    fn decision_path(&self) -> Result<DecisionPath, ConsentError> {
        let catalog = self.transport.catalog().map_err(error)?;
        let modules = catalog["modules"]
            .as_array()
            .ok_or_else(|| ConsentError::Unavailable("catalog modules missing".into()))?;
        for module in modules {
            if module["capabilities"]["provides"]
                .as_array()
                .is_some_and(|caps| caps.iter().any(|c| c == "consent/v1"))
            {
                let provider = module["module_id"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        ConsentError::Unavailable("consent provider identity missing".into())
                    })?;
                return Ok(DecisionPath::Consent(provider.to_owned()));
            }
        }
        Ok(DecisionPath::Legacy)
    }

    fn raise_decision(&self, card: &DecisionCard) -> Result<String, ConsentError> {
        self.raise_decision_on(&self.decision_path()?, card)
    }

    fn raise_decision_on(
        &self,
        path: &DecisionPath,
        card: &DecisionCard,
    ) -> Result<String, ConsentError> {
        let (provider, method) = match path {
            DecisionPath::Legacy => (CORE, "elicitation.request"),
            DecisionPath::Consent(provider) => (provider.as_str(), "consent.request"),
        };
        // consent.request checks only the envelope. Keep decision_request's full
        // core v2 body validation even though Cingulate stores the body opaquely.
        let request = decision_request(card)?;
        // The engine records the decision's endpoint before calling this method.
        // Wake the shared answer poller before sending: a lost reply must not
        // leave those durable pending cards unpolled.
        self.core.raised();
        let reply = self
            .transport
            .management(provider, method, request)
            .map_err(error)?;
        let id = reply["elicitation_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ConsentError::Unavailable("missing elicitation id".into()))?
            .to_owned();
        Ok(id)
    }

    fn withdraw_decision_on(&self, path: &DecisionPath, id: &str) -> Result<(), ConsentError> {
        let (provider, method) = match path {
            DecisionPath::Legacy => (CORE, "elicitation.withdraw"),
            DecisionPath::Consent(provider) => (provider.as_str(), "consent.withdraw"),
        };
        let reply = self
            .transport
            .management(provider, method, json!({"elicitation_id":id}))
            .map_err(error)?;
        if !matches!(
            reply["state"].as_str(),
            Some("withdrawn" | "answered" | "expired")
        ) {
            return Err(ConsentError::Unavailable(
                "withdrawal returned no settled card".into(),
            ));
        }
        Ok(())
    }

    fn attach(&self, sink: Arc<dyn DecisionSink>) {
        self.core.attach(sink);
    }
}

#[cfg(test)]
#[path = "cingulate_consent_tests.rs"]
mod tests;
