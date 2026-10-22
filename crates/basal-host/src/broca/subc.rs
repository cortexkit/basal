//! Broca's session-aware calls share the module's subc consumer connection.
//! A call's outcome is read with `run.result`. Nothing here listens to
//! Broca: the module's event reader commits Broca's `run.ended` and
//! `run.skipped` notices and wakes `PollWake`, and each pending call's
//! scheduled check covers an event that never arrives.
use super::{BrocaError, BrocaHost, Route, Transport, wire::*};
use crate::transport::{SubcTransport, WireError};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use subc_protocol::BindIdentity;

use basal_proto::util::lock;
fn error(e: WireError) -> BrocaError {
    match e {
        WireError::Typed(refusal) => BrocaError::Flow(refusal),
        WireError::NeverSent(detail) => BrocaError::Unavailable {
            proven_unsent: true,
            detail,
        },
        WireError::Unknown(detail)
        | WireError::ScopeCut(detail)
        | WireError::TimedOut(detail)
        | WireError::Unreadable(detail) => BrocaError::Unavailable {
            proven_unsent: false,
            detail,
        },
        WireError::Refused { code, message } | WireError::RefusedDetails { code, message, .. } => {
            BrocaError::Refused {
                code,
                detail: message,
            }
        }
    }
}
fn identity(route: &Route) -> BindIdentity {
    BindIdentity::new(&route.project_root, &route.harness, &route.session)
}
type Wake = Arc<dyn Fn() + Send + Sync>;

/// How long a failed pass waits before trying again. A failure can leave a
/// call that no event will wake again (Broca unreachable, a run whose
/// `run.status` still says active or paused in the brief window after
/// `run.result` reports it ended), so a failure is retried on its own
/// rather than waiting for the next event or check.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);
const MAX_RETRY_AFTER_FAILURE: Duration = Duration::from_secs(300);

fn retry_delay(failures: u32) -> Duration {
    RETRY_AFTER_FAILURE
        .saturating_mul(
            1u32.checked_shl(failures.saturating_sub(1).min(6))
                .unwrap_or(u32::MAX),
        )
        .min(MAX_RETRY_AFTER_FAILURE)
}

/// Coalesces wakes from the event reader and newly scheduled checks, and
/// runs `BrocaHost::wake` on one blocking thread. Reading Broca from a Tokio
/// callback would nest the consumer's blocking runtime.
pub struct PollWake {
    sender: mpsc::SyncSender<()>,
    receiver: Mutex<Option<mpsc::Receiver<()>>>,
}
impl PollWake {
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = mpsc::sync_channel(1);
        Arc::new(Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
        })
    }
    pub fn notify(&self) {
        let _ = self.sender.try_send(());
    }
    pub fn callback(self: &Arc<Self>) -> Wake {
        let weak = Arc::downgrade(self);
        Arc::new(move || {
            if let Some(wake) = weak.upgrade() {
                wake.notify();
            }
        })
    }
    /// Starts the thread. Its first pass reads every pending call
    /// (`BrocaHost::poll`), which is how a module start reads every call left
    /// in flight by the last process. After that it sleeps until woken or
    /// until the next check is due, then runs `BrocaHost::wake`. With no
    /// pending call it sleeps until woken.
    pub fn start(self: &Arc<Self>, host: &Arc<BrocaHost>) {
        let Some(receiver) = lock(&self.receiver).take() else {
            return;
        };
        host.set_waker(self.callback());
        let weak = Arc::downgrade(host);
        std::thread::spawn(move || {
            let mut failures: u32 = 0;
            let mut first = true;
            loop {
                let Some(host) = weak.upgrade() else { break };
                let pass = if std::mem::take(&mut first) {
                    host.poll()
                } else {
                    host.wake()
                };
                match pass {
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        tracing::warn!(%error, failures, retry_after_s = retry_delay(failures).as_secs(), "Broca calls remain pending; backing off");
                    }
                    Ok(()) => failures = 0,
                }
                let timeout = if failures == 0 {
                    host.next_wake_in()
                } else {
                    Some(retry_delay(failures))
                };
                drop(host);
                let woken = match timeout {
                    None => receiver
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                    Some(timeout) => receiver.recv_timeout(timeout),
                };
                if matches!(woken, Err(mpsc::RecvTimeoutError::Disconnected)) {
                    break;
                }
            }
        });
    }
}

pub struct SubcBrocaTransport {
    connection: Arc<SubcTransport>,
    module: String,
    #[cfg(feature = "rig-kill-hook")]
    unscoped_send: Mutex<Option<UnscopedSendHook>>,
}
#[cfg(feature = "rig-kill-hook")]
type UnscopedSendHook = Arc<dyn Fn(&Route) -> bool + Send + Sync>;
impl SubcBrocaTransport {
    fn no_flow_scope(&self, method: &str) -> WireError {
        WireError::Typed(crate::flow_refusal::FlowRefusal::new(
            crate::flow_refusal::RefusalReason::NoFlowScope,
            &self.module,
            method,
        ))
    }
    pub fn new(connection: Arc<SubcTransport>, module: String) -> Arc<Self> {
        Arc::new(Self {
            connection,
            module,
            #[cfg(feature = "rig-kill-hook")]
            unscoped_send: Mutex::new(None),
        })
    }
    #[cfg(feature = "rig-kill-hook")]
    pub fn set_unscoped_send_hook(&self, hook: UnscopedSendHook) {
        *lock(&self.unscoped_send) = Some(hook);
    }
    fn call<T: serde::de::DeserializeOwned>(
        &self,
        route: &Route,
        method: &str,
        params: &[u8],
    ) -> Result<T, BrocaError> {
        let value = match &route.flow_id {
            Some(flow) => self.connection.management_for_model(
                flow,
                identity(route),
                &self.module,
                method,
                params,
            ),
            None => Err(self.no_flow_scope(method)),
        }
        .map_err(error)?;
        serde_json::from_value(value).map_err(|e| BrocaError::Wire(e.to_string()))
    }
}
impl Transport for SubcBrocaTransport {
    fn provider_ready(
        &self,
        flow_id: &str,
        action: &str,
    ) -> Result<(), crate::flow_refusal::FlowRefusal> {
        crate::transport::Transport::provider_ready(
            self.connection.as_ref(),
            flow_id,
            &self.module,
            action,
        )
    }
    fn refresh_flow(&self, identity: &super::FlowIdentity) -> Result<(), BrocaError> {
        use crate::transport::Transport;
        let value = self
            .connection
            .management(
                crate::subc_catalog::CORE,
                "flow.install_status",
                serde_json::json!({"flow_id":identity.flow_id,"version":identity.version}),
            )
            .map_err(error)?;
        match crate::core_host::decode_install_status(&value).map_err(BrocaError::Wire)? {
            crate::InstallStatus::Active { code_hash, scope }
                if code_hash == identity.code_hash =>
            {
                self.connection.configure_flow(&identity.flow_id, scope);
                Ok(())
            }
            _ => Err(BrocaError::Unavailable {
                proven_unsent: false,
                detail: "core no longer stands behind the accepted model call".into(),
            }),
        }
    }
    fn configure_flow(&self, flow_id: &str, scope: Option<crate::flow_scope::RegisteredScope>) {
        crate::transport::Transport::configure_flow(self.connection.as_ref(), flow_id, scope);
    }
    fn send(&self, route: &Route, params: &[u8]) -> Result<SendResult, BrocaError> {
        #[cfg(feature = "rig-kill-hook")]
        if lock(&self.unscoped_send)
            .as_ref()
            .is_some_and(|hook| hook(route))
        {
            let value = self
                .connection
                .unscoped_model_send(identity(route), &self.module, params)
                .map_err(error)?;
            return serde_json::from_value(value).map_err(|e| BrocaError::Wire(e.to_string()));
        }
        self.call(route, OP_SESSION_SEND, params)
    }
    fn event_get(
        &self,
        route: &Route,
        params: &RunEventGetParams,
    ) -> Result<RunEventReply, BrocaError> {
        self.call(
            route,
            OP_RUN_EVENT_GET,
            &serde_json::to_vec(params).map_err(|e| BrocaError::Invalid(e.to_string()))?,
        )
    }
    fn result(
        &self,
        route: &Route,
        params: &RunResultParams,
    ) -> Result<RunResultResponse, BrocaError> {
        self.call(
            route,
            OP_RUN_RESULT,
            &serde_json::to_vec(params).map_err(|e| BrocaError::Invalid(e.to_string()))?,
        )
    }
    fn status(
        &self,
        route: &Route,
        params: &StatusParams,
    ) -> Result<RunStatusResponse, BrocaError> {
        self.call(
            route,
            OP_RUN_STATUS,
            &serde_json::to_vec(params).map_err(|e| BrocaError::Invalid(e.to_string()))?,
        )
    }
    fn release(&self, route: &Route) {
        if let Some(flow) = &route.flow_id {
            self.connection
                .release_model(flow, identity(route), &self.module);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_passes_back_off_exponentially_to_a_cap() {
        assert_eq!(retry_delay(1), Duration::from_secs(5));
        assert_eq!(retry_delay(2), Duration::from_secs(10));
        assert_eq!(retry_delay(3), Duration::from_secs(20));
        assert_eq!(retry_delay(100), Duration::from_secs(300));
    }

    #[test]
    fn wakeups_are_coalesced_without_a_clock() {
        let wake = PollWake::new();
        let callback = wake.callback();
        callback();
        callback();
        let receiver = lock(&wake.receiver);
        let receiver = receiver.as_ref().unwrap();
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
    }
}
