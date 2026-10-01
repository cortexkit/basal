//! Broca's session-aware calls share the module's subc consumer connection.
//! Control events remain buffered until the host supplies a cursor saved in
//! SQLite, so a failed snapshot write cannot lose an already received event.
use super::{BrocaError, BrocaHost, Route, Transport, wire::*};
use crate::transport::{SubcTransport, WireError, map_error};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, mpsc};
use subc_client_rs::consumer::ConnectionState;
use subc_protocol::BindIdentity;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
fn error(e: WireError) -> BrocaError {
    match e {
        WireError::NeverSent(detail) => BrocaError::Unavailable {
            proven_unsent: true,
            detail,
        },
        WireError::Unknown(detail) => BrocaError::Unavailable {
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
    pub fn start(&self, host: &Arc<BrocaHost>) {
        let Some(receiver) = lock(&self.receiver).take() else {
            return;
        };
        let weak = Arc::downgrade(host);
        std::thread::spawn(move || {
            while receiver.recv().is_ok() {
                let Some(host) = weak.upgrade() else { break };
                if let Err(error) = host.poll() {
                    tracing::error!(%error,"Broca stream recovery remains pending");
                }
            }
        });
    }
}
#[derive(Default)]
struct BufferState {
    events: VecDeque<SubscribeEvent>,
    failure: Option<BrocaError>,
    closed: bool,
}
#[derive(Default)]
struct Buffer {
    state: Mutex<BufferState>,
    task: Mutex<Option<tokio::task::AbortHandle>>,
}
impl Drop for Buffer {
    fn drop(&mut self) {
        if let Some(task) = lock(&self.task).take() {
            task.abort();
        }
    }
}
impl Buffer {
    fn push(&self, bytes: &[u8]) -> Result<bool, BrocaError> {
        let event: SubscribeEvent =
            serde_json::from_slice(bytes).map_err(|e| BrocaError::Wire(e.to_string()))?;
        let terminal = matches!(&event,SubscribeEvent::Control {unit,..} if matches!(unit.as_ref(),ControlUnit::RunFinished {..}));
        // Display events are transient UI updates, not stored assistant
        // messages. Only control events may advance the saved cursor.
        if matches!(event, SubscribeEvent::Control { .. }) {
            lock(&self.state).events.push_back(event);
        }
        Ok(terminal)
    }
    fn deliver(&self, bytes: &[u8], wake: &Wake) -> Result<bool, BrocaError> {
        let terminal = self.push(bytes)?;
        (wake)();
        Ok(terminal)
    }
    fn close(&self, failure: Option<BrocaError>) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.failure = failure;
    }
    fn batch(&self, from: &Option<FromWire>) -> Result<Vec<SubscribeEvent>, BrocaError> {
        let mut state = lock(&self.state);
        if let Some(FromWire::Cursor(cursor)) = from {
            state
                .events
                .retain(|event| matches!(event,SubscribeEvent::Control {cursor:c,..} if c>cursor));
        }
        // Do not drain on delivery: a failed snapshot commit must receive the
        // same events again. Only a subsequent durable cursor releases them.
        if !state.events.is_empty() {
            return Ok(state.events.iter().cloned().collect());
        }
        if let Some(error) = state.failure.take() {
            return Err(error);
        }
        Ok(vec![])
    }
}

fn reconnect(streams: &Mutex<HashMap<Key, Arc<Buffer>>>, wake: &Wake) {
    lock(streams).clear();
    (wake)();
}

pub struct SubcBrocaTransport {
    connection: Arc<SubcTransport>,
    module: String,
    streams: Mutex<HashMap<Key, Arc<Buffer>>>,
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
        // Subscriptions belong to the old daemon connection. Replace them on
        // reconnect and resume from SQLite, not an in-memory receive position.
        reconnect(&self.streams, &self.wake);
    }
    fn call<T: serde::de::DeserializeOwned>(
        &self,
        route: &Route,
        method: &str,
        params: &[u8],
    ) -> Result<T, BrocaError> {
        let value = self
            .connection
            .management_as(identity(route), &self.module, method, params)
            .map_err(error)?;
        serde_json::from_value(value).map_err(|e| BrocaError::Wire(e.to_string()))
    }
    fn open(&self, route: &Route, params: &SubscribeParams) -> Result<Arc<Buffer>, BrocaError> {
        let bytes = serde_json::to_vec(params).map_err(|e| BrocaError::Invalid(e.to_string()))?;
        let mut subscription = self
            .connection
            .subscribe_as(identity(route), &self.module, "session.subscribe", &bytes)
            .map_err(error)?;
        let buffer = Arc::new(Buffer::default());
        let weak = Arc::downgrade(&buffer);
        let wake = self.wake.clone();
        let task = self.connection.spawn(async move {
            while let Some(bytes) = subscription.events().recv().await {
                let Some(buffer) = weak.upgrade() else { return };
                match buffer.deliver(&bytes, &wake) {
                    Ok(terminal) => {
                        if terminal {
                            buffer.close(None);
                            return;
                        }
                    }
                    Err(error) => {
                        buffer.close(Some(error));
                        wake();
                        return;
                    }
                }
            }
            let failure = match subscription.closed().await {
                Ok(()) => BrocaError::Unavailable {
                    proven_unsent: false,
                    detail: "Broca stream ended before a terminal event".into(),
                },
                Err(e) => error(map_error(e)),
            };
            if let Some(buffer) = weak.upgrade() {
                buffer.close(Some(failure));
                wake();
            }
        });
        *lock(&buffer.task) = Some(task.abort_handle());
        Ok(buffer)
    }
}
impl Transport for SubcBrocaTransport {
    fn send(&self, route: &Route, params: &[u8]) -> Result<SendResult, BrocaError> {
        self.call(route, "session.send", params)
    }
    fn subscribe(
        &self,
        route: &Route,
        params: &SubscribeParams,
    ) -> Result<Vec<SubscribeEvent>, BrocaError> {
        let k = key(route);
        let cached = lock(&self.streams).get(&k).cloned();
        let buffer = match cached {
            Some(buffer) => buffer,
            None => {
                let buffer = self.open(route, params)?;
                lock(&self.streams).insert(k.clone(), buffer.clone());
                buffer
            }
        };
        let result = buffer.batch(&params.from);
        if result.is_err() {
            let mut streams = lock(&self.streams);
            if streams
                .get(&k)
                .is_some_and(|current| Arc::ptr_eq(current, &buffer))
            {
                streams.remove(&k);
            }
            drop(streams);
            let recoverable = match &result {
                Err(BrocaError::Unavailable { .. }) => true,
                Err(BrocaError::Refused { code, .. }) => code == "cursor_expired",
                _ => false,
            };
            if recoverable {
                (self.wake)();
            }
        }
        result
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
    fn read(&self, route: &Route, params: &ReadParams) -> Result<SessionReadResponse, BrocaError> {
        self.call(
            route,
            "session.read",
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
    use serde_json::json;
    #[test]
    fn stream_batches_replay_until_a_durable_cursor_and_keep_error_order() {
        let buffer = Buffer::default();
        let first = json!({"kind":"control","cursor":{"wal_seq":1,"sub_index":0},"unit":{"type":"assistant_message","message":{"message_id":"m","content":[{"type":"text","text":"answer"}]}}});
        let second = json!({"kind":"control","cursor":{"wal_seq":2,"sub_index":0},"unit":{"type":"run_finished","run_id":"run","reason":"completed"}});
        assert!(!buffer.push(first.to_string().as_bytes()).unwrap());
        assert!(buffer.push(second.to_string().as_bytes()).unwrap());
        buffer.close(Some(BrocaError::Refused {
            code: "cursor_expired".into(),
            detail: "gone".into(),
        }));
        let initial = buffer.batch(&None).unwrap();
        assert_eq!(initial.len(), 2);
        assert_eq!(buffer.batch(&None).unwrap(), initial);
        let after = buffer
            .batch(&Some(FromWire::Cursor(Cursor {
                wal_seq: 1,
                sub_index: 0,
            })))
            .unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0], initial[1]);
        assert!(
            matches!(buffer.batch(&Some(FromWire::Cursor(Cursor {wal_seq:2,sub_index:0}))),Err(BrocaError::Refused {code,..}) if code=="cursor_expired")
        );
        assert!(buffer.push(b"not json").is_err());
    }
    #[test]
    fn stream_and_reconnect_wakeups_are_coalesced_without_a_clock() {
        let wake = PollWake::new();
        let callback = wake.callback();
        let buffer = Buffer::default();
        let event=br#"{"kind":"control","cursor":{"wal_seq":1,"sub_index":0},"unit":{"type":"future_control"}}"#;
        buffer.deliver(event, &callback).unwrap();
        buffer.deliver(event, &callback).unwrap();
        let receiver = lock(&wake.receiver);
        let receiver = receiver.as_ref().unwrap();
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
        let streams = Mutex::new(HashMap::from([(
            ("/".into(), "basal".into(), "call".into()),
            Arc::new(Buffer::default()),
        )]));
        reconnect(&streams, &callback);
        assert!(lock(&streams).is_empty());
        assert!(receiver.try_recv().is_ok());
    }
}
