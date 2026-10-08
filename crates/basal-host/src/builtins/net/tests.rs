use super::*;

#[test]
fn relative_redirects_keep_embedded_urls_and_the_current_query_path() {
    let base = parse_url("https://api.test/dir/page?old=1").unwrap();
    for (location, expected) in [
        ("/login?next=https://x", "/login?next=https://x"),
        ("?page=2", "/dir/page?page=2"),
        ("#anchor", "/dir/page?old=1"),
        ("../next", "/next"),
    ] {
        assert_eq!(resolve_location(&base, location).unwrap().target, expected);
    }
}

#[test]
fn response_header_count_is_bounded_by_bytes_not_128_slots() {
    let mut head = b"HTTP/1.1 200 OK\r\n".to_vec();
    for _ in 0..200 {
        head.extend_from_slice(b"x: y\r\n");
    }
    head.extend_from_slice(b"\r\n");
    assert_eq!(parse_head(&head).unwrap().1.len(), 200);
}

#[test]
fn nonstandard_https_port_is_refused_before_dns() {
    let client = Client::new(NetConfig::default());
    let rules = [HostRule {
        host: "api.test".into(),
        methods: vec!["GET".into()],
    }];
    let req = parse_request(&json!({"url":"https://api.test:8443/"})).unwrap();
    let Err(Failure::Refused(denial)) = client.fetch(&req, &rules) else {
        panic!("port was not refused")
    };
    assert_eq!(denial.code, codes::DENIED);
    assert!(denial.message.contains("8443"));
}

#[test]
fn redirect_headers_require_the_same_https_origin() {
    let first = parse_url("https://api.test/").unwrap();
    assert!(same_origin(
        &first,
        &parse_url("https://API.test:443/next").unwrap()
    ));
    assert!(!same_origin(
        &first,
        &parse_url("https://api.test:8443/next").unwrap()
    ));
    assert!(!same_origin(
        &first,
        &parse_url("https://other.test/next").unwrap()
    ));
}

#[test]
fn dns_receives_the_fetch_deadline() {
    struct Resolver(Mutex<Option<Instant>>);
    use std::sync::Mutex;
    impl Resolve for Resolver {
        fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<SocketAddr>> {
            panic!("unbounded resolver path")
        }
        fn resolve_until(
            &self,
            _: &str,
            _: u16,
            deadline: Instant,
        ) -> std::io::Result<Vec<SocketAddr>> {
            *self.0.lock().unwrap() = Some(deadline);
            Ok(vec![])
        }
    }
    let resolver = Arc::new(Resolver(Mutex::new(None)));
    let client = Client::new(NetConfig {
        resolver: resolver.clone(),
        ..NetConfig::default()
    });
    let deadline = Instant::now();
    assert!(
        client
            .addresses(&parse_url("https://api.test/").unwrap(), deadline)
            .is_err()
    );
    assert_eq!(*resolver.0.lock().unwrap(), Some(deadline));
}
