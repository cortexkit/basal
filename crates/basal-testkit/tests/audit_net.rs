use basal_host::TransportError;
use basal_host::builtins::Failure;
use basal_host::builtins::net::{Client, HostRule, NetConfig, StaticResolver, parse_request};
use basal_testkit::https::{Reply, TestServer};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

fn config(server: &TestServer) -> NetConfig {
    NetConfig {
        resolver: Arc::new(StaticResolver(BTreeMap::from([(
            "api.test".into(),
            vec![server.addr()],
        )]))),
        trust_anchors: Some(vec![server.trust_der()]),
        private_hosts: ["api.test".into()].into_iter().collect(),
        ..NetConfig::default()
    }
}
fn rules() -> Vec<HostRule> {
    vec![HostRule {
        host: "api.test".into(),
        methods: vec!["GET".into()],
    }]
}

#[test]
fn oversized_redirect_body_does_not_block_a_valid_hop() {
    let server = TestServer::start(|_| {
        BTreeMap::from([
            (
                "/redirect".into(),
                Reply::raw(
                    b"HTTP/1.1 302 Found\r\nLocation: /done\r\nContent-Length: 2000000\r\n\r\n",
                ),
            ),
            ("/done".into(), Reply::text(200, "done")),
        ])
    });
    let req = parse_request(&json!({"url":server.url("api.test", "/redirect")})).unwrap();
    assert_eq!(
        Client::new(config(&server)).fetch(&req, &rules()).unwrap()["body"],
        "done"
    );
}

#[test]
fn nonstandard_redirect_port_is_refused_before_a_second_connection() {
    let server = TestServer::start(|port| {
        BTreeMap::from([
            (
                "/redirect".into(),
                Reply::redirect(
                    302,
                    &format!(
                        "https://api.test:{}/done",
                        if port == 8443 { 8444 } else { 8443 }
                    ),
                ),
            ),
            ("/done".into(), Reply::text(200, "done")),
        ])
    });
    let req = parse_request(&json!({"url":server.url("api.test", "/redirect"), "options":{"headers":{"X-Secret":"secret"}}})).unwrap();
    let result = Client::new(config(&server)).fetch(&req, &rules());
    let seen = server.seen();
    assert_eq!(seen[0].header("x-secret"), Some("secret"));
    assert_eq!(seen.len(), 1, "a refused redirect must never connect");
    assert!(
        matches!(result, Err(Failure::Refused(ref d)) if d.code == "denied"),
        "{result:?}"
    );
}

#[test]
fn query_redirect_does_not_read_an_incomplete_body() {
    let server = TestServer::start(|_| {
        BTreeMap::from([
            (
                "/redirect".into(),
                Reply::raw(
                    b"HTTP/1.1 302 Found\r\nLocation: /done\r\nContent-Length: 100\r\n\r\nshort",
                ),
            ),
            ("/done".into(), Reply::text(200, "done")),
        ])
    });
    let req = parse_request(&json!({"url":server.url("api.test", "/redirect")})).unwrap();
    assert_eq!(
        Client::new(config(&server)).fetch(&req, &rules()).unwrap()["body"],
        "done"
    );
}

#[test]
fn unclean_tls_eof_cannot_fulfill_a_connection_framed_body() {
    use rustls::pki_types::PrivateKeyDer;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::Duration;
    let cert = rcgen::generate_simple_self_signed(vec!["api.test".into()]).unwrap();
    let der = cert.cert.der().to_vec();
    let key = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key)
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let (stop, cancelled) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let tcp = loop {
            match listener.accept() {
                Ok((tcp, _)) => break tcp,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if !matches!(
                        cancelled.recv_timeout(Duration::from_millis(10)),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ) {
                        return;
                    }
                }
                Err(e) => panic!("{e}"),
            }
        };
        tcp.set_nonblocking(false).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        tcp.set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(rustls::StreamOwned::new(
            rustls::ServerConnection::new(Arc::new(tls)).unwrap(),
            tcp,
        ));
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
        }
        let mut stream = reader.into_inner();
        stream.write_all(b"HTTP/1.1 200 OK\r\n\r\npartial").unwrap();
        stream.flush().unwrap();
        // Deliberately omit close_notify: the peer cannot authenticate EOF.
    });
    let config = NetConfig {
        resolver: Arc::new(StaticResolver(BTreeMap::from([(
            "api.test".into(),
            vec![addr],
        )]))),
        trust_anchors: Some(vec![der]),
        private_hosts: ["api.test".into()].into_iter().collect(),
        total_timeout: Duration::from_secs(10),
        ..NetConfig::default()
    };
    let req = parse_request(&json!({"url":"https://api.test/"})).unwrap();
    let result = Client::new(config).fetch(&req, &rules());
    let _ = stop.send(());
    worker.join().unwrap();
    assert!(
        matches!(
            result,
            Err(Failure::Transport(TransportError::Unavailable { .. }))
        ),
        "{result:?}"
    );
}
