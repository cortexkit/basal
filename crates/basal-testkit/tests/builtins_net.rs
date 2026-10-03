//! `net.fetch` against a local HTTPS server whose self-signed certificate
//! only these tests trust: HTTPS only, approved hosts and methods, every
//! redirect hop checked, addresses checked before connecting and the
//! connection pinned to them, size caps, and nothing added from anywhere.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use basal_host::builtins::net::{self, Client, HostRule, NetConfig, Resolve, StaticResolver};
use basal_host::builtins::{Failure, codes};
use basal_host::{Sent, TransportError, UnknownReason};
use basal_testkit::https::{Reply, TEST_HOSTS, TestServer};
use serde_json::{Value, json};

fn routes(server_port: u16) -> BTreeMap<String, Reply> {
    let mut r = BTreeMap::new();
    r.insert(
        "/hello".into(),
        Reply::text(200, "hello").header("Set-Cookie", "session=1"),
    );
    r.insert("/post".into(), Reply::text(201, "created"));
    r.insert(
        "/to-evil".into(),
        Reply::redirect(302, &format!("https://evil.test:{server_port}/secret")),
    );
    r.insert("/secret".into(), Reply::text(200, "secret"));
    r.insert(
        "/post-to-evil".into(),
        Reply {
            status: 307,
            headers: vec![(
                "Location".into(),
                format!("https://evil.test:{server_port}/secret"),
            )],
            body: b"moved".to_vec(),
            chunked: false,
            raw: None,
        },
    );
    r.insert("/to-hello".into(), Reply::redirect(301, "/hello"));
    r.insert(
        "/to-other".into(),
        Reply::redirect(307, &format!("https://other.test:{server_port}/hello")),
    );
    for i in 0..7 {
        r.insert(
            format!("/loop{i}"),
            Reply::redirect(302, &format!("/loop{}", i + 1)),
        );
    }
    r.insert(
        "/big".into(),
        Reply {
            status: 200,
            headers: Vec::new(),
            body: vec![b'x'; 2 * 1024 * 1024],
            chunked: false,
            raw: None,
        },
    );
    r.insert(
        "/big-chunked".into(),
        Reply {
            status: 200,
            headers: Vec::new(),
            body: vec![b'x'; 2 * 1024 * 1024],
            chunked: true,
            raw: None,
        },
    );
    r.insert(
        "/small-chunked".into(),
        Reply {
            status: 200,
            headers: Vec::new(),
            body: b"chunked body".to_vec(),
            chunked: true,
            raw: None,
        },
    );
    r.insert(
        "/binary".into(),
        Reply {
            status: 200,
            headers: Vec::new(),
            body: vec![0xff, 0xfe, 0x00],
            chunked: false,
            raw: None,
        },
    );
    r.insert(
        "/garbage".into(),
        Reply::raw(b"this is not an HTTP response\r\n\r\n"),
    );
    r.insert(
        "/bad-length".into(),
        Reply::raw(b"HTTP/1.1 200 OK\r\nContent-Length: lots\r\n\r\nbody"),
    );
    r
}

fn server() -> TestServer {
    TestServer::start(routes)
}

/// A client whose resolver sends every test host to the server, which is
/// on the loopback interface, so the test hosts are exempt from the
/// public-address rule and the server's certificate is the only trust
/// anchor.
fn client(server: &TestServer) -> Client {
    let mut table = BTreeMap::new();
    for host in TEST_HOSTS {
        table.insert(host.to_owned(), vec![server.addr()]);
    }
    Client::new(NetConfig {
        resolver: Arc::new(StaticResolver(table)),
        trust_anchors: Some(vec![server.trust_der()]),
        private_hosts: TEST_HOSTS.iter().map(|h| (*h).to_owned()).collect(),
        connect_timeout: Duration::from_secs(2),
        total_timeout: Duration::from_secs(10),
        ..NetConfig::default()
    })
}

fn rules(entries: &[(&str, &[&str])]) -> Vec<HostRule> {
    entries
        .iter()
        .map(|(h, m)| HostRule {
            host: (*h).to_owned(),
            methods: m.iter().map(|m| (*m).to_owned()).collect(),
        })
        .collect()
}

const GET_HEAD: &[&str] = &["GET", "HEAD"];

fn fetch(client: &Client, rules: &[HostRule], args: Value) -> Result<Value, Failure> {
    let request = net::parse_request(&args).map_err(Failure::Refused)?;
    client.fetch(&request, rules)
}

fn refused(result: Result<Value, Failure>) -> &'static str {
    match result {
        Err(Failure::Refused(d)) => d.code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn fetches_from_an_approved_host_and_adds_nothing() {
    let server = server();
    let client = client(&server);
    let rules = rules(&[("api.test", GET_HEAD)]);
    let got = fetch(
        &client,
        &rules,
        json!({ "url": server.url("api.test", "/hello"), "options": { "headers": { "Accept": "text/plain" } } }),
    )
    .expect("fetch");
    assert_eq!(got["status"], 200);
    assert_eq!(got["body"], "hello");
    assert_eq!(got["headers"]["content-type"], "text/plain");
    assert!(got["headers"].get("set-cookie").is_none(), "{got}");
    let head = fetch(
        &client,
        &rules,
        json!({ "url": server.url("api.test", "/hello"), "options": { "method": "HEAD" } }),
    )
    .expect("head");
    assert_eq!(head["body"], "");
    let chunked = fetch(
        &client,
        &rules,
        json!({ "url": server.url("api.test", "/small-chunked") }),
    )
    .expect("chunked");
    assert_eq!(chunked["body"], "chunked body");

    let seen = server.seen();
    assert_eq!(seen[0].header("accept"), Some("text/plain"));
    assert_eq!(
        seen[0].header("host"),
        Some(format!("api.test:{}", server.port()).as_str())
    );
    for s in &seen {
        assert_eq!(s.header("cookie"), None);
        assert_eq!(s.header("authorization"), None);
        assert_eq!(s.header("proxy-authorization"), None);
    }

    // A script may not add cookies or credentials either.
    for header in ["Cookie", "authorization", "Proxy-Authorization", "Host"] {
        assert_eq!(
            refused(fetch(
                &client,
                &rules,
                json!({ "url": server.url("api.test", "/hello"), "options": { "headers": { header: "x" } } }),
            )),
            codes::DENIED,
            "{header}"
        );
    }
}

#[test]
fn only_https_urls_are_fetched() {
    let server = server();
    let client = client(&server);
    let rules = rules(&[("api.test", GET_HEAD)]);
    let url = format!("http://api.test:{}/hello", server.port());
    assert_eq!(
        refused(fetch(&client, &rules, json!({ "url": url }))),
        codes::DENIED
    );
    assert_eq!(server.connections(), 0);
}

#[test]
fn a_host_or_method_not_approved_is_refused_before_connecting() {
    let server = server();
    let client = client(&server);
    let rules = rules(&[("api.test", GET_HEAD), ("other.test", &["POST"])]);
    assert_eq!(
        refused(fetch(
            &client,
            &rules,
            json!({ "url": server.url("evil.test", "/hello") })
        )),
        codes::DENIED
    );
    assert_eq!(
        refused(fetch(
            &client,
            &rules,
            json!({ "url": server.url("api.test", "/post"), "options": { "method": "POST", "body": "x" } })
        )),
        codes::DENIED
    );
    assert_eq!(server.connections(), 0);
    // The method approved for other.test is sent, with its body.
    let posted = fetch(
        &client,
        &rules,
        json!({ "url": server.url("other.test", "/post"), "options": { "method": "POST", "body": "payload" } }),
    )
    .expect("post");
    assert_eq!(posted["status"], 201);
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].body, b"payload");
}

/// A `GET` has changed nothing, so a redirect it may not follow refuses the
/// whole call.
#[test]
fn a_redirect_to_an_unapproved_host_is_refused() {
    let server = server();
    let client = client(&server);
    let rules = rules(&[("api.test", GET_HEAD)]);
    assert_eq!(
        refused(fetch(
            &client,
            &rules,
            json!({ "url": server.url("api.test", "/to-evil"), "options": { "method": "GET" } })
        )),
        codes::DENIED
    );
    let paths: Vec<String> = server.seen().into_iter().map(|s| s.path).collect();
    assert_eq!(paths, ["/to-evil"], "the unapproved hop was requested");
}

/// A `POST` the server answered may have taken effect, so a redirect it may
/// not follow does not turn the call into a refusal: the call answers with
/// the redirect itself, and the unapproved host is never contacted.
#[test]
fn a_post_redirected_to_an_unapproved_host_returns_the_redirect() {
    let server = server();
    let client = client(&server);
    let rules = rules(&[("api.test", &["POST"])]);
    let got = fetch(
        &client,
        &rules,
        json!({ "url": server.url("api.test", "/post-to-evil"), "options": { "method": "POST", "body": "payload" } }),
    )
    .expect("the redirect is the answer");
    assert_eq!(got["status"], 307, "{got}");
    assert_eq!(
        got["headers"]["location"],
        format!("https://evil.test:{}/secret", server.port())
    );
    assert_eq!(got["body"], "moved");
    let seen: Vec<(String, String)> = server
        .seen()
        .into_iter()
        .map(|s| (s.method, s.path))
        .collect();
    assert_eq!(
        seen,
        [("POST".to_owned(), "/post-to-evil".to_owned())],
        "the unapproved hop was requested"
    );
}

#[test]
fn redirects_are_followed_by_hand_within_the_allowlist_and_the_hop_limit() {
    let server = server();
    let client = client(&server);
    let rules = rules(&[("api.test", GET_HEAD), ("other.test", GET_HEAD)]);
    let same = fetch(
        &client,
        &rules,
        json!({ "url": server.url("api.test", "/to-hello") }),
    )
    .expect("same host");
    assert_eq!(same["body"], "hello");
    let across = fetch(
        &client,
        &rules,
        json!({ "url": server.url("api.test", "/to-other"), "options": { "headers": { "X-Token": "t" } } }),
    )
    .expect("other host");
    assert_eq!(across["body"], "hello");
    // A script's headers go only to the host it named.
    let last = server.seen().pop().expect("seen");
    assert_eq!(
        last.header("host"),
        Some(format!("other.test:{}", server.port()).as_str())
    );
    assert_eq!(last.header("x-token"), None);
    // Five hops are followed; the sixth is refused.
    match fetch(
        &client,
        &rules,
        json!({ "url": server.url("api.test", "/loop0") }),
    ) {
        Err(Failure::Refused(d)) => assert_eq!(d.code, codes::NET, "{d:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_host_resolving_to_loopback_is_refused_before_connecting() {
    let server = server();
    let mut table = BTreeMap::new();
    table.insert(
        "api.test".to_owned(),
        vec![SocketAddr::new(IpAddr::from([127, 0, 0, 1]), server.port())],
    );
    let client = Client::new(NetConfig {
        resolver: Arc::new(StaticResolver(table)),
        trust_anchors: Some(vec![server.trust_der()]),
        // No exemption: production's rule.
        private_hosts: BTreeSet::new(),
        ..NetConfig::default()
    });
    let rules = rules(&[("api.test", GET_HEAD)]);
    assert_eq!(
        refused(fetch(
            &client,
            &rules,
            json!({ "url": server.url("api.test", "/hello") })
        )),
        codes::DENIED
    );
    assert_eq!(server.connections(), 0);
}

#[test]
fn private_and_reserved_addresses_are_not_public() {
    for text in [
        "0.0.0.0",
        "10.1.2.3",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "192.168.1.1",
        "224.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "fe80::1",
        "fc00::1",
        "ff02::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:169.254.169.254",
        "64:ff9b::a00:1",
        "2002:a00:1::1",
    ] {
        let ip: IpAddr = text.parse().expect("ip");
        assert!(!net::is_public(ip), "{text} counted as public");
    }
    for text in [
        "8.8.8.8",
        "1.1.1.1",
        "2606:4700:4700::1111",
        "::ffff:8.8.8.8",
    ] {
        let ip: IpAddr = text.parse().expect("ip");
        assert!(net::is_public(ip), "{text} counted as private");
    }
}

#[test]
fn ip_literal_and_wildcard_hosts_are_refused() {
    for host in [
        "127.0.0.1",
        "[::1]",
        "2130706433",
        "127.1",
        "0x7f.1",
        "*.example.com",
    ] {
        let url = format!("https://{host}/x");
        assert!(net::parse_url(&url).is_err(), "{url}");
    }
    assert!(net::parse_url("https://user:pw@api.test/x").is_err());
}

/// A resolver that answers the server's address once and an unreachable
/// documentation address after that, counting its calls.
struct Rebinding {
    first: SocketAddr,
    calls: AtomicUsize,
}

impl Resolve for Rebinding {
    fn resolve(&self, _: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(vec![self.first])
        } else {
            Ok(vec![SocketAddr::new(IpAddr::from([192, 0, 2, 1]), port)])
        }
    }
}

#[test]
fn the_connection_goes_to_the_address_that_was_checked() {
    let server = server();
    let resolver = Arc::new(Rebinding {
        first: server.addr(),
        calls: AtomicUsize::new(0),
    });
    let client = Client::new(NetConfig {
        resolver: resolver.clone(),
        trust_anchors: Some(vec![server.trust_der()]),
        private_hosts: ["api.test".to_owned()].into_iter().collect(),
        connect_timeout: Duration::from_secs(1),
        total_timeout: Duration::from_secs(5),
        ..NetConfig::default()
    });
    let rules = rules(&[("api.test", GET_HEAD)]);
    let got = fetch(
        &client,
        &rules,
        json!({ "url": server.url("api.test", "/hello") }),
    )
    .expect("fetch");
    assert_eq!(got["body"], "hello");
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "resolved twice");
}

#[test]
fn an_oversized_body_is_refused() {
    let server = server();
    let client = client(&server);
    let rules = rules(&[("api.test", GET_HEAD)]);
    for path in ["/big", "/big-chunked"] {
        assert_eq!(
            refused(fetch(
                &client,
                &rules,
                json!({ "url": server.url("api.test", path) })
            )),
            codes::TOO_LARGE,
            "{path}"
        );
    }
}

/// Once a `POST` has reached the server, a body basal will not return (over
/// the cap, or not UTF-8) does not make the call a refusal: the server may
/// have acted, so the script gets its status and headers, `body: null` and
/// `body_error` naming why. A `GET` that changed nothing is still refused.
#[test]
fn a_post_whose_body_is_refused_still_answers_with_status_and_headers() {
    let server = server();
    let client = client(&server);
    let rules = rules(&[("api.test", &["GET", "POST"])]);
    for (path, code) in [
        ("/big", codes::TOO_LARGE),
        ("/big-chunked", codes::TOO_LARGE),
        ("/binary", codes::NOT_UTF8),
    ] {
        let got = fetch(
            &client,
            &rules,
            json!({ "url": server.url("api.test", path), "options": { "method": "POST", "body": "x" } }),
        )
        .unwrap_or_else(|e| panic!("{path}: the POST was answered as {e:?}"));
        assert_eq!(got["status"], 200, "{path}: {got}");
        assert_eq!(got["body"], Value::Null, "{path}: {got}");
        assert_eq!(got["body_error"], code, "{path}: {got}");
        assert!(got["headers"].is_object(), "{path}: {got}");
        assert_eq!(
            refused(fetch(
                &client,
                &rules,
                json!({ "url": server.url("api.test", path) })
            )),
            code,
            "GET {path}"
        );
    }
}

/// Once a `POST` has reached the server, an answer basal cannot read at all
/// (no HTTP head, a malformed Content-Length) leaves the outcome unknown,
/// so it is a transport failure that may have been sent, for reconcile, not
/// a refusal. A `GET` that changed nothing is still refused.
#[test]
fn a_post_whose_answer_is_unreadable_has_an_unknown_outcome() {
    let server = server();
    let client = client(&server);
    let rules = rules(&[("api.test", &["GET", "POST"])]);
    for path in ["/garbage", "/bad-length"] {
        match fetch(
            &client,
            &rules,
            json!({ "url": server.url("api.test", path), "options": { "method": "POST", "body": "x" } }),
        ) {
            Err(Failure::Transport(TransportError::Unavailable {
                sent: Sent::Maybe(UnknownReason::ReplyUnreadable),
                ..
            })) => {}
            other => panic!("{path}: the POST was answered as {other:?}"),
        }
        assert_eq!(
            refused(fetch(
                &client,
                &rules,
                json!({ "url": server.url("api.test", path) })
            )),
            codes::NET,
            "GET {path}"
        );
    }
}

#[test]
fn a_certificate_not_trusted_is_refused() {
    let server = server();
    let mut table = BTreeMap::new();
    table.insert("api.test".to_owned(), vec![server.addr()]);
    // The bundled roots, as in production: the test certificate is not
    // among them.
    let client = Client::new(NetConfig {
        resolver: Arc::new(StaticResolver(table)),
        private_hosts: ["api.test".to_owned()].into_iter().collect(),
        ..NetConfig::default()
    });
    let rules = rules(&[("api.test", GET_HEAD)]);
    assert_eq!(
        refused(fetch(
            &client,
            &rules,
            json!({ "url": server.url("api.test", "/hello") })
        )),
        codes::TLS
    );
}
