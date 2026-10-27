use super::*;

#[test]
fn review_secret_partial_responses_are_final_refusals() {
    for (status, headers) in [
        (206, Vec::new()),
        (200, vec![("Content-Range".into(), "bytes 0-2/30".into())]),
    ] {
        let result = finish_secret(
            Response {
                status,
                headers,
                body: b"abc".to_vec(),
                body_error: None,
                effect_possible: false,
            },
            b"complete-secret-key",
            "key",
        );
        assert!(
            matches!(result, Err(Failure::Refused(denial)) if denial.code == "secret_partial_response")
        );
    }
    assert_eq!(
        finish(Response {
            status: 206,
            headers: Vec::new(),
            body: b"abc".to_vec(),
            body_error: None,
            effect_possible: false
        })
        .unwrap()["status"],
        206
    );
}

#[test]
fn review_secret_response_integrity_metadata_is_dropped_only_for_secret_fetches() {
    let headers = ["Content-Length", "ETag", "Digest", "Content-MD5"];
    let response = || Response {
        status: 200,
        headers: headers
            .iter()
            .map(|name| (name.to_string(), "metadata".into()))
            .chain([("Accept-Ranges".into(), "none".into())])
            .collect(),
        body: Vec::new(),
        body_error: None,
        effect_possible: false,
    };
    let ordinary = finish(response()).unwrap();
    let secret = finish_secret(response(), b"complete-secret-key", "key").unwrap();
    for header in headers {
        assert!(
            ordinary["headers"]
                .get(header.to_ascii_lowercase())
                .is_some()
        );
        assert!(secret["headers"].get(header.to_ascii_lowercase()).is_none());
    }
    assert_eq!(secret["headers"]["accept-ranges"], "none");
}

#[test]
fn review_escaped_secret_echoes_are_withheld_after_exact_redaction() {
    use base64::Engine;
    let key = "Opaque/Key \"Quoted\" Space Token";
    let percent = key.bytes().map(|b| format!("%{b:02x}")).collect::<String>();
    let encoded = base64::engine::general_purpose::STANDARD.encode(key.to_ascii_lowercase());
    let mut forms = vec![
        key.replace('/', "\\/"),
        key.chars()
            .map(|c| format!("\\u{:04x}", c as u32))
            .collect::<String>(),
        key.replace('/', "&sol;")
            .replace('"', "&quot;")
            .replace(' ', "&#32;"),
        key.bytes()
            .map(|b| format!("&#x{b:x};"))
            .collect::<String>(),
        key.replace('"', "\\\""),
        key.replace(' ', "+"),
        percent.replace('%', "%25"),
        key.to_ascii_lowercase(),
        format!("A{}\r\n{}", &encoded[..7], &encoded[7..]),
        format!("AB{}\\n{}", &encoded[..7], &encoded[7..]),
        format!("ABC{encoded}"),
    ];
    forms.push(
        key.replace("Opaque", "&#79;paque")
            .replace('/', "\\/")
            .replace(' ', "%2520"),
    );
    for form in forms {
        let value = finish_secret(
            Response {
                status: 200,
                headers: vec![("x-echo".into(), form.clone())],
                body: form.into_bytes(),
                body_error: None,
                effect_possible: false,
            },
            key.as_bytes(),
            "key",
        )
        .unwrap();
        assert!(value["body"].is_null(), "escaped echo was delivered");
        assert_eq!(value["body_error"], "secret_echoed");
        assert!(value["headers"].get("x-echo").is_none());
        assert_eq!(value["headers_error"], "secret_echoed");
    }
    let value = finish_secret(
        Response {
            status: 200,
            headers: Vec::new(),
            body: b"Opaque/Key".to_vec(),
            body_error: None,
            effect_possible: false,
        },
        key.as_bytes(),
        "key",
    )
    .unwrap();
    assert_eq!(
        value["body"], "Opaque/Key",
        "partial key substrings are not withheld"
    );
}

#[test]
fn review_echo_normalization_handles_named_entities_malformed_prefixes_and_limits() {
    for (key, body) in [
        ("opaque-fj-key", "opaque-&fjlig;-key"),
        ("secret", "&&#115;ecret"),
        ("secret", "%5Cu0073ecret"),
        ("A+B", "A%252BB"),
        ("a\"b\\c/d", "a\\\"b\\\\c\\/d"),
        ("secret", "eHNl\\nY3JldA=="),
    ] {
        let value = finish_secret(
            Response {
                status: 200,
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
                body_error: None,
                effect_possible: false,
            },
            key.as_bytes(),
            "key",
        )
        .unwrap();
        assert!(value["body"].is_null());
        assert_eq!(value["body_error"], "secret_echoed");
    }
    let mut guard = crate::secrets::EchoGuard::new(b"secret");
    assert!(guard.unsafe_echo(&vec![b'x'; 8 * 1024 * 1024 + 1]));
}

#[test]
fn review_echo_detection_does_not_treat_ordinary_identifiers_as_nested_payloads() {
    let body = serde_json::to_string(
        &(0..96)
            .map(|i| format!("{}{:016x}", "a".repeat(48), i))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let value = finish_secret(
        Response {
            status: 200,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            body_error: None,
            effect_possible: false,
        },
        b"opaque-credential-key",
        "key",
    )
    .unwrap();
    assert_eq!(value["body"], body);
    assert!(value.get("body_error").is_none());
}

#[test]
fn review_html_ascii_prefixes_of_combining_entities_are_withheld() {
    for (key, body) in [
        ("secret<", "secret&nvlt;"),
        ("secret>", "secret&nvgt;"),
        ("secret=", "secret&bne;"),
    ] {
        let value = finish_secret(
            Response {
                status: 200,
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
                body_error: None,
                effect_possible: false,
            },
            key.as_bytes(),
            "key",
        )
        .unwrap();
        assert!(value["body"].is_null());
        assert_eq!(value["body_error"], "secret_echoed");
    }
}

#[test]
fn review_nested_selective_fragments_preserve_literal_escape_text() {
    for (key, body) in [
        ("a%41b", "a%41%2562"),
        ("a%41b", "a%41%25%36%32"),
        ("a&amp;b", "a&amp;&amp;#98;"),
        (r"a\u0041b", r"a\u0041\u005cu0062"),
    ] {
        let value = finish_secret(
            Response {
                status: 200,
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
                body_error: None,
                effect_possible: false,
            },
            key.as_bytes(),
            "key",
        )
        .unwrap();
        assert!(value["body"].is_null());
        assert_eq!(value["body_error"], "secret_echoed");
    }
}

#[test]
fn review_base64_space_and_tab_wrapping_is_withheld() {
    for body in ["c2Vj cmV0", "c2Vj\tcmV0", "c2Vj&Tab;cmV0"] {
        let value = finish_secret(
            Response {
                status: 200,
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
                body_error: None,
                effect_possible: false,
            },
            b"secret",
            "key",
        )
        .unwrap();
        assert!(value["body"].is_null());
        assert_eq!(value["body_error"], "secret_echoed");
    }
}

#[test]
fn review_exact_redaction_is_bounded_and_does_not_mask_literal_partial_keys() {
    let mut key = vec![b'A'; 8192];
    key[8191] = b'B';
    let body = vec![b'A'; 64 * 1024];
    let mut remaining = 32;
    assert!(crate::secrets::redact_with_budget(&body, &key, "key", &mut remaining).is_err());
    let value = finish_secret(
        Response {
            status: 200,
            headers: Vec::new(),
            body: body.clone(),
            body_error: None,
            effect_possible: false,
        },
        &key,
        "key",
    )
    .unwrap();
    assert_eq!(value["body"].as_str().unwrap().as_bytes(), body);
    assert!(value.get("body_error").is_none());
}

#[test]
fn review_encoded_header_names_are_dropped_before_case_normalization() {
    use base64::Engine;
    let key = b"Opaque-Credential-Header";
    let name = format!(
        "A{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.to_ascii_lowercase())
    );
    let value = finish_secret(
        Response {
            status: 200,
            headers: vec![(name, "safe-value".into())],
            body: Vec::new(),
            body_error: None,
            effect_possible: false,
        },
        key,
        "key",
    )
    .unwrap();
    assert_eq!(value["headers"], serde_json::json!({}));
    assert_eq!(value["headers_error"], "secret_echoed");
}

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
