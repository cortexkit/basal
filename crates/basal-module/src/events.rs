//! Read the JetStream pull consumer `m_basal`, which ck-bus provisions on the
//! event stream. This reader never creates or modifies the stream or consumer.
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use async_nats::jetstream::{self, consumer::PullConsumer};
use basal_core::{
    Runtime,
    events::{Notice, Report, parse_subject, parse_subject_identity},
};
use basal_host::{Catalog, Consent, transport::Transport};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cortexkit_bus_naming::AccountNames;
use cortexkit_bus_nats::{ConnectConfig, NatsConnection, NatsConnectionState};
use cortexkit_bus_trait::{ContentDigest, Stream};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const BATCH: usize = 32;
const PULL_WAIT: Duration = Duration::from_secs(60);
const BACKLOG_REFRESH: Duration = Duration::from_secs(60);
const DURABLE: &str = "m_basal";
// cortexkit-bus-nats reserves these for the transport id and SHA-256 body digest.
// Its private codec does not export their names, so batch decoding names them here.
const MESSAGE_ID: &str = "Nats-Msg-Id";
const CONTENT_DIGEST: &str = "Ck-Content-Digest";
const EVENT_KEY: &str = "event_key";

#[derive(Debug, Clone, Default, Serialize)]
pub struct Health {
    pub skipped_before_install: u64,
    pub filtered_out: u64,
    pub stale_dropped: u64,
    pub connected: bool,
    /// When the reader last went from connected to not connected, in Unix
    /// milliseconds; `None` while connected. The planned reconnect before each
    /// bus credential expires is down for well under a second, so diagnostics
    /// use this to tell that apart from an outage.
    pub disconnected_since_ms: Option<i64>,
    pub error: Option<String>,
    pub retry_in_ms: u64,
    /// Estimated pending plus delivered-but-unacknowledged notices: sampled
    /// before a nonempty batch or every 60 seconds at idle, then reduced by
    /// successfully committed/acknowledged notices. Poll failures retain the
    /// estimate; arrivals between polls can make it stale. Not a liveness probe.
    pub backlog: u64,
    pub batches: u64,
    pub notices: u64,
    pub admitted: u64,
    pub queued: u64,
    pub overflow: u64,
    /// Wakes committed for pending Broca model calls. Staying at zero while
    /// model calls run means Broca's run events are not reaching basal, and
    /// calls settle only at their scheduled checks.
    pub model_wakes: u64,
    pub malformed: u64,
    pub first_batch_notices: Option<usize>,
    /// Processing time after the first notices arrive, including the backlog
    /// poll and durable commit, but not the preceding idle pull wait.
    pub first_batch_elapsed_ms: Option<u64>,
    pub first_batch_oldest_age_ms: Option<i64>,
    /// The last lifecycle-event publish failure, cleared by the next pass that
    /// publishes without error. Publishing never stops event delivery.
    pub outbox_error: Option<String>,
    #[cfg(any(test, feature = "test-support"))]
    #[serde(skip)]
    pub outbox_passes_for_test: u64,
    #[cfg(any(test, feature = "test-support"))]
    #[serde(skip)]
    pub pull_requests_for_test: u64,
    #[cfg(any(test, feature = "test-support"))]
    #[serde(skip)]
    pub backlog_polls_for_test: u64,
    #[cfg(any(test, feature = "test-support"))]
    #[serde(skip)]
    pub disconnect_wakes_for_test: u64,
}

impl Health {
    /// Record the reader as not connected, keeping the start of an outage that
    /// is already under way.
    fn mark_down(&mut self) {
        self.connected = false;
        self.disconnected_since_ms.get_or_insert_with(now_ms);
    }
}

pub struct Reader {
    health: Arc<Mutex<Health>>,
    stop: Arc<AtomicBool>,
    cancel: Arc<tokio::sync::Notify>,
    thread: Option<std::thread::JoinHandle<()>>,
}

// Clear connection health during unwinding as well as ordinary reader exit,
// before the supervisor has a chance to observe the stopped reader.
struct ReaderStopped(Arc<Mutex<Health>>);

impl Drop for ReaderStopped {
    fn drop(&mut self) {
        let mut h = self.0.lock().unwrap_or_else(|p| p.into_inner());
        h.mark_down();
        h.error = Some("reader stopped".into());
    }
}

fn supervise(health: Arc<Mutex<Health>>, stop: Arc<AtomicBool>, mut run: impl FnMut()) {
    let mut delay = 250u64;
    while !stop.load(Ordering::Acquire) {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _stopped = ReaderStopped(health.clone());
            run();
        }));
        let reason = match result {
            Ok(()) => "reader stopped: loop exited".into(),
            Err(panic) => {
                let reason = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic");
                format!("reader stopped: panic: {reason}")
            }
        };
        {
            let mut h = health.lock().unwrap_or_else(|p| p.into_inner());
            h.mark_down();
            h.error = Some(reason);
            h.retry_in_ms = delay;
        }
        let until = Instant::now() + Duration::from_millis(delay);
        while !stop.load(Ordering::Acquire) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(25));
        }
        delay = (delay * 2).min(30_000);
    }
}

async fn until_cancelled(cancel: &tokio::sync::Notify, work: impl Future<Output = ()>) {
    // Synchronous SQLite work finishes its poll (and transaction) before this
    // select can observe shutdown. Dropping an async bus wait cannot cut it.
    tokio::select! {
        biased;
        _ = cancel.notified() => {},
        _ = work => {},
    }
}

async fn until_disconnected(state: &mut tokio::sync::watch::Receiver<NatsConnectionState>) {
    // Check the current value as well as later changes: a disconnect during
    // setup must not be lost before the session starts waiting. Unknown future
    // states and a closed watch also end the session rather than claiming health.
    let _ = state
        .wait_for(|state| !matches!(state, NatsConnectionState::Connected))
        .await;
}

impl Reader {
    pub fn start(rt: Runtime, transport: Arc<dyn Transport>) -> Self {
        Self::start_with_custody(rt, transport, None, None)
    }

    pub fn start_with_custody(
        rt: Runtime,
        transport: Arc<dyn Transport>,
        custody: Option<basal_core::codemode::supervisor::Supervisor>,
        consent: Option<Arc<dyn Consent>>,
    ) -> Self {
        let health = Arc::new(Mutex::new(Health::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(tokio::sync::Notify::new());
        let thread_health = health.clone();
        let thread_stop = stop.clone();
        let thread_cancel = cancel.clone();
        let thread = std::thread::spawn(move || {
            let _stopped = ReaderStopped(thread_health.clone());
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("event reader runtime");
            supervise(thread_health.clone(), thread_stop.clone(), || {
                runtime.block_on(until_cancelled(
                    &thread_cancel,
                    reader_loop(
                        rt.clone(),
                        custody.clone(),
                        consent.clone(),
                        transport.clone(),
                        thread_health.clone(),
                        thread_stop.clone(),
                    ),
                ));
            });
        });
        Self {
            health,
            stop,
            cancel,
            thread: Some(thread),
        }
    }

    pub fn health(&self) -> Health {
        self.health
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.cancel.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn now_ms() -> i64 {
    i64::try_from(basal_proto::util::unix_millis()).unwrap_or(i64::MAX)
}

#[derive(Clone, Deserialize)]
struct Credential {
    jwt: String,
    exp: i64,
    acct: String,
    inbox_prefix: String,
    server_url: String,
    credential_public: String,
    spawn_generation: u64,
    credential_epoch: u64,
}

impl Credential {
    fn validate(&self) -> Result<(), String> {
        AccountNames::derive(&self.acct).map_err(|e| e.to_string())?;
        let config = ConnectConfig::new(&self.credential_public).map_err(|e| e.to_string())?;
        let port = self
            .server_url
            .strip_prefix("nats://127.0.0.1:")
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|p| *p > 0);
        if port.is_none()
            || self.exp <= now_ms() / 1000
            || config.inbox_prefix() != self.inbox_prefix
        {
            return Err("invalid or expired ckbus credential".into());
        }
        Ok(())
    }
}

async fn credential_call(
    transport: Arc<dyn Transport>,
    method: &'static str,
    params: Value,
) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || {
        transport
            .management("ckbus", method, params)
            .map_err(|e| format!("ckbus {method}: {e:?}"))
    })
    .await
    .map_err(|e| format!("reader stopped: credential worker {e}"))?
}

async fn credential(
    transport: Arc<dyn Transport>,
    prior: Option<&Credential>,
) -> Result<Credential, String> {
    let value = if let Some(prior) = prior {
        match credential_call(
            transport.clone(),
            "ckbus.credential_renew",
            json!({"credential_public":prior.credential_public}),
        )
        .await
        {
            Ok(value) => {
                if value["credential_public"] != prior.credential_public
                    || value["spawn_generation"].as_u64() != Some(prior.spawn_generation)
                    || value["credential_epoch"].as_u64() != Some(prior.credential_epoch)
                {
                    return Err("ckbus renewal changed credential identity".into());
                }
                let mut renewed = prior.clone();
                renewed.jwt = value["jwt"].as_str().ok_or("renewal JWT missing")?.into();
                renewed.exp = value["exp"]
                    .as_i64()
                    .filter(|exp| *exp > prior.exp)
                    .ok_or("renewal did not extend expiry")?;
                renewed.validate()?;
                return Ok(renewed);
            }
            Err(e)
                if e.contains("ckbus_credential_superseded")
                    || e.contains("ckbus_credential_revoked") =>
            {
                credential_call(transport, "ckbus.credential", json!({})).await?
            }
            Err(e) => return Err(e),
        }
    } else {
        credential_call(transport, "ckbus.credential", json!({})).await?
    };
    let credential: Credential = serde_json::from_value(value).map_err(|e| e.to_string())?;
    credential.validate()?;
    Ok(credential)
}

async fn connect(
    transport: Arc<dyn Transport>,
    credential: &Credential,
) -> Result<NatsConnection, String> {
    let key = credential.credential_public.clone();
    let jwt = credential.jwt.clone();
    let options = async_nats::ConnectOptions::with_auth_callback(move |nonce| {
        let transport = transport.clone();
        let key = key.clone();
        let jwt = jwt.clone();
        async move {
            let value = credential_call(
                transport,
                "ckbus.nonce_sign",
                json!({"credential_public":key,"nonce_b64":STANDARD.encode(nonce)}),
            )
            .await
            .map_err(async_nats::AuthError::new)?;
            let signature = value["signature_b64"]
                .as_str()
                .ok_or_else(|| async_nats::AuthError::new("signature missing"))?;
            let signature = STANDARD
                .decode(signature)
                .map_err(|e| async_nats::AuthError::new(e.to_string()))?;
            if signature.len() != 64 {
                return Err(async_nats::AuthError::new("signature must be 64 bytes"));
            }
            let mut auth = async_nats::Auth::new();
            auth.jwt = Some(jwt);
            auth.signature = Some(signature);
            Ok(auth)
        }
    })
    // The reader session owns reconnects for both intake and publishing.
    // Disable client reconnects so replacing a disconnected session cannot
    // leave another connection retrying with its old credential.
    .max_reconnects(0)
    .connection_timeout(Duration::from_secs(2));
    NatsConnection::connect(
        &credential.server_url,
        options,
        ConnectConfig::new(&credential.credential_public).map_err(|e| e.to_string())?,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Only bind the pre-existing durable. Basal has no authority to provision or
/// change a named consumer, including when the stream or durable is absent.
pub async fn bind(connection: &NatsConnection, stream: &str) -> Result<PullConsumer, String> {
    jetstream::new(connection.client())
        .get_consumer_from_stream(DURABLE, stream)
        .await
        .map_err(|e| e.to_string())
}

pub fn decode(message: &jetstream::Message, catalog: &dyn Catalog) -> Result<Notice, String> {
    // Flow bodies must be fetched under the flow's own scope so the publisher
    // can check access; an inline body would bypass that check for subscribers.
    if !message.payload.is_empty() {
        return Err("event notice carries a body".into());
    }
    decode_metadata(message, catalog, false)
}

fn decode_metadata(
    message: &jetstream::Message,
    catalog: &dyn Catalog,
    inline_control: bool,
) -> Result<Notice, String> {
    let headers = message.headers.as_ref().ok_or("notice headers absent")?;
    let get = |name: &str| {
        headers
            .get_last(name)
            .map(|v| v.as_str().to_owned())
            .ok_or_else(|| format!("notice {name} absent"))
    };
    // Control bodies are for basal itself, not subscribing flows. Inline bodies
    // need no digest, but a supplied digest must match; empty claim-check notices
    // still require their digest to identify the publisher's stored body.
    let digest = if inline_control && !message.payload.is_empty() {
        get(CONTENT_DIGEST)
            .unwrap_or_else(|_| ContentDigest::of_bytes(&message.payload).to_string())
    } else {
        get(CONTENT_DIGEST)?
    };
    // Notices accept bare lowercase hex or `sha256:<lowercase hex>`, as
    // described in docs/script.md. Retain the exact notice spelling here;
    // body verification strips only that prefix and requires bare reply hex.
    let hex = basal_core::events::digest_hex(&digest).ok_or("invalid notice digest")?;
    hex.parse::<ContentDigest>().map_err(str::to_owned)?;
    let mut labels = BTreeMap::new();
    for (name, values) in headers.iter() {
        let name: &str = name.as_ref();
        if !name.eq_ignore_ascii_case(MESSAGE_ID)
            && !name.eq_ignore_ascii_case(CONTENT_DIGEST)
            && let Some(value) = values.last()
        {
            labels.insert(name.to_owned(), value.as_str().to_owned());
        }
    }
    let published_at_ms = i64::try_from(
        message
            .info()
            .map_err(|e| e.to_string())?
            .published
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .map_err(|e| e.to_string())?;
    let mut notice = Notice {
        subject: message.subject.to_string(),
        event_key: get(MESSAGE_ID)?,
        digest,
        headers: labels,
        published_at_ms,
    };
    let (_, _, version) = notice.subject_identity().map_err(|e| e.to_string())?;
    // Versions beyond v1 use distinct transport ids so stream-wide
    // deduplication does not suppress another version of the same event.
    // Only the bare key in the header can resolve the stored body.
    if version != 1 {
        notice.event_key = get(EVENT_KEY)?;
        if get(MESSAGE_ID)? != format!("{}.v{version}", notice.event_key) {
            return Err("versioned notice transport id does not match event_key".into());
        }
    }
    let (module, name, version) = notice.identity().map_err(|e| e.to_string())?;
    if version != 1 && catalog.event(module, name, version).is_none() {
        return Err("undeclared event version".into());
    }
    notice.target_flow().map_err(|e| e.to_string())?;
    Ok(notice)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallChange {
    flow_id: String,
    #[serde(rename = "seq")]
    _seq: std::num::NonZeroU64,
}

// A late-result wake only says "read the feed": the result itself comes from
// `late_results`. The tool-provider crate's LateResultReady decoder ignores
// unknown members; neither the call key nor the outcome settles a call without
// fetching its feed entry.
use cortexkit_role_tool_provider::LateResultReady;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CardSettled {
    #[serde(rename = "elicitation_id")]
    _elicitation_id: String,
    #[serde(rename = "feed")]
    _feed: AnswerFeed,
    #[serde(rename = "seq")]
    _seq: std::num::NonZeroU64,
}

#[derive(Deserialize)]
enum AnswerFeed {
    #[serde(rename = "answers")]
    Answers,
}

fn control_payload<T: serde::de::DeserializeOwned>(
    message: &jetstream::Message,
    catalog: &dyn Catalog,
) -> Result<T, String> {
    let notice = decode_metadata(message, catalog, true)?;
    let digest = basal_core::events::digest_hex(&notice.digest).ok_or("invalid notice digest")?;
    if ContentDigest::of_bytes(&message.payload).to_string() != digest {
        return Err("control notice digest mismatch".into());
    }
    serde_json::from_slice(&message.payload).map_err(|_| "invalid control payload".into())
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_control_payload(
    message: &jetstream::Message,
    catalog: &dyn Catalog,
) -> Result<Value, String> {
    control_payload(message, catalog)
}

fn malformed_notice_could_matter(rt: &Runtime, identity: Option<(&str, &str, u32)>) -> bool {
    let Some((module, name, _version)) = identity else {
        return false;
    };
    if module == "broca" && matches!(name, "run.ended" | "run.skipped") {
        return true;
    }
    // A failed lookup is treated as potentially relevant so a store problem
    // cannot hide a malformed notice from an installed event consumer.
    rt.has_enabled_event_subscription(module, name)
        .unwrap_or(true)
}

pub async fn pull(consumer: &PullConsumer) -> Result<Vec<jetstream::Message>, String> {
    let mut stream = consumer
        .fetch()
        .max_messages(BATCH)
        .expires(Duration::from_secs(1))
        .messages()
        .await
        .map_err(|e| e.to_string())?;
    let mut messages = Vec::new();
    while let Some(message) = stream.next().await {
        messages.push(message.map_err(|e| e.to_string())?);
    }
    Ok(messages)
}

/// Commit all valid notices in one store transaction. Every five seconds while
/// SQLite is busy, tell JetStream the batch is still in progress, extending its
/// redelivery timer. Acknowledge completion only after the transaction commits.
/// `acknowledge=false` leaves committed messages unacknowledged so a test can
/// restart the runtime as if the process crashed between commit and final ack.
pub async fn commit_batch(
    rt: Runtime,
    messages: &[jetstream::Message],
    acknowledge: bool,
) -> Result<Report, String> {
    commit_batch_report(rt, None, None, messages, acknowledge)
        .await
        .map(|(report, _)| report)
}

#[cfg(any(test, feature = "test-support"))]
pub async fn commit_batch_with_custody(
    rt: Runtime,
    custody: basal_core::codemode::supervisor::Supervisor,
    messages: &[jetstream::Message],
    acknowledge: bool,
) -> Result<Report, String> {
    commit_batch_report(rt, Some(custody), None, messages, acknowledge)
        .await
        .map(|(report, _)| report)
}

#[cfg(any(test, feature = "test-support"))]
pub async fn commit_batch_with_consent(
    rt: Runtime,
    consent: Arc<dyn Consent>,
    messages: &[jetstream::Message],
    acknowledge: bool,
) -> Result<(Report, usize), String> {
    commit_batch_report(rt, None, Some(consent), messages, acknowledge).await
}

async fn commit_batch_report(
    rt: Runtime,
    custody: Option<basal_core::codemode::supervisor::Supervisor>,
    consent: Option<Arc<dyn Consent>>,
    messages: &[jetstream::Message],
    acknowledge: bool,
) -> Result<(Report, usize), String> {
    let owned_messages = messages.to_vec();
    // Decoding consults the synchronous daemon catalog. Keep it with SQLite
    // admission on the blocking pool, while the async task sends progress acks.
    let mut commit = tokio::task::spawn_blocking(move || {
        let mut notices = Vec::new();
        let mut malformed = 0;
        let mut card_settled = false;
        for m in &owned_messages {
            if let Ok(subject) = parse_subject(&m.subject) {
                if let Some(reader) = subject.reader.as_deref() {
                    if reader == "basal"
                        && subject.event == "late_result_ready"
                        && subject.version == 1
                    {
                        match control_payload::<LateResultReady>(m, rt.catalog()) {
                            Ok(_) => {
                                if let Some(custody) = &custody {
                                    custody.late_result_ready(&subject.module);
                                }
                            }
                            Err(reason) => {
                                malformed += 1;
                                tracing::warn!(target:"events", subject=%m.subject, %reason, "refusing malformed late-result wake before acknowledgement");
                            }
                        }
                    }
                    if reader == "basal"
                        && subject.module == "cingulate"
                        && subject.event == "card_settled"
                        && subject.version == 1
                    {
                        match control_payload::<CardSettled>(m, rt.catalog()) {
                            Ok(_) => card_settled = true,
                            Err(reason) => {
                                malformed += 1;
                                tracing::warn!(target:"events", subject=%m.subject, %reason, "refusing malformed card-settled wake before acknowledgement");
                            }
                        }
                    }
                    continue;
                }
                // Basal's own body-carrying publications (owner notices and
                // attribution facts) also arrive on its durable. They are
                // messages for prefrontal and astrocyte, not flow triggers, so
                // acknowledge them without counting them as malformed notices.
                if subject.module == "basal"
                    && subject.version == 1
                    && matches!(
                        subject.event.as_str(),
                        "flow_owner_notice" | "attribution_fact"
                    )
                {
                    continue;
                }
                if basal_host::core_identity::is_core_module(&subject.module)
                    && subject.event == "flow_install_changed"
                    && subject.version == 1
                {
                    match control_payload::<InstallChange>(m, rt.catalog()).and_then(|change| {
                        if change.flow_id.is_empty() {
                            Err("install change lacks flow_id".into())
                        } else {
                            Ok(change)
                        }
                    }) {
                        Ok(change) => rt
                            .install_changed(&change.flow_id)
                            .map_err(|e| e.to_string())?,
                        Err(reason) => {
                            malformed += 1;
                            tracing::warn!(target: "events", subject=%m.subject, %reason, "refusing malformed install change before acknowledgement");
                        }
                    }
                    continue;
                }
            }
            // Parse the subject before decoding so a body error still leaves the event identity
            // available to check whether the malformed notice could matter to a consumer.
            let identity = parse_subject_identity(&m.subject).ok();
            match decode(m, rt.catalog()) {
                Ok(notice) => notices.push(notice),
                Err(reason) => {
                    malformed += 1;
                    if malformed_notice_could_matter(&rt, identity) {
                        tracing::warn!(target: "events", subject=%m.subject, %reason, "refusing malformed event notice before acknowledgement");
                    } else {
                        tracing::debug!(target: "events", subject=%m.subject, %reason, "refusing malformed event notice before acknowledgement");
                    }
                }
            }
        }
        // A batch can cover several settled cards; one reread covers all of
        // them from the reader's own cursor, without trusting the pointer seq.
        if card_settled && let Some(consent) = &consent {
            consent.card_settled();
        }
        // Control-only batches admit no flow work, so do not open an empty
        // admission transaction merely to acknowledge their bus messages.
        if notices.is_empty() {
            return Ok((Report::default(), malformed));
        }
        rt.admit_events(&notices)
            .map(|report| (report, malformed))
            .map_err(|e| e.to_string())
    });
    let mut progress = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(5),
        Duration::from_secs(5),
    );
    let report = loop {
        tokio::select! {
            result = &mut commit => break result.map_err(|e| format!("reader stopped: batch worker {e}"))?.map_err(|e| e.to_string())?,
            _ = progress.tick() => {
                for message in messages { message.ack_with(jetstream::AckKind::Progress).await.map_err(|e| e.to_string())?; }
            }
        }
    };
    if acknowledge {
        for message in messages {
            message.double_ack().await.map_err(|e| e.to_string())?;
        }
    }
    Ok(report)
}

/// Publish only immutable, committed outbox rows. Either a fresh receipt or a
/// duplicate acknowledges the same body; errors keep the row for durable retry.
pub async fn publish_outbox(
    rt: &Runtime,
    connection: &NatsConnection,
    account: &str,
) -> Result<usize, String> {
    publish_outbox_pass(rt, connection, account)
        .await
        .map_err(|error| error.message)
}

struct OutboxError {
    message: String,
    retry_unavailable: bool,
}

impl From<basal_core::CoreError> for OutboxError {
    fn from(error: basal_core::CoreError) -> Self {
        Self {
            retry_unavailable: matches!(
                error,
                basal_core::CoreError::Store(_) | basal_core::CoreError::Cut
            ),
            message: error.to_string(),
        }
    }
}

impl From<String> for OutboxError {
    fn from(message: String) -> Self {
        Self {
            message,
            retry_unavailable: false,
        }
    }
}

async fn publish_outbox_pass(
    rt: &Runtime,
    connection: &NatsConnection,
    account: &str,
) -> Result<usize, OutboxError> {
    let names = AccountNames::derive(account).map_err(|e| e.to_string())?;
    let stream = connection.stream(&names.streams().event);
    let pending = rt.pending_template_events(BATCH)?;
    let mut published = 0;
    for event in pending {
        let subject = names
            .event_subject("basal", &event.event_name, 1)
            .map_err(|e| e.to_string())?;
        let headers = BTreeMap::from([
            ("flow_id".into(), event.flow_id.clone()),
            ("agent".into(), event.agent_id.clone()),
            ("seq".into(), event.seq.to_string()),
        ]);
        let receipt = stream
            .publish(
                &subject,
                &event.event_key(),
                ContentDigest::of_bytes(event.body.as_bytes()),
                headers,
            )
            .await;
        match receipt {
            Ok(receipt) => {
                tracing::debug!(target: "events", event_key=%event.event_key(), duplicate=receipt.duplicate(), "lifecycle event acknowledged");
                rt.template_event_published(&event)?;
                published += 1;
            }
            Err(error) => {
                rt.template_event_retry(&event)?;
                return Err(error.to_string().into());
            }
        }
    }
    for event in rt.pending_bus_events(BATCH)? {
        let subject = match event.reader.as_deref() {
            Some(reader) => names.event_subject_with_reader("basal", event.name, 1, reader),
            None => names.event_subject("basal", event.name, 1),
        }
        .map_err(|e| e.to_string())?;
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(MESSAGE_ID, event.message_id.as_str());
        headers.insert(
            CONTENT_DIGEST,
            ContentDigest::of_bytes(event.body.as_bytes())
                .to_string()
                .as_str(),
        );
        let receipt = async {
            jetstream::new(connection.client())
                .publish_with_headers(subject, headers, event.body.clone().into())
                .await
                .map_err(|e| e.to_string())?
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        match receipt {
            Ok(_) => {
                rt.bus_event_published(&event)?;
                published += 1;
            }
            Err(error) => {
                rt.bus_event_retry(&event)?;
                return Err(error.into());
            }
        }
    }
    Ok(published)
}

async fn refresh_backlog(health: &Mutex<Health>, poll: impl Future<Output = Result<u64, String>>) {
    let backlog = match tokio::time::timeout(Duration::from_secs(2), poll).await {
        Ok(Ok(backlog)) => backlog,
        Ok(Err(error)) => {
            tracing::debug!(target: "events", %error, "event backlog poll failed; retaining last value");
            return;
        }
        Err(error) => {
            tracing::debug!(target: "events", %error, "event backlog poll timed out; retaining last value");
            return;
        }
    };
    health.lock().unwrap_or_else(|p| p.into_inner()).backlog = backlog;
}

async fn outbox_loop(
    rt: &Runtime,
    connection: &NatsConnection,
    account: &str,
    health: &Mutex<Health>,
) -> Result<(), String> {
    loop {
        // Drain a prior commit permit before querying. A commit during the pass
        // leaves a permit for the wait below, so no newly committed row is lost.
        let changes = rt.store().overview_changes();
        let changed = changes.outbox_changed();
        tokio::pin!(changed);
        let _ = futures_util::poll!(&mut changed);
        let outbox = publish_outbox_pass(rt, connection, account).await;
        let retry_unavailable = outbox
            .as_ref()
            .err()
            .is_some_and(|error| error.retry_unavailable);
        {
            let mut h = health.lock().unwrap_or_else(|p| p.into_inner());
            #[cfg(any(test, feature = "test-support"))]
            {
                h.outbox_passes_for_test += 1;
            }
            match outbox {
                Ok(_) => h.outbox_error = None,
                Err(error) => {
                    let error = error.message;
                    if h.outbox_error.as_deref() != Some(error.as_str()) {
                        tracing::warn!(target: "events", "lifecycle event publish failed: {error}");
                    }
                    h.outbox_error = Some(error);
                }
            }
        }
        if retry_unavailable {
            // A broken store cannot persist backoff. Ignore commit permits for
            // one second before trying again; the outer cancellation still wins.
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let next = rt.next_bus_event_at().map_err(|e| e.to_string())?;
        let retry = async {
            match next {
                Some(at) => {
                    tokio::time::sleep(Duration::from_millis(
                        at.saturating_sub(rt.config().clock.now_ms()).max(0) as u64,
                    ))
                    .await
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = changes.outbox_changed() => {},
            _ = retry => {},
        }
    }
}

async fn reader_loop(
    rt: Runtime,
    custody: Option<basal_core::codemode::supervisor::Supervisor>,
    consent: Option<Arc<dyn Consent>>,
    transport: Arc<dyn Transport>,
    health: Arc<Mutex<Health>>,
    stop: Arc<AtomicBool>,
) {
    let mut prior = None;
    let mut delay = 250u64;
    while !stop.load(Ordering::Acquire) {
        let session = async {
            let credential = credential(transport.clone(), prior.as_ref()).await?;
            prior = Some(credential.clone());
            let connection = connect(transport.clone(), &credential).await?;
            let mut connection_state = connection.connection_state();
            let work = async {
            let names = AccountNames::derive(&credential.acct).map_err(|e| e.to_string())?;
            let mut consumer = bind(&connection, &names.streams().event).await?;
            {
                let mut h = health.lock().unwrap_or_else(|p| p.into_inner());
                h.connected = true; h.disconnected_since_ms = None; h.error = None; h.retry_in_ms = 0;
            }
            delay = 250;
            let intake = async {
            let mut refresh_at = tokio::time::Instant::now();
            while !stop.load(Ordering::Acquire) {
                if now_ms() / 1000 >= credential.exp.saturating_sub(60) { break; }
                #[cfg(any(test, feature = "test-support"))]
                { health.lock().unwrap_or_else(|p| p.into_inner()).pull_requests_for_test += 1; }
                let stream = consumer.fetch().max_messages(BATCH).expires(PULL_WAIT).messages().await.map_err(|e| e.to_string())?;
                // Yield the first available notice without waiting for the pull
                // expiry; retain the stream so the rest of the fetch is not lost.
                let mut batches = stream.ready_chunks(BATCH);
                loop {
                let messages = tokio::select! {
                    batch = batches.next() => match batch {
                        Some(batch) => batch.into_iter().collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?,
                        None => break,
                    },
                    _ = tokio::time::sleep_until(refresh_at) => Vec::new(),
                };
                let start = Instant::now();
                if !messages.is_empty() || tokio::time::Instant::now() >= refresh_at {
                #[cfg(any(test, feature = "test-support"))]
                { health.lock().unwrap_or_else(|p| p.into_inner()).backlog_polls_for_test += 1; }
                refresh_backlog(&health, async {
                    consumer.info().await
                        .map(|info| info.num_pending.saturating_add(u64::try_from(info.num_ack_pending).unwrap_or(u64::MAX)))
                        .map_err(|e| e.to_string())
                }).await;
                refresh_at = tokio::time::Instant::now() + BACKLOG_REFRESH;
                }
                if messages.is_empty() { continue; }
                let oldest = messages.iter().filter_map(|m| m.info().ok()).map(|i| now_ms().saturating_sub(i.published.unix_timestamp_nanos().div_euclid(1_000_000) as i64)).max();
                let (report, malformed) = commit_batch_report(rt.clone(), custody.clone(), consent.clone(), &messages, true).await?;
                let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                let mut h = health.lock().unwrap_or_else(|p| p.into_inner());
                h.backlog = h.backlog.saturating_sub(messages.len() as u64);
                if h.first_batch_notices.is_none() {
                    h.first_batch_notices = Some(messages.len()); h.first_batch_elapsed_ms = Some(elapsed); h.first_batch_oldest_age_ms = oldest;
                    tracing::info!(target: "events", notices=messages.len(), elapsed_ms=elapsed, oldest_age_ms=oldest, admitted=report.admitted, queued=report.queued, overflow=report.overflow, "first retained event batch committed");
                }
                h.batches += 1;
                h.notices += messages.len() as u64;
                h.admitted += report.admitted as u64;
                h.queued += report.queued as u64;
                h.overflow += report.overflow as u64;
                h.model_wakes += report.model_wakes as u64;
                h.malformed += malformed as u64;
                h.skipped_before_install += report.skipped_before_install as u64;
                h.filtered_out += report.filtered_out as u64;
                h.stale_dropped += report.stale_dropped as u64;
                }
            }
            Ok::<_, String>(())
            };
            // Publisher failures keep their durable retries and never stall
            // intake. Shutdown drops both waits only outside SQLite work.
            tokio::select! {
                result = intake => result,
                result = outbox_loop(&rt, &connection, &credential.acct, &health) => result,
            }
            };
            // A connection event ends both users of this session, even while a
            // pull is parked. Only the existing outer loop rebuilds it.
            tokio::select! {
                biased;
                _ = until_disconnected(&mut connection_state) => {
                    #[cfg(any(test, feature = "test-support"))]
                    { health.lock().unwrap_or_else(|p| p.into_inner()).disconnect_wakes_for_test += 1; }
                    Err("event bus disconnected".into())
                },
                result = work => result,
            }
        }.await;
        {
            let mut h = health.lock().unwrap_or_else(|p| p.into_inner());
            h.mark_down();
            h.error = session.err();
            h.retry_in_ms = delay;
            if let Some(error) = &h.error {
                tracing::warn!(target: "events", retry_in_ms=delay, "event reader: {error}");
            }
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        let until = tokio::time::Instant::now() + Duration::from_millis(delay);
        while !stop.load(Ordering::Acquire) && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        delay = (delay * 2).min(30_000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disconnect_watch_observes_current_and_future_connection_loss() {
        for lost in [
            NatsConnectionState::Disconnected,
            NatsConnectionState::Closed,
        ] {
            let (sender, mut state) = tokio::sync::watch::channel(NatsConnectionState::Connected);
            let disconnected = until_disconnected(&mut state);
            tokio::pin!(disconnected);
            assert!(futures_util::poll!(&mut disconnected).is_pending());
            sender.send(lost).unwrap();
            tokio::time::timeout(Duration::from_secs(1), disconnected)
                .await
                .expect("connection loss must wake a parked session");

            let (_sender, mut state) = tokio::sync::watch::channel(lost);
            tokio::time::timeout(Duration::from_secs(1), until_disconnected(&mut state))
                .await
                .expect("connection loss before setup must not be missed");
        }
    }

    #[tokio::test]
    async fn disconnect_watch_closure_ends_the_session() {
        let (sender, mut state) = tokio::sync::watch::channel(NatsConnectionState::Connected);
        drop(sender);
        tokio::time::timeout(Duration::from_secs(1), until_disconnected(&mut state))
            .await
            .expect("a closed connection watch must not leave the session parked");
    }

    #[tokio::test]
    async fn shutdown_cancels_a_parked_bus_wait() {
        let cancel = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let stopped = cancel.clone();
        let observed = dropped.clone();
        let session = tokio::spawn(async move {
            until_cancelled(&stopped, async {
                let _dropped = Dropped(observed);
                entered.send(()).unwrap();
                std::future::pending::<()>().await;
            })
            .await;
        });
        waiting.await.unwrap();
        cancel.notify_one();
        tokio::time::timeout(Duration::from_secs(1), session)
            .await
            .expect("shutdown must not wait for an idle bus peer")
            .unwrap();
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn backlog_poll_error_keeps_reader_session_health() {
        check_failed_backlog_poll(async { Err("consumer info unavailable".into()) }).await;
    }

    #[tokio::test]
    async fn backlog_poll_timeout_keeps_reader_session_health() {
        check_failed_backlog_poll(std::future::pending()).await;
    }

    async fn check_failed_backlog_poll(poll: impl Future<Output = Result<u64, String>>) {
        let health = Mutex::new(Health {
            connected: true,
            backlog: 7,
            batches: 1,
            ..Default::default()
        });
        let before = serde_json::to_value(health.lock().unwrap().clone()).unwrap();
        refresh_backlog(&health, poll).await;
        assert_eq!(
            serde_json::to_value(health.lock().unwrap().clone()).unwrap(),
            before
        );
        refresh_backlog(&health, async { Ok(3) }).await;
        let h = health.lock().unwrap();
        assert_eq!(h.backlog, 3);
        assert!(h.connected);
        assert!(h.error.is_none());
        assert_eq!(h.retry_in_ms, 0);
    }

    #[tokio::test]
    async fn backlog_poll_success_updates_metric_only() {
        let health = Mutex::new(Health {
            connected: true,
            backlog: 7,
            batches: 1,
            ..Default::default()
        });
        let mut expected = serde_json::to_value(health.lock().unwrap().clone()).unwrap();
        expected["backlog"] = json!(11);
        refresh_backlog(&health, async { Ok(11) }).await;
        assert_eq!(
            serde_json::to_value(health.lock().unwrap().clone()).unwrap(),
            expected
        );
    }

    #[test]
    fn reader_supervisor_reports_panic_and_restarts() {
        check_restart(true);
    }

    #[test]
    fn reader_supervisor_reports_unexpected_exit_and_restarts() {
        check_restart(false);
    }

    fn check_restart(panic: bool) {
        let health = Arc::new(Mutex::new(Health::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let mut attempts = 0;
        let start = Instant::now();
        let mut stopped_health = None;
        supervise(health.clone(), stop.clone(), || {
            attempts += 1;
            if attempts == 1 {
                health.lock().unwrap().connected = true;
                assert!(!panic, "injected reader loop panic");
            } else {
                stopped_health = Some(health.lock().unwrap().clone());
                stop.store(true, Ordering::Release);
            }
        });
        assert_eq!(attempts, 2);
        let h = stopped_health.unwrap();
        assert!(!h.connected);
        let expected = if panic {
            "reader stopped: panic: injected reader loop panic"
        } else {
            "reader stopped: loop exited"
        };
        assert_eq!(h.error.as_deref(), Some(expected));
        assert_eq!(h.retry_in_ms, 250);
        assert!(start.elapsed() >= Duration::from_millis(250));
    }
}
