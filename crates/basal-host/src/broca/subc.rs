//! Broca's session-aware calls share the module's subc consumer connection.
//! A call's outcome is read with `run.result`. Nothing here listens to
//! Broca: the module's event reader commits Broca's `run.ended` and
//! `run.skipped` notices and wakes `PollWake`, and each pending call's
//! scheduled check covers an event that never arrives.
use super::{BrocaError, BrocaHost, Route, Transport, wire::*};
use crate::transport::{SubcTransport, WireError};
use std::sync::{Arc, Mutex, mpsc};
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

/// Coalesces wakes from the event reader and newly scheduled checks, and
/// runs `BrocaHost::wake` on one blocking thread. Reading Broca from a Tokio
/// callback would nest the consumer's blocking runtime.
pub struct PollWake {
    sender: mpsc::SyncSender<()>,
    receiver: Mutex<Option<mpsc::Receiver<()>>>,
    /// How many passes the thread has finished, so tests can wait for a
    /// pass instead of sleeping. Test builds only.
    #[cfg(any(test, feature = "test-support"))]
    passes: Arc<(Mutex<usize>, std::sync::Condvar)>,
}
impl PollWake {
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = mpsc::sync_channel(1);
        Arc::new(Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
            #[cfg(any(test, feature = "test-support"))]
            passes: Arc::default(),
        })
    }
    /// Blocks until the thread has finished at least `count` passes, or
    /// `limit` elapses. Returns whether it had. Test builds only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_for_passes(&self, count: usize, limit: std::time::Duration) -> bool {
        let (passes, finished) = &*self.passes;
        let passes = lock(passes);
        let (passes, _) = finished
            .wait_timeout_while(passes, limit, |done| *done < count)
            .unwrap_or_else(|p| p.into_inner());
        *passes >= count
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
    /// Starts the thread. It sleeps until woken (a bus wake committed, a
    /// call newly saved as waiting) or until the next check is due, then runs
    /// `BrocaHost::wake`. With no pending call it sleeps until woken. A failed
    /// read backs off that call alone through its own `check_at`, so the
    /// thread itself never backs off. Module start reads every pending call
    /// through `BrocaHost::attach`, so the first pass here is an ordinary
    /// `wake`.
    pub fn start(self: &Arc<Self>, host: &Arc<BrocaHost>) {
        let Some(receiver) = lock(&self.receiver).take() else {
            return;
        };
        host.set_waker(self.callback());
        let weak = Arc::downgrade(host);
        #[cfg(any(test, feature = "test-support"))]
        let passes = self.passes.clone();
        std::thread::spawn(move || {
            loop {
                let Some(host) = weak.upgrade() else { break };
                if let Err(error) = host.wake() {
                    tracing::warn!(%error, "Broca calls remain pending; each retries at its own next check");
                }
                #[cfg(any(test, feature = "test-support"))]
                {
                    let (count, finished) = &*passes;
                    *lock(count) += 1;
                    finished.notify_all();
                }
                let timeout = host.next_wake_in();
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
        let value = match &route.route_key() {
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
                self.connection.configure_flow(
                    &crate::flow_scope::flow_route_key(&identity.flow_id, identity.version),
                    scope,
                );
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
    fn retain_flow_routes(&self, flow_id: &str, live: &std::collections::HashSet<String>) {
        crate::transport::Transport::retain_flow_routes(self.connection.as_ref(), flow_id, live);
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
    fn send_status(
        &self,
        route: &Route,
        params: &SendStatusParams,
    ) -> Result<serde_json::Value, BrocaError> {
        self.call(
            route,
            OP_SEND_STATUS,
            &serde_json::to_vec(params).map_err(|e| BrocaError::Invalid(e.to_string()))?,
        )
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
        if let Some(flow) = &route.route_key() {
            self.connection
                .release_model(flow, identity(route), &self.module);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
