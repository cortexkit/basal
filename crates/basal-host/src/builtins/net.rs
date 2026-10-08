//! `net.fetch`: HTTPS requests to the manifest's approved hosts.
//!
//! The rules, in the order a request meets them:
//! - HTTPS only, to a host named in the manifest's `net.fetch` line, with a
//!   method approved for that host (`GET` and `HEAD` unless the line lists
//!   others). An IP-literal host is never accepted, so every connection
//!   starts from a DNS name.
//! - Before connecting, the host is resolved once and every address it
//!   resolves to must be public: a private, loopback, link-local,
//!   multicast or otherwise reserved address (an IPv4-mapped IPv6 address
//!   is judged as the IPv4 address it carries) refuses the request. The
//!   connection then goes to an address from that same answer, so a second
//!   DNS answer cannot swap in another; TLS still verifies the certificate
//!   against the host name.
//! - Redirects are followed by hand, at most [`MAX_REDIRECTS`], and every
//!   hop is checked again from the top: scheme, host, method and address.
//! - Connecting, the whole exchange and the response have limits: a connect
//!   timeout, a deadline over every hop, and caps on the response head and
//!   body.
//! - Nothing is added from anywhere: no cookies are sent or kept, no proxy
//!   or credential is read from the environment, and a script may not set
//!   `Authorization`, `Cookie` or the headers that frame the request.
//!
//! basal speaks HTTP/1.1 itself, one request per connection, so each of
//! those steps is in this file rather than inside a client library.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
#[cfg(any(test, feature = "rig-kill-hook"))]
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{Denial, Failure, codes, options, string_arg};
use crate::{TransportError, UnknownReason};

/// The methods a manifest may approve.
pub const METHODS: [&str; 6] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];
/// The methods a host is approved for when its line lists none.
pub const DEFAULT_METHODS: [&str; 2] = ["GET", "HEAD"];
/// The most redirects one fetch follows.
pub const MAX_REDIRECTS: usize = 5;
/// The default cap on a response body.
pub const DEFAULT_BODY_BYTES: usize = super::MAX_TEXT_RESULT_BYTES;
/// The cap on a response's status line and headers.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;
/// The largest request body a script may send.
pub const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
/// The most headers a script may set.
pub const MAX_REQUEST_HEADERS: usize = 32;
/// How many of a host's addresses are tried in turn.
const MAX_ADDRESS_ATTEMPTS: usize = 4;
const MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;
const MAX_CHUNK_LINE_BYTES: u64 = 1024;
const MAX_TRAILER_LINE_BYTES: u64 = 8 * 1024;
const MAX_DNS_NAME_BYTES: usize = 253;
const MAX_DNS_LABEL_BYTES: usize = 63;
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// Headers a script may not set: the ones that frame the request (basal
/// writes those itself) and the ones that carry cookies or credentials.
const FORBIDDEN_HEADERS: [&str; 19] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "upgrade",
    "te",
    "trailer",
    "expect",
    "cookie",
    "cookie2",
    "authorization",
    "proxy-authorization",
    "x-http-method-override",
    "x-http-method",
    "x-method-override",
    "accept-encoding",
    "user-agent",
];

/// One approved host and the methods approved for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRule {
    pub host: String,
    pub methods: Vec<String>,
}

/// Whether a request with `method` only reads.
pub fn is_query_method(method: &str) -> bool {
    method == "GET" || method == "HEAD"
}

/// Why `host` cannot be an approved host or a URL's host: `None` when it is
/// a lower-case DNS name. Wildcards and IP literals (dotted, numeric or
/// IPv6) are refused.
pub fn host_problem(host: &str) -> Option<&'static str> {
    if host.contains('*') {
        return Some("a wildcard host is not allowed");
    }
    if host.contains(':') || host.contains('[') || host.parse::<Ipv4Addr>().is_ok() {
        return Some("an IP-literal host is not allowed");
    }
    if host.is_empty() || host.len() > MAX_DNS_NAME_BYTES {
        return Some("a host name is 1 to 253 bytes");
    }
    let labels: Vec<&str> = host.split('.').collect();
    // A numeric last label is how shortened and integer IPv4 forms
    // (`127.1`, `2130706433`, `0x7f.1`) look; no public DNS name ends so.
    if let Some(last) = labels.last()
        && (last.bytes().all(|b| b.is_ascii_digit()) || last.starts_with("0x"))
    {
        return Some("an IP-literal host is not allowed");
    }
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.len() <= MAX_DNS_LABEL_BYTES
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if !labels.iter().all(label_ok) {
        return Some("a host is a lower-case DNS name (letters, digits, `-` and dots)");
    }
    None
}

/// A parsed `https` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    /// The path and query, starting with `/`.
    pub target: String,
}

impl Url {
    fn authority(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn target_ok(target: &str) -> bool {
    target.starts_with('/') && target.bytes().all(|b| b.is_ascii_graphic())
}

/// Parses an absolute `https://` URL. The fragment is dropped; user info,
/// IP literals and characters that would need encoding are refused.
pub fn parse_url(text: &str) -> Result<Url, Denial> {
    let bad = |why: &str| Denial::invalid(format!("{text:?}: {why}"));
    let Some(scheme_end) = text.find("://") else {
        return Err(bad("not an absolute URL"));
    };
    if !text[..scheme_end].eq_ignore_ascii_case("https") {
        return Err(Denial::denied(format!(
            "{text:?}: only https URLs are fetched"
        )));
    }
    let rest = &text[scheme_end + 3..];
    let rest = rest.split('#').next().unwrap_or("");
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return Err(bad("credentials in a URL are not allowed"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(']') && !h.contains('[') => {
            let port: u16 = p.parse().map_err(|_| bad("a bad port"))?;
            if port == 0 {
                return Err(bad("a bad port"));
            }
            (h, port)
        }
        _ => (authority, 443),
    };
    let host = host.to_ascii_lowercase();
    if let Some(problem) = host_problem(&host) {
        return Err(Denial::denied(format!("{text:?}: {problem}")));
    }
    let target = if tail.is_empty() {
        "/".to_owned()
    } else if tail.starts_with('?') {
        format!("/{tail}")
    } else {
        tail.to_owned()
    };
    if !target_ok(&target) {
        return Err(bad(
            "the path holds characters that must be percent-encoded",
        ));
    }
    Ok(Url { host, port, target })
}

/// Resolves a redirect's `Location` against the URL that answered it.
fn resolve_location(base: &Url, location: &str) -> Result<Url, Denial> {
    let location = location.trim();
    // A scheme exists only before the first path, query or fragment delimiter.
    if location
        .split(['/', '?', '#'])
        .next()
        .is_some_and(|s| s.contains(':'))
    {
        return parse_url(location);
    }
    if let Some(rest) = location.strip_prefix("//") {
        return parse_url(&format!("https://{rest}"));
    }
    let location = location.split('#').next().unwrap_or("");
    let target = if location.is_empty() {
        base.target.clone()
    } else if location.starts_with('?') {
        format!("{}{location}", base.target.split('?').next().unwrap_or("/"))
    } else if location.starts_with('/') {
        location.to_owned()
    } else {
        let path = base.target.split('?').next().unwrap_or("/");
        let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
        format!("{dir}{location}")
    };
    let target = remove_dot_segments(&target);
    if !target_ok(&target) {
        return Err(Denial::new(
            codes::NET,
            format!("redirect to {location:?}, which basal cannot follow"),
        ));
    }
    Ok(Url {
        host: base.host.clone(),
        port: base.port,
        target,
    })
}

fn remove_dot_segments(target: &str) -> String {
    let (path, query) = target
        .split_once('?')
        .map_or((target, None), |(p, q)| (p, Some(q)));
    let mut parts = Vec::new();
    for part in path.split('/').skip(1) {
        match part {
            "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let mut path = format!("/{}", parts.join("/"));
    if (target.ends_with("/.") || target.ends_with("/..")) && !path.ends_with('/') {
        path.push('/');
    }
    if let Some(query) = query {
        path.push('?');
        path.push_str(query);
    }
    path
}

/// A DNS-name grant approves its normal HTTPS endpoint, never another service
/// on an arbitrary port. URL parsing already enforces the HTTPS scheme.
pub fn allowed_url(rules: &[HostRule], url: &Url, method: &str) -> Result<(), Denial> {
    if url.port != 443 {
        return Err(Denial::denied(format!(
            "HTTPS port {} is not approved; hosts grant only port 443",
            url.port
        )));
    }
    allowed(rules, &url.host, method)
}

fn same_origin(a: &Url, b: &Url) -> bool {
    // Every Url is HTTPS, so scheme equality is guaranteed by parsing.
    a.host == b.host && a.port == b.port
}

/// Requires `host` to be approved, and `method` approved for it.
pub fn allowed(rules: &[HostRule], host: &str, method: &str) -> Result<(), Denial> {
    let Some(rule) = rules.iter().find(|r| r.host == host) else {
        return Err(Denial::denied(format!(
            "{host} is not among the manifest's net.fetch hosts"
        )));
    };
    if rule.methods.iter().any(|m| m == method) {
        Ok(())
    } else {
        Err(Denial::denied(format!(
            "{method} is not approved for {host}"
        )))
    }
}

/// Whether `ip` is a public unicast address a fetch may connect to.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn in_v4(ip: u32, base: [u8; 4], bits: u32) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    ip & mask == u32::from_be_bytes(base) & mask
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let v = u32::from(ip);
    // The special-purpose ranges of the IANA IPv4 registry (RFC 6890 and
    // its updates), each named as the registry names it. None of them is a
    // public server a fetch should reach.
    let reserved: [([u8; 4], u32); 16] = [
        ([0, 0, 0, 0], 8),          // "this network"
        ([10, 0, 0, 0], 8),         // private
        ([100, 64, 0, 0], 10),      // carrier-grade NAT
        ([127, 0, 0, 0], 8),        // loopback
        ([169, 254, 0, 0], 16),     // link-local
        ([172, 16, 0, 0], 12),      // private
        ([192, 0, 0, 0], 24),       // IETF protocol assignments
        ([192, 0, 2, 0], 24),       // documentation
        ([192, 88, 99, 0], 24),     // 6to4 relay anycast
        ([192, 168, 0, 0], 16),     // private
        ([198, 18, 0, 0], 15),      // benchmarking
        ([198, 51, 100, 0], 24),    // documentation
        ([203, 0, 113, 0], 24),     // documentation
        ([224, 0, 0, 0], 4),        // multicast
        ([240, 0, 0, 0], 4),        // reserved for future use
        ([255, 255, 255, 255], 32), // limited broadcast
    ];
    !reserved.iter().any(|(base, bits)| in_v4(v, *base, *bits))
}

fn in_v6(ip: u128, base: u128, bits: u32) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u128::MAX << (128 - bits)
    };
    ip & mask == base & mask
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let v = u128::from(ip);
    // Only global unicast (2000::/3) is public, less the parts of it that
    // are not: IETF protocol assignments, documentation, and 6to4, whose
    // addresses carry an IPv4 address that may be private. Everything
    // outside 2000::/3 (loopback, unspecified, unique-local, link-local,
    // multicast, NAT64 and IPv4-compatible forms) is refused.
    in_v6(v, 0x2000 << 112, 3)
        && !in_v6(v, 0x2001 << 112, 23)
        && !in_v6(v, (0x2001 << 112) | (0x0db8 << 96), 32)
        && !in_v6(v, 0x2002 << 112, 16)
}

/// The script's request, checked for shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

/// The method a `net.fetch` call asks for: `GET` when it names none.
pub fn method_of(args: &Value) -> Result<String, Denial> {
    if !args.is_object() {
        return Err(Denial::invalid("net.fetch arguments must be an object"));
    }
    let options = options(args, "options", &["method", "headers", "body"])?;
    match options.and_then(|o| o.get("method")) {
        None | Some(Value::Null) => Ok("GET".to_owned()),
        Some(Value::String(m)) => {
            let m = m.to_ascii_uppercase();
            if METHODS.contains(&m.as_str()) {
                Ok(m)
            } else {
                Err(Denial::invalid(format!(
                    "method {m:?} is not one of {}",
                    METHODS.join(", ")
                )))
            }
        }
        Some(_) => Err(Denial::invalid("method must be a string")),
    }
}

fn header_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Reads `net.fetch`'s arguments.
pub fn parse_request(args: &Value) -> Result<Request, Denial> {
    let url = string_arg(args, "url")?;
    let method = method_of(args)?;
    let options = options(args, "options", &["method", "headers", "body"])?;
    let mut headers = Vec::new();
    match options.and_then(|o| o.get("headers")) {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            if map.len() > MAX_REQUEST_HEADERS {
                return Err(Denial::invalid(format!(
                    "at most {MAX_REQUEST_HEADERS} headers"
                )));
            }
            for (name, value) in map {
                let Some(value) = value.as_str() else {
                    return Err(Denial::invalid(format!("header {name} must be a string")));
                };
                if !header_name_ok(name) {
                    return Err(Denial::invalid(format!("{name:?} is not a header name")));
                }
                if FORBIDDEN_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
                    return Err(Denial::denied(format!(
                        "a flow may not set the {name} header"
                    )));
                }
                if value.len() > MAX_HEADER_VALUE_BYTES
                    || value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f)
                {
                    return Err(Denial::invalid(format!(
                        "header {name} must be at most 8 KiB with no control characters"
                    )));
                }
                headers.push((name.clone(), value.to_owned()));
            }
        }
        Some(_) => return Err(Denial::invalid("headers must be an object of strings")),
    }
    let body = match options.and_then(|o| o.get("body")) {
        None | Some(Value::Null) => None,
        Some(Value::String(b)) => {
            if is_query_method(&method) {
                return Err(Denial::invalid(format!("a {method} request has no body")));
            }
            if b.len() > MAX_REQUEST_BODY_BYTES {
                return Err(Denial::new(
                    codes::TOO_LARGE,
                    format!("a request body is at most {MAX_REQUEST_BODY_BYTES} bytes"),
                ));
            }
            Some(b.clone())
        }
        Some(_) => return Err(Denial::invalid("body must be a string")),
    };
    Ok(Request {
        url,
        method,
        headers,
        body,
    })
}

/// Turns a host name into addresses.
pub trait Resolve: Send + Sync {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>>;
    fn resolve_until(
        &self,
        host: &str,
        port: u16,
        _deadline: Instant,
    ) -> std::io::Result<Vec<SocketAddr>> {
        self.resolve(host, port)
    }
}

/// The system resolver, bounded by a timeout.
#[derive(Debug, Clone, Copy)]
pub struct SystemResolver {
    pub timeout: Duration,
}

impl Default for SystemResolver {
    fn default() -> Self {
        Self {
            timeout: DNS_TIMEOUT,
        }
    }
}

impl Resolve for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        self.resolve_until(host, port, Instant::now() + self.timeout)
    }
    fn resolve_until(
        &self,
        host: &str,
        port: u16,
        deadline: Instant,
    ) -> std::io::Result<Vec<SocketAddr>> {
        // getaddrinfo cannot be cancelled. A single reusable worker and one
        // queued job bound the resources even if the OS resolver gets stuck.
        let deadline = deadline.min(Instant::now() + self.timeout);
        let (reply, receiver) = mpsc::sync_channel(1);
        let job = DnsJob {
            host: host.into(),
            port,
            deadline,
            reply,
        };
        dns_worker().try_send(job).map_err(|_| dns_timeout())?;
        receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| dns_timeout())?
    }
}

struct DnsJob {
    host: String,
    port: u16,
    deadline: Instant,
    reply: mpsc::SyncSender<std::io::Result<Vec<SocketAddr>>>,
}
fn dns_timeout() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "DNS resolution timed out or busy",
    )
}
fn dns_worker() -> &'static mpsc::SyncSender<DnsJob> {
    static WORKER: OnceLock<mpsc::SyncSender<DnsJob>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<DnsJob>(1);
        std::thread::spawn(move || {
            while let Ok(job) = receiver.recv() {
                let result = if Instant::now() >= job.deadline {
                    Err(dns_timeout())
                } else {
                    (job.host.as_str(), job.port)
                        .to_socket_addrs()
                        .map(Vec::from_iter)
                };
                let _ = job.reply.send(result);
            }
        });
        sender
    })
}

/// A fixed table of host names to addresses, for tests. The port each
/// entry names is used whatever port the URL asks for.
#[derive(Debug, Clone, Default)]
pub struct StaticResolver(pub BTreeMap<String, Vec<SocketAddr>>);

impl Resolve for StaticResolver {
    fn resolve(&self, host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        self.0.get(host).cloned().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no address for {host}"),
            )
        })
    }
}

/// Settings of `net.fetch`. The default is production's: the system
/// resolver, the bundled Mozilla roots and no exceptions. Only tests change
/// `trust_anchors` and `private_hosts`, and production has no setting that
/// reaches them.
#[derive(Clone)]
pub struct NetConfig {
    pub resolver: Arc<dyn Resolve>,
    /// DER certificates trusted instead of the bundled roots.
    pub trust_anchors: Option<Vec<Vec<u8>>>,
    /// Hosts allowed to resolve to non-public addresses (a test server on
    /// the loopback interface).
    #[cfg(any(test, feature = "rig-kill-hook"))]
    pub private_hosts: BTreeSet<String>,
    pub connect_timeout: Duration,
    /// The deadline over a whole fetch, every redirect included.
    pub total_timeout: Duration,
    pub max_body_bytes: usize,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            resolver: Arc::new(SystemResolver::default()),
            trust_anchors: None,
            #[cfg(any(test, feature = "rig-kill-hook"))]
            private_hosts: BTreeSet::new(),
            connect_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(30),
            max_body_bytes: DEFAULT_BODY_BYTES,
        }
    }
}

/// The HTTPS client behind `net.fetch`.
pub struct Client {
    config: NetConfig,
    tls: OnceLock<Result<Arc<ClientConfig>, String>>,
}

/// What one exchange returned.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Why the body was not kept (an error code), when the server may have
    /// acted on the request and the answer is returned without its body.
    body_error: Option<&'static str>,
    /// Whether a request that may change something had left by the time
    /// this answer came: this one, or an earlier hop of the same fetch.
    effect_possible: bool,
}

/// A failure before any byte of this exchange's request was written.
fn unsent(detail: String) -> Failure {
    Failure::Transport(TransportError::unsent(detail))
}

impl Client {
    fn private_host(&self, _host: &str) -> bool {
        #[cfg(any(test, feature = "rig-kill-hook"))]
        {
            self.config.private_hosts.contains(_host)
        }
        #[cfg(not(any(test, feature = "rig-kill-hook")))]
        {
            false
        }
    }
    pub fn new(config: NetConfig) -> Self {
        Self {
            config,
            tls: OnceLock::new(),
        }
    }

    fn tls(&self) -> Result<Arc<ClientConfig>, Denial> {
        self.tls
            .get_or_init(|| {
                let mut roots = RootCertStore::empty();
                match &self.config.trust_anchors {
                    None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
                    Some(anchors) => {
                        for der in anchors {
                            roots
                                .add(CertificateDer::from(der.clone()))
                                .map_err(|e| e.to_string())?;
                        }
                    }
                }
                let provider = Arc::new(rustls::crypto::ring::default_provider());
                let config = ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .map_err(|e| e.to_string())?
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                Ok(Arc::new(config))
            })
            .clone()
            .map_err(|e| Denial::new(codes::TLS, e))
    }

    /// Runs a fetch: every hop checked against `rules`, the final response
    /// returned as `{status, headers, body}`.
    pub fn fetch(&self, request: &Request, rules: &[HostRule]) -> Result<Value, Failure> {
        let deadline = Instant::now() + self.config.total_timeout;
        let first = parse_url(&request.url)?;
        let mut url = first.clone();
        let mut method = request.method.clone();
        let mut body = request.body.clone();
        let mut progress = Progress::default();
        // The last redirect answered once a mutation had left. From then on
        // the server may have applied the effect, so a refusal to follow
        // the redirect must not be reported as a refused call: the call
        // ends with this redirect as its answer instead (see `stop`).
        let mut redirected: Option<Response> = None;
        let mut redirects = 0;
        loop {
            let approval = if self.private_host(&url.host) {
                allowed(rules, &url.host, &method)
            } else {
                allowed_url(rules, &url, &method)
            };
            if let Err(refusal) = approval {
                return stop(redirected, refusal);
            }
            // A script's headers go only to the host it named.
            let headers: &[(String, String)] = if same_origin(&url, &first) {
                &request.headers
            } else {
                &[]
            };
            progress.request_left = false;
            let response = match self.exchange(
                &url,
                &method,
                headers,
                body.as_deref(),
                deadline,
                &mut progress,
            ) {
                Ok(response) => response,
                // Refused before this hop's request left (a private address,
                // a certificate refused): the redirect was not followed.
                Err(Failure::Refused(refusal)) if !progress.request_left => {
                    return stop(redirected, refusal);
                }
                Err(failure) => return Err(failure),
            };
            if !is_query_method(&method) {
                progress.sent_mutation = true;
            }
            if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                return finish(response);
            }
            let next = if redirects >= MAX_REDIRECTS {
                Err(Denial::new(
                    codes::NET,
                    format!("more than {MAX_REDIRECTS} redirects"),
                ))
            } else {
                header(&response.headers, "location")
                    .ok_or_else(|| Denial::new(codes::NET, "a redirect without a Location"))
                    .and_then(|location| resolve_location(&url, location))
            };
            let status = response.status;
            redirected = progress.sent_mutation.then_some(response);
            url = match next {
                Ok(next) => next,
                Err(refusal) => return stop(redirected, refusal),
            };
            redirects += 1;
            // As browsers do: 303 always, and 301 or 302 after a POST, turn
            // the next request into a GET without a body.
            if status == 303 && method != "HEAD" || matches!(status, 301 | 302) && method == "POST"
            {
                method = "GET".to_owned();
                body = None;
            }
        }
    }

    /// The checked addresses of `url`'s host.
    fn addresses(&self, url: &Url, deadline: Instant) -> Result<Vec<SocketAddr>, Failure> {
        let addrs = self
            .config
            .resolver
            .resolve_until(&url.host, url.port, deadline)
            .map_err(|e| unsent(format!("resolving {}: {e}", url.host)))?;
        if addrs.is_empty() {
            return Err(unsent(format!("{} resolved to no address", url.host)));
        }
        if !self.private_host(&url.host)
            && let Some(bad) = addrs.iter().find(|a| !is_public(a.ip()))
        {
            return Err(Denial::denied(format!(
                "{} resolves to {}, which is not a public address",
                url.host,
                bad.ip()
            ))
            .into());
        }
        Ok(addrs)
    }

    /// One request and its response, over a connection to an address
    /// checked by [`Client::addresses`].
    fn exchange(
        &self,
        url: &Url,
        method: &str,
        headers: &[(String, String)],
        body: Option<&str>,
        deadline: Instant,
        progress: &mut Progress,
    ) -> Result<Response, Failure> {
        let sent_mutation = progress.sent_mutation;
        // Before this exchange's request leaves, a transport failure is
        // provably unsent only if no earlier hop sent a mutation.
        let before = |detail: String| {
            if sent_mutation {
                Failure::Transport(TransportError::maybe_sent(
                    UnknownReason::ConnectionLost,
                    detail,
                ))
            } else {
                unsent(detail)
            }
        };
        let addrs = self
            .addresses(url, deadline)
            .map_err(|failure| match failure {
                Failure::Transport(TransportError::Unavailable { detail, .. }) => before(detail),
                refused => refused,
            })?;
        let mut last_error = String::new();
        let mut tcp = None;
        for addr in addrs.iter().take(MAX_ADDRESS_ATTEMPTS) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match TcpStream::connect_timeout(addr, self.config.connect_timeout.min(left)) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last_error = format!("connecting to {addr}: {e}"),
            }
        }
        let Some(tcp) = tcp else {
            return Err(before(last_error));
        };
        let _ = tcp.set_nodelay(true);
        let name = ServerName::try_from(url.host.clone())
            .map_err(|e| Denial::invalid(format!("{}: {e}", url.host)))?;
        let tls = self.tls()?;
        let conn =
            ClientConnection::new(tls, name).map_err(|e| Denial::new(codes::TLS, e.to_string()))?;
        let mut stream = StreamOwned::new(conn, tcp);
        while stream.conn.is_handshaking() {
            set_timeouts(&stream.sock, deadline).map_err(|e| before(e.to_string()))?;
            if let Err(e) = stream.conn.complete_io(&mut stream.sock) {
                let refused = e
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<rustls::Error>())
                    .map(|tls| tls.to_string());
                return Err(match refused {
                    Some(tls) => {
                        Denial::new(codes::TLS, format!("TLS with {}: {tls}", url.host)).into()
                    }
                    None => before(format!("TLS with {}: {e}", url.host)),
                });
            }
        }

        let mut head = format!(
            "{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: basal\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
            url.target,
            url.authority()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(b) = body {
            head.push_str(&format!("Content-Length: {}\r\n", b.len()));
        } else if !is_query_method(method) {
            head.push_str("Content-Length: 0\r\n");
        }
        head.push_str("\r\n");
        // From the first byte written, the request may have reached the
        // server; a failure now leaves its outcome unknown.
        let lost = |e: std::io::Error| {
            let reason = if matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                UnknownReason::ReplyTimeout
            } else {
                UnknownReason::ConnectionLost
            };
            Failure::Transport(TransportError::maybe_sent(reason, e.to_string()))
        };
        progress.request_left = true;
        // From here on, a mutation (this request, or one an earlier hop
        // sent) may have taken effect. A refusal would be journaled as a
        // call that never happened, so no outcome after this point may be
        // one: `refuse_body` keeps the answer without its body, and
        // `unreadable` makes the outcome unknown.
        let effect_possible = sent_mutation || !is_query_method(method);
        set_timeouts(&stream.sock, deadline).map_err(lost)?;
        stream.write_all(head.as_bytes()).map_err(lost)?;
        if let Some(b) = body {
            stream.write_all(b.as_bytes()).map_err(lost)?;
        }
        stream.flush().map_err(lost)?;

        let mut reader = BufReader::new(Deadlined { stream, deadline });
        let (status, headers) = loop {
            // A head over its cap reads as invalid data. Either way the
            // outcome is already unknown for a query; for a mutation, the
            // more precise reason is that the answer could not be read.
            let raw = read_head(&mut reader).map_err(|e| {
                if effect_possible && e.kind() == std::io::ErrorKind::InvalidData {
                    unreadable(effect_possible, e.to_string())
                } else {
                    lost(e)
                }
            })?;
            let (status, headers) =
                parse_head(&raw).map_err(|d| unreadable(effect_possible, d.message))?;
            // Informational answers (103 Early Hints) precede the real one.
            if (100..200).contains(&status) && status != 101 {
                continue;
            }
            break (status, headers);
        };
        let no_body = method == "HEAD" || status == 204 || status == 304;
        let cap = self.config.max_body_bytes.min(DEFAULT_BODY_BYTES);
        // `None` when the body is over the cap; it is not read further.
        let body = if no_body {
            Some(Vec::new())
        } else if header(&headers, "transfer-encoding")
            .is_some_and(|te| te.to_ascii_lowercase().contains("chunked"))
        {
            read_chunked(&mut reader, cap, || {
                unreadable(effect_possible, "a chunked body basal cannot read".into())
            })?
        } else if let Some(length) = header(&headers, "content-length") {
            let length: usize = length
                .trim()
                .parse()
                .map_err(|_| unreadable(effect_possible, "a bad Content-Length".into()))?;
            if length > cap {
                None
            } else {
                let mut body = vec![0; length];
                reader.read_exact(&mut body).map_err(lost)?;
                Some(body)
            }
        } else {
            let mut body = Vec::new();
            (&mut reader)
                .take(cap as u64 + 1)
                .read_to_end(&mut body)
                .map_err(lost)?;
            (body.len() <= cap).then_some(body)
        };
        let (body, body_error) = match body {
            Some(body) => (body, None),
            None => (
                Vec::new(),
                Some(if matches!(status, 301 | 302 | 303 | 307 | 308) {
                    // A redirect is decided from its headers, not its body.
                    codes::TOO_LARGE
                } else {
                    refuse_body(effect_possible, too_large(cap))?
                }),
            ),
        };
        Ok(Response {
            status,
            headers,
            body,
            body_error,
            effect_possible,
        })
    }
}

fn too_large(cap: usize) -> Denial {
    Denial::new(
        codes::TOO_LARGE,
        format!("the response body is larger than {cap} bytes"),
    )
}

/// Bounds the next read or write by what is left of the deadline.
fn set_timeouts(sock: &TcpStream, deadline: Instant) -> std::io::Result<()> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the fetch ran past its deadline",
        ));
    }
    sock.set_read_timeout(Some(left))?;
    sock.set_write_timeout(Some(left))
}

/// A TLS stream whose every read is bounded by the fetch's deadline, so a
/// server trickling bytes cannot hold a fetch past it.
struct Deadlined {
    stream: StreamOwned<ClientConnection, TcpStream>,
    deadline: Instant,
}

impl Read for Deadlined {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        set_timeouts(&self.stream.sock, self.deadline)?;
        // Without authenticated TLS EOF a connection-framed body may be
        // truncated; preserve rustls's UnexpectedEof instead of inventing EOF.
        self.stream.read(buf)
    }
}

/// Reads a response head up to its blank line, at most [`MAX_HEAD_BYTES`].
fn read_head(reader: &mut impl BufRead) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = (&mut *reader)
            .take((MAX_HEAD_BYTES - head.len().min(MAX_HEAD_BYTES)) as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the connection closed before a response",
            ));
        }
        head.extend_from_slice(&line);
        if head.len() > MAX_HEAD_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the response head is too large",
            ));
        }
        if line == b"\r\n" || line == b"\n" {
            return Ok(head);
        }
    }
}

fn parse_head(raw: &[u8]) -> Result<(u16, Vec<(String, String)>), Denial> {
    // Every header occupies at least one line; the byte cap bounds allocation.
    let mut slots = vec![httparse::EMPTY_HEADER; raw.iter().filter(|b| **b == b'\n').count()];
    let mut response = httparse::Response::new(&mut slots);
    match response.parse(raw) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(Denial::new(codes::NET, "a response basal cannot read")),
    }
    let status = response
        .code
        .ok_or_else(|| Denial::new(codes::NET, "a response without a status"))?;
    let headers = response
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_ascii_lowercase(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();
    Ok((status, headers))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Reads a chunked body. Returns `None` once the body passes `cap`, and
/// the failure `bad` builds when the chunk framing cannot be read.
fn read_chunked(
    reader: &mut impl BufRead,
    cap: usize,
    bad: impl Fn() -> Failure,
) -> Result<Option<Vec<u8>>, Failure> {
    let lost = |e: std::io::Error| {
        Failure::Transport(TransportError::maybe_sent(
            UnknownReason::ConnectionLost,
            e.to_string(),
        ))
    };
    let mut body = Vec::new();
    loop {
        let mut line = Vec::new();
        (&mut *reader)
            .take(MAX_CHUNK_LINE_BYTES)
            .read_until(b'\n', &mut line)
            .map_err(lost)?;
        let text = String::from_utf8_lossy(&line);
        let size_text = text
            .trim()
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        let size = usize::from_str_radix(&size_text, 16).map_err(|_| bad())?;
        if size == 0 {
            // Trailers, up to the blank line.
            loop {
                let mut trailer = Vec::new();
                let n = (&mut *reader)
                    .take(MAX_TRAILER_LINE_BYTES)
                    .read_until(b'\n', &mut trailer)
                    .map_err(lost)?;
                if n == 0 || trailer == b"\r\n" || trailer == b"\n" {
                    return Ok(Some(body));
                }
            }
        }
        if body.len().saturating_add(size) > cap {
            return Ok(None);
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..]).map_err(lost)?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).map_err(lost)?;
    }
}

/// How far a fetch has got, across its redirect hops.
#[derive(Default)]
struct Progress {
    /// Whether any request of a method that is not a query has left: a
    /// later failure then may follow an effect.
    sent_mutation: bool,
    /// Whether the current hop's request has started to leave. A refusal
    /// before that point means the hop was never sent.
    request_left: bool,
}

/// Ends a fetch that will not follow a redirect any further. Before any
/// mutation has left, nothing has happened and the refusal stands. After
/// one has, the server may already have applied it, and a refusal would be
/// journaled as a call that never happened; the call instead ends with the
/// last redirect the server sent, as the answer the script sees and the
/// journal records.
fn stop(redirected: Option<Response>, refusal: Denial) -> Result<Value, Failure> {
    match redirected {
        Some(response) => finish(response),
        None => Err(refusal.into()),
    }
}

/// A response body basal will not return (`refusal` says why). Before any
/// request that may change something has left, nothing happened and the
/// call is refused. After one has, the server may have acted, so the call
/// keeps the server's status and headers and reports only the body as
/// refused: this returns the code for the answer's `body_error`.
fn refuse_body(effect_possible: bool, refusal: Denial) -> Result<&'static str, Failure> {
    if effect_possible {
        Ok(refusal.code)
    } else {
        Err(refusal.into())
    }
}

/// A response basal cannot read at all: an unparseable or oversized head,
/// or framing it cannot follow. Before any request that may change
/// something has left, that is a refusal. After one has, the server may
/// have acted and what it answered is unknown, so the outcome is unknown
/// too and the call goes to reconcile.
fn unreadable(effect_possible: bool, detail: String) -> Failure {
    if effect_possible {
        Failure::Transport(TransportError::maybe_sent(
            UnknownReason::ReplyUnreadable,
            detail,
        ))
    } else {
        Denial::new(codes::NET, detail).into()
    }
}

/// The script's view of a final response. Repeated headers are joined
/// with `, `; `Set-Cookie` is dropped, since nothing keeps cookies.
fn finish(response: Response) -> Result<Value, Failure> {
    let mut headers: Map<String, Value> = Map::new();
    for (name, value) in response.headers {
        if name == "set-cookie" {
            continue;
        }
        match headers.get_mut(&name) {
            Some(Value::String(existing)) => {
                existing.push_str(", ");
                existing.push_str(&value);
            }
            _ => {
                headers.insert(name, Value::String(value));
            }
        }
    }
    let body_error = match (response.body_error, String::from_utf8(response.body)) {
        (Some(code), _) => Err(code),
        (None, Ok(text)) => Ok(text),
        (None, Err(_)) => Err(refuse_body(
            response.effect_possible,
            Denial::new(codes::NOT_UTF8, "the response body is not UTF-8"),
        )?),
    };
    Ok(match body_error {
        Ok(body) => json!({ "status": response.status, "headers": headers, "body": body }),
        Err(code) => json!({
            "status": response.status,
            "headers": headers,
            "body": null,
            "body_error": code,
        }),
    })
}
