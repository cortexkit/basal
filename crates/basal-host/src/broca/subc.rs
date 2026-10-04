//! Broca's session-aware calls share the module's subc consumer connection.
//! A call's outcome is read with `run.result`; the session subscription only
//! wakes the host. It attaches at the live head and keeps no cursor, because
//! every wake re-reads each pending call in full: an event lost to a dropped
//! connection costs latency, never an outcome.
use super::{BrocaError, BrocaHost, Route, Transport, wire::*};
use crate::transport::{SubcTransport, WireError, map_error};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use subc_client_rs::consumer::ConnectionState;
use subc_protocol::BindIdentity;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
fn error(e: WireError) -> BrocaError {
    match e {
        WireError::Typed(refusal) => BrocaError::Flow(refusal),
        WireError::NeverSent(detail) => BrocaError::Unavailable {
            proven_unsent: true,
            detail,
        },
        WireError::Unknown(detail)
        | WireError::TimedOut(detail)
        | WireError::Unreadable(detail) => BrocaError::Unavailable {
            proven_unsent: false,
            detail,
        },
        WireError::Refused { code, message } => BrocaError::Refused {
            code,
            detail: message,
        },
    }
}
fn identity(route: &Route) -> BindIdentity {
    BindIdentity::new(&route.project_root, &route.harness, &route.session)
}
type Key = (String, String, String);
fn key(route: &Route) -> Key {
    (
        route.project_root.clone(),
        route.harness.clone(),
        route.session.clone(),
    )
}
type Wake = Arc<dyn Fn() + Send + Sync>;

/// How long a failed poll waits before trying again. A failure can leave a
/// call that no event will wake again (Broca unreachable, a run whose
/// `run.status` still says active or paused in the brief window after
/// `run.result` reports it ended, a run still owned by an activation), so a
/// failure is retried on its own rather than waiting for the next event.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);

/// Coalesce stream/reconnect notifications and poll on a blocking thread.
/// Polling from a Tokio callback would nest the consumer's blocking runtime.
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
    /// Starts the polling thread and polls once at once, which is how a
    /// module start reads every call left in flight by the last process.
    pub fn start(self: &Arc<Self>, host: &Arc<BrocaHost>) {
        let Some(receiver) = lock(&self.receiver).take() else {
            return;
        };
        let weak = Arc::downgrade(host);
        let retry = self.callback();
        std::thread::spawn(move || {
            while receiver.recv().is_ok() {
                let Some(host) = weak.upgrade() else { break };
                if let Err(error) = host.poll() {
                    tracing::warn!(%error, "Broca calls remain pending; polling again shortly");
                    drop(host);
                    std::thread::sleep(RETRY_AFTER_FAILURE);
                    retry();
                }
            }
        });
        self.notify();
    }
}

/// One session's subscription. It wakes the host when the session's run
/// finishes, and closes itself then or when the stream fails, so the next
/// `watch` opens a fresh one if the call is still pending.
#[derive(Default)]
struct Stream {
    closed: AtomicBool,
    task: Mutex<Option<tokio::task::AbortHandle>>,
}
impl Drop for Stream {
    fn drop(&mut self) {
        if let Some(task) = lock(&self.task).take() {
            task.abort();
        }
    }
}
impl Stream {
    /// Handles one subscribe event. Only a finished run wakes the host;
    /// assistant messages and display events carry nothing basal reads.
    /// Returns whether the stream is done.
    fn deliver(&self, bytes: &[u8], wake: &Wake) -> bool {
        let finished = match serde_json::from_slice::<SubscribeEvent>(bytes) {
            Ok(SubscribeEvent::Control { unit }) => {
                matches!(*unit, ControlUnit::RunFinished { .. })
            }
            Ok(SubscribeEvent::Display { .. }) => false,
            Err(error) => {
                // An event basal cannot read may be the finish it waits for.
                tracing::warn!(%error, "undecodable Broca stream event");
                true
            }
        };
        if finished {
            self.close(wake);
        }
        finished
    }
    fn close(&self, wake: &Wake) {
        self.closed.store(true, Ordering::SeqCst);
        (wake)();
    }
}

fn reconnect(streams: &Mutex<HashMap<Key, Arc<Stream>>>, wake: &Wake) {
    lock(streams).clear();
    (wake)();
}

pub struct SubcBrocaTransport {
    connection: Arc<SubcTransport>,
    module: String,
    streams: Mutex<HashMap<Key, Arc<Stream>>>,
    wake: Wake,
}
impl SubcBrocaTransport {
    pub fn new(connection: Arc<SubcTransport>, module: String, wake: Wake) -> Arc<Self> {
        let transport = Arc::new(Self {
            connection: connection.clone(),
            module,
            streams: Mutex::new(HashMap::new()),
            wake,
        });
        let weak = Arc::downgrade(&transport);
        connection.on_connection_state(move |state| {
            if matches!(state, ConnectionState::Restored { .. }) {
                if let Some(transport) = weak.upgrade() {
                    transport.reconnected();
                }
            }
        });
        transport
    }
    fn reconnected(&self) {
        // Subscriptions belong to the old daemon connection. Drop them and
        // poll: each pending call is re-read and re-watched.
        reconnect(&self.streams, &self.wake);
    }
    fn call<T: serde::de::DeserializeOwned>(
        &self,
        route: &Route,
        method: &str,
        params: &[u8],
    ) -> Result<T, BrocaError> {
        let value = match &route.flow_id {
            Some(flow) => self.connection.management_for_model(flow, identity(route), &self.module, method, params),
            None => self.connection.management_as(identity(route), &self.module, method, params),
        }.map_err(error)?;
        serde_json::from_value(value).map_err(|e| BrocaError::Wire(e.to_string()))
    }
    fn open(&self, route: &Route) -> Result<Arc<Stream>, BrocaError> {
        let bytes = serde_json::to_vec(&SubscribeParams::live())
            .map_err(|e| BrocaError::Invalid(e.to_string()))?;
        let mut subscription = match &route.flow_id {
            Some(flow) => self.connection.subscribe_for_model(flow, identity(route), &self.module, "session.subscribe", &bytes),
            None => self.connection.subscribe_as(identity(route), &self.module, "session.subscribe", &bytes),
        }.map_err(error)?;
        let stream = Arc::new(Stream::default());
        let weak = Arc::downgrade(&stream);
        let wake = self.wake.clone();
        let task = self.connection.spawn(async move {
            while let Some(bytes) = subscription.events().recv().await {
                let Some(stream) = weak.upgrade() else { return };
                if stream.deliver(&bytes, &wake) {
                    return;
                }
            }
            if let Err(e) = subscription.closed().await {
                let error = error(map_error(e));
                tracing::warn!(%error, "Broca stream closed with an error");
            }
            if let Some(stream) = weak.upgrade() {
                stream.close(&wake);
            }
        });
        *lock(&stream.task) = Some(task.abort_handle());
        Ok(stream)
    }
}
impl Transport for SubcBrocaTransport {
    fn configure_flow(&self, flow_id: &str, agent_owned: bool, scope: Option<crate::flow_scope::RegisteredScope>) {
        crate::transport::Transport::configure_flow(self.connection.as_ref(), flow_id, agent_owned, scope);
    }
    fn send(&self, route: &Route, params: &[u8]) -> Result<SendResult, BrocaError> {
        self.call(route, "session.send", params)
    }
    fn watch(&self, route: &Route) -> Result<(), BrocaError> {
        let k = key(route);
        let open = lock(&self.streams)
            .get(&k)
            .is_some_and(|stream| !stream.closed.load(Ordering::SeqCst));
        if !open {
            let stream = self.open(route)?;
            lock(&self.streams).insert(k, stream);
        }
        Ok(())
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
            "run.status",
            &serde_json::to_vec(params).map_err(|e| BrocaError::Invalid(e.to_string()))?,
        )
    }
    fn release(&self, route: &Route) {
        lock(&self.streams).remove(&key(route));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn counter() -> (Arc<AtomicUsize>, Wake) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        (
            count,
            Arc::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    #[test]
    fn only_a_finished_run_wakes_the_host_and_closes_the_stream() {
        let (wakes, wake) = counter();
        let stream = Stream::default();
        let message = br#"{"kind":"control","cursor":{"wal_seq":1,"sub_index":0},"unit":{"type":"assistant_message","message":{"message_id":"m","content":[{"type":"text","text":"answer"}]}}}"#;
        let display = br#"{"kind":"display","event":{"type":"text_delta","text":"a"}}"#;
        let finished = br#"{"kind":"control","cursor":{"wal_seq":2,"sub_index":0},"unit":{"type":"run_finished","run_id":"run","reason":"completed","usage":{"output_tokens":5}}}"#;
        assert!(!stream.deliver(message, &wake));
        assert!(!stream.deliver(display, &wake));
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        assert!(!stream.closed.load(Ordering::SeqCst));
        assert!(stream.deliver(finished, &wake));
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert!(stream.closed.load(Ordering::SeqCst));
        // An event basal cannot decode might have been the finish: wake.
        let stream = Stream::default();
        assert!(stream.deliver(b"not json", &wake));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stream_and_reconnect_wakeups_are_coalesced_without_a_clock() {
        let wake = PollWake::new();
        let callback = wake.callback();
        let finished = br#"{"kind":"control","cursor":{"wal_seq":1,"sub_index":0},"unit":{"type":"run_finished","reason":"cancelled"}}"#;
        Stream::default().deliver(finished, &callback);
        Stream::default().deliver(finished, &callback);
        let receiver = lock(&wake.receiver);
        let receiver = receiver.as_ref().unwrap();
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
        let streams = Mutex::new(HashMap::from([(
            ("/".into(), "basal".into(), "call".into()),
            Arc::new(Stream::default()),
        )]));
        reconnect(&streams, &callback);
        assert!(lock(&streams).is_empty());
        assert!(receiver.try_recv().is_ok());
    }
}
