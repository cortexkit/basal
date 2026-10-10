//! Read the JetStream pull consumer `m_basal`, which ck-bus provisions on the
//! event stream. This reader never creates or modifies the stream or consumer.
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_nats::jetstream::{self, consumer::PullConsumer};
use basal_core::{
    Runtime,
    events::{Notice, Report},
};
use basal_host::transport::Transport;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cortexkit_bus_naming::AccountNames;
use cortexkit_bus_nats::{ConnectConfig, NatsConnection};
use cortexkit_bus_trait::ContentDigest;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const BATCH: usize = 32;
const DURABLE: &str = "m_basal";
// cortexkit-bus-nats reserves these for the event key and SHA-256 body digest.
// Its private codec does not export their names, so batch decoding names them here.
const MESSAGE_ID: &str = "Nats-Msg-Id";
const CONTENT_DIGEST: &str = "Ck-Content-Digest";

#[derive(Debug, Clone, Default, Serialize)]
pub struct Health {
    pub connected: bool,
    pub error: Option<String>,
    pub retry_in_ms: u64,
    pub batches: u64,
    pub notices: u64,
    pub admitted: u64,
    pub queued: u64,
    pub overflow: u64,
    pub malformed: u64,
    pub first_batch_notices: Option<usize>,
    pub first_batch_elapsed_ms: Option<u64>,
    pub first_batch_oldest_age_ms: Option<i64>,
}

pub struct Reader {
    health: Arc<Mutex<Health>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Reader {
    pub fn start(rt: Runtime, transport: Arc<dyn Transport>) -> Self {
        let health = Arc::new(Mutex::new(Health::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_health = health.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("event reader runtime");
            runtime.block_on(reader_loop(rt, transport, thread_health, thread_stop));
        });
        Self {
            health,
            stop,
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
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
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
    .map_err(|e| e.to_string())?
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

pub fn decode(message: &jetstream::Message) -> Result<Notice, String> {
    if !message.payload.is_empty() {
        return Err("event notice carries a body".into());
    }
    let headers = message.headers.as_ref().ok_or("notice headers absent")?;
    let get = |name: &str| {
        headers
            .get_last(name)
            .map(|v| v.as_str().to_owned())
            .ok_or_else(|| format!("notice {name} absent"))
    };
    let digest = get(CONTENT_DIGEST)?;
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
    let notice = Notice {
        subject: message.subject.to_string(),
        event_key: get(MESSAGE_ID)?,
        digest,
        headers: labels,
    };
    notice.identity().map_err(|e| e.to_string())?;
    Ok(notice)
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
    let notices = messages
        .iter()
        .filter_map(|m| match decode(m) {
            Ok(notice) => Some(notice),
            Err(reason) => {
                tracing::warn!(target: "events", subject=%m.subject, %reason, "refusing malformed event notice before acknowledgement");
                None
            }
        })
        .collect::<Vec<_>>();
    let mut commit = tokio::task::spawn_blocking(move || rt.admit_events(&notices));
    let mut progress = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(5),
        Duration::from_secs(5),
    );
    let report = loop {
        tokio::select! {
            result = &mut commit => break result.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?,
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

async fn reader_loop(
    rt: Runtime,
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
            let names = AccountNames::derive(&credential.acct).map_err(|e| e.to_string())?;
            let consumer = bind(&connection, &names.streams().event).await?;
            {
                let mut h = health.lock().unwrap_or_else(|p| p.into_inner());
                h.connected = true; h.error = None; h.retry_in_ms = 0;
            }
            delay = 250;
            while !stop.load(Ordering::Acquire) {
                if now_ms() / 1000 >= credential.exp.saturating_sub(60) { break; }
                if connection.client().connection_state() != async_nats::connection::State::Connected { return Err("event bus disconnected".into()); }
                let start = Instant::now();
                let messages = pull(&consumer).await?;
                if messages.is_empty() { continue; }
                let oldest = messages.iter().filter_map(|m| m.info().ok()).map(|i| now_ms().saturating_sub(i.published.unix_timestamp_nanos().div_euclid(1_000_000) as i64)).max();
                let malformed = messages.iter().filter(|m| decode(m).is_err()).count();
                let report = commit_batch(rt.clone(), &messages, true).await?;
                let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                let mut h = health.lock().unwrap_or_else(|p| p.into_inner());
                if h.first_batch_notices.is_none() {
                    h.first_batch_notices = Some(messages.len()); h.first_batch_elapsed_ms = Some(elapsed); h.first_batch_oldest_age_ms = oldest;
                    tracing::info!(target: "events", notices=messages.len(), elapsed_ms=elapsed, oldest_age_ms=oldest, admitted=report.admitted, queued=report.queued, overflow=report.overflow, "first retained event batch committed");
                }
                h.batches += 1; h.notices += messages.len() as u64; h.admitted += report.admitted as u64; h.queued += report.queued as u64; h.overflow += report.overflow as u64; h.malformed += malformed as u64;
            }
            Ok::<_, String>(())
        }.await;
        if stop.load(Ordering::Acquire) {
            break;
        }
        if let Err(error) = session {
            tracing::warn!(target: "events", retry_in_ms=delay, "event reader: {error}");
            let mut h = health.lock().unwrap_or_else(|p| p.into_inner());
            h.connected = false;
            h.error = Some(error);
            h.retry_in_ms = delay;
        }
        let until = tokio::time::Instant::now() + Duration::from_millis(delay);
        while !stop.load(Ordering::Acquire) && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        delay = (delay * 2).min(30_000);
    }
}
