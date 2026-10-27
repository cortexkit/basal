use super::*;

#[test]
fn flow_secrets_header_join_cannot_reassemble_a_released_key() {
    let response = Response {
        status: 200,
        headers: vec![
            ("X-Echo".into(), "opaque-prefix".into()),
            ("x-echo".into(), "opaque-suffix".into()),
        ],
        body: Vec::new(),
        body_error: None,
        effect_possible: false,
    };
    let value = finish_secret(response, b"opaque-prefix, opaque-suffix", "key").unwrap();
    assert_eq!(value["headers"]["x-echo"], "{{ck-secret:key}}");
}

#[test]
fn flow_secrets_header_names_are_scrubbed_before_case_normalization() {
    let key = "MixedCaseVaultKey";
    let (status, headers) =
        parse_head(format!("HTTP/1.1 200 OK\r\n{key}: value\r\n\r\n").as_bytes()).unwrap();
    assert_eq!(header(&headers, "mixedcasevaultkey"), Some("value"));
    let value = finish_secret(
        Response {
            status,
            headers,
            body: Vec::new(),
            body_error: None,
            effect_possible: false,
        },
        key.as_bytes(),
        "key",
    )
    .unwrap();
    assert_eq!(value["headers"]["{{ck-secret:key}}"], "value");
    assert!(value["headers"].get(key.to_ascii_lowercase()).is_none());
}

#[test]
fn flow_secrets_header_redaction_cannot_make_set_cookie_visible() {
    let response = Response {
        status: 200,
        headers: vec![("Set-Cookie".into(), "private-session".into())],
        body: Vec::new(),
        body_error: None,
        effect_possible: false,
    };
    let value = finish_secret(response, b"Set-Cookie", "key").unwrap();
    assert_eq!(value["headers"], serde_json::json!({}));
}

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
