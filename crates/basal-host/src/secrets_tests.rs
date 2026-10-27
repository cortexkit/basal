use super::*;
use crate::builtins::{BuiltinHost, Grant};
use crate::transport::{Transport, WireError};
use crate::{CallClass, CallRequest, Dispatched, Host, TransportError};
use basal_proto::{CallKind, JsonText, Primitive};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KEY: &str = "opaque-api-key+/with%bytes";

#[derive(Default)]
struct Vault(Mutex<Vec<Value>>);
impl Transport for Vault {
    fn catalog(&self) -> Result<Value, WireError> {
        Ok(json!({}))
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        assert_eq!((module, op), ("claustrum", "secret.use"));
        self.0.lock().unwrap().push(params);
        Ok(
            json!({"status":"released","kind":"agent_api_key","key_b64url":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(KEY),"location":"header:X-Api-Key"}),
        )
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("no tool route")
    }
}

fn request(args: Value) -> CallRequest {
    CallRequest {
        flow_id: "test-flow".into(),
        flow_version: Some(2),
        run_id: "run".into(),
        position: 0,
        kind: CallKind::Primitive(Primitive::NetFetch),
        args: JsonText::new(args.to_string()).unwrap(),
        idempotency_key: "key".into(),
        attempt: 1,
    }
}

fn description() -> crate::ScopeDescription {
    serde_json::from_value(json!({"status":"live","scope_epoch":7,"daemon_incarnation":"d",
        "owner_synced":true,"owner_configured":true,"scope":{
            "owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"flow:scope",
            "scope_epoch":7,"kind":"head","owner_authorized":true,
            "attributes":{"flow_id":"test-flow","flow_version":2,"flow_code_hash":HASH,"flow_install_id":"install-1"}}})).unwrap()
}

#[test]
fn flow_secrets_scope_missing_or_mismatched_identity_never_calls_vault() {
    let good = description();
    let scope = crate::flow_scope::FlowScope {
        owner: good.scope.as_ref().unwrap().owner.clone(),
        scope_ref: "flow:scope".into(),
        epoch: 7,
    };
    let host = crate::mock::MockHost::new();
    let vault = Vault::default();
    let mut variants = Vec::new();
    for field in [
        "flow_id",
        "flow_version",
        "flow_code_hash",
        "flow_install_id",
    ] {
        let mut value = serde_json::to_value(&good).unwrap();
        value["scope"]["attributes"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        variants.push((
            serde_json::from_value(value).unwrap(),
            "secret_scope_missing",
        ));
    }
    for (field, value) in [
        ("flow_version", json!(3)),
        ("flow_code_hash", json!("b".repeat(64))),
        ("flow_id", json!("another-flow")),
    ] {
        let mut description = serde_json::to_value(&good).unwrap();
        description["scope"]["attributes"][field] = value;
        variants.push((
            serde_json::from_value(description).unwrap(),
            "secret_scope_mismatch",
        ));
    }
    for (description, expected) in variants {
        host.set_scope_description(scope.owner.clone(), &scope.scope_ref, Ok(description));
        let error = use_attested(
            &host,
            &vault,
            &scope,
            &request(json!({})),
            HASH,
            json!({"name":"key"}),
        )
        .unwrap_err();
        assert!(matches!(error, WireError::Refused { code, .. } if code == expected));
    }
    assert!(vault.0.lock().unwrap().is_empty());
    host.set_scope_description(scope.owner.clone(), &scope.scope_ref, Ok(good.clone()));
    use_attested(
        &host,
        &vault,
        &scope,
        &request(json!({})),
        HASH,
        json!({"name":"key"}),
    )
    .unwrap();
    assert_eq!(
        vault.0.lock().unwrap()[0]["stamp"],
        serde_json::to_value(good.scope.unwrap()).unwrap()
    );
}

struct CountingHost(Mutex<usize>);
struct Logs(Arc<Mutex<String>>);
impl tracing::field::Visit for Logs {
    fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        write!(self.0.lock().unwrap(), "{value:?}").unwrap();
    }
}
impl tracing::Subscriber for Logs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        event.record(&mut Logs(self.0.clone()));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
impl Host for CountingHost {
    fn secret_use(&self, _: &CallRequest, _: &str, _: Value) -> Result<Value, WireError> {
        *self.0.lock().unwrap() += 1;
        Err(WireError::RefusedDetails {
            code: "not_found".into(),
            message: KEY.into(),
            detail: json!({"unsafe":KEY}),
        })
    }
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> Result<Dispatched, TransportError> {
        panic!("no forwarding")
    }
    crate::live_host_services!();
}

fn grant() -> Grant {
    Grant { hosts: vec![crate::builtins::net::HostRule { host: "x.test".into(), methods: vec!["GET".into(), "POST".into()] }],
        secrets: vec![serde_json::from_value(json!({"name":"key","target":{"scheme":"https","host":"x.test","path":"/repos/a"},"methods":["GET"]})).unwrap()],
        ..Grant::default() }
}

#[test]
fn flow_secrets_scope_and_declaration_refuse_before_release_and_errors_drop_vault_text() {
    let carrier = Arc::new(CountingHost(Mutex::new(0)));
    let host = BuiltinHost::default().with_secret_host(carrier.clone());
    for (url, method, name, code) in [
        (
            "https://x.test/repos/ab",
            "GET",
            "key",
            "secret_out_of_scope",
        ),
        (
            "https://x.test/repos/a",
            "POST",
            "key",
            "secret_out_of_scope",
        ),
        (
            "https://x.test/repos/a",
            "GET",
            "missing",
            "secret_not_declared",
        ),
        (
            "https://x.test/repos/a/%2e%2e/other",
            "GET",
            "key",
            "secret_out_of_scope",
        ),
    ] {
        let envelope = crate::builtins::envelope(
            &json!({"url":url,"options":{"method":method,"secret":name}}),
            &grant(),
        );
        let Dispatched::Completed(outcome) = host.dispatch(&request(envelope)).unwrap() else {
            panic!("synchronous refusal")
        };
        assert_eq!(
            serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
            code
        );
    }
    assert_eq!(*carrier.0.lock().unwrap(), 0);
    let envelope = crate::builtins::envelope(
        &json!({"url":"https://x.test/repos/a","options":{"secret":"key"}}),
        &grant(),
    );
    let logs = Arc::new(Mutex::new(String::new()));
    let result =
        tracing::subscriber::with_default(Logs(logs.clone()), || host.dispatch(&request(envelope)));
    let Dispatched::Completed(outcome) = result.unwrap() else {
        panic!("synchronous refusal")
    };
    assert_eq!(*carrier.0.lock().unwrap(), 1);
    assert!(!outcome.value.as_str().contains(KEY));
    assert!(!logs.lock().unwrap().contains(KEY));
    assert_eq!(
        serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
        "not_found"
    );
}

#[test]
fn flow_secrets_redaction_covers_raw_base64url_padding_and_selective_percent_forms() {
    let placeholder = b"{{ck-secret:key}}";
    let mut forms = vec![KEY.as_bytes().to_vec()];
    for engine in [
        &base64::engine::general_purpose::STANDARD,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE,
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ] {
        forms.push(engine.encode(KEY).into_bytes());
        forms.push(
            engine
                .encode(format!("benign-prefix:{KEY}:benign-suffix"))
                .into_bytes(),
        );
    }
    forms.push(
        KEY.bytes()
            .map(|b| format!("%{b:02X}"))
            .collect::<String>()
            .into_bytes(),
    );
    forms.push(
        KEY.bytes()
            .map(|b| format!("%{b:02x}"))
            .collect::<String>()
            .into_bytes(),
    );
    forms.push(format!("%6f{}", &KEY[1..]).into_bytes());
    for form in forms {
        let input = [b"prefix=".as_slice(), &form, b"&suffix"].concat();
        assert_eq!(
            redact(&input, KEY.as_bytes(), "key"),
            [b"prefix=".as_slice(), placeholder, b"&suffix"].concat()
        );
    }
    assert_eq!(redact(b"%25%5A", b"%25Z", "key"), placeholder);
}

struct InstallVault {
    capability: bool,
    refusal: Option<&'static str>,
    calls: Mutex<Vec<(String, Value)>>,
}
impl Transport for InstallVault {
    fn catalog(&self) -> Result<Value, WireError> {
        let provides = if self.capability {
            vec![crate::core_consent::FLOW_INSTALL_SECRETS_V1]
        } else {
            vec![]
        };
        Ok(
            json!({"modules":[{"module_id":"prefrontal-core","capabilities":{"provides":provides}}]}),
        )
    }
    fn management(&self, _: &str, op: &str, params: Value) -> Result<Value, WireError> {
        self.calls.lock().unwrap().push((op.into(), params));
        if op == "secret.request_flow_approval" {
            if let Some(code) = self.refusal {
                return Err(WireError::Refused {
                    code: code.into(),
                    message: KEY.into(),
                });
            }
            Ok(
                json!({"status":"approval_required","request_id":"request-1","statement_b64url":"e30","expires_at_ms":123456}),
            )
        } else {
            assert_eq!(op, "flow.install_request");
            Ok(json!({"outcome":"card_raised","flow_id":"test-flow","version":2,"attempt":1}))
        }
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("unscoped management only")
    }
}

fn card() -> crate::InstallCard {
    crate::InstallCard {
        card_id: "card".into(),
        flow_id: "test-flow".into(),
        version: 2,
        fields: json!({"install_id":"install-1","code_hash":HASH,"purpose":"Read an authenticated API",
            "wire_author":{"operator":true},"code":{"script":"return 1;","manifest":json!({
                "secrets":[{"name":"key","target":{"scheme":"https","host":"x.test","path":"/repos/a"},"methods":["GET"]}]
            }).to_string()}}),
    }
}

#[test]
fn flow_secrets_install_requires_core_capability_and_relays_vault_statements_unchanged() {
    use crate::Consent;
    for capability in [false, true] {
        let vault = Arc::new(InstallVault {
            capability,
            refusal: None,
            calls: Mutex::new(Vec::new()),
        });
        let consent = crate::core_consent::CoreConsent::new(vault.clone());
        let result = consent.raise(&card());
        let calls = vault.calls.lock().unwrap();
        if !capability {
            assert!(
                matches!(result, Err(crate::ConsentError::Secret { code, retryable: false }) if code == "install_secrets_unsupported")
            );
            assert!(calls.is_empty());
        } else {
            result.unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].0, "secret.request_flow_approval");
            assert_eq!(
                calls[0].1["flow"],
                json!({"flow_id":"test-flow","version":2,"code_hash":HASH,"install_id":"install-1"})
            );
            assert_eq!(calls[1].1["flow_install"]["install_id"], "install-1");
            assert_eq!(
                calls[1].1["flow_install"]["secrets"],
                json!([{"request_id":"request-1","statement_b64url":"e30"}])
            );
        }
    }
}

#[test]
fn flow_secrets_install_vault_refusals_keep_codes_not_provider_details() {
    use crate::Consent;
    for (code, retryable) in [
        ("invalid_params", false),
        ("limit_exceeded", false),
        ("not_found", false),
        ("secret_setup_incomplete", false),
        ("pending_limit", true),
        ("store_error", true),
    ] {
        let vault = Arc::new(InstallVault {
            capability: true,
            refusal: Some(code),
            calls: Mutex::new(Vec::new()),
        });
        let error = crate::core_consent::CoreConsent::new(vault.clone())
            .raise(&card())
            .unwrap_err();
        assert!(!error.to_string().contains(KEY));
        assert!(
            matches!(error, crate::ConsentError::Secret { code: actual, retryable: actual_retryable } if actual == code && actual_retryable == retryable)
        );
        assert_eq!(vault.calls.lock().unwrap().len(), 1);
    }
}

struct ReleaseHost(std::sync::atomic::AtomicUsize);
impl Host for ReleaseHost {
    fn secret_use(&self, _: &CallRequest, _: &str, _: Value) -> Result<Value, WireError> {
        let count = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(
            json!({"status":"released","kind":"agent_api_key","key_b64url":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(KEY),
            "location":if count == 0 { "authorization_bearer" } else { "header:X-Api-Key" }}),
        )
    }
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> Result<Dispatched, TransportError> {
        panic!("no forwarding")
    }
    crate::live_host_services!();
}

#[test]
fn flow_secrets_http_echo_is_scrubbed_before_script_delivery_and_never_follows_redirects() {
    use std::io::{Read, Write};
    let certified = rcgen::generate_simple_self_signed(vec!["x.test".into()]).unwrap();
    let certificate = certified.cert.der().clone();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![certificate.clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()).into(),
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for (index, expected) in [
            format!("Authorization: Bearer {KEY}"),
            format!("X-Api-Key: {KEY}"),
        ]
        .into_iter()
        .enumerate()
        {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(config.clone())).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, socket);
            let mut input = Vec::new();
            while !input.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                input.push(byte[0]);
            }
            assert!(String::from_utf8_lossy(&input).contains(&expected));
            assert!(input.starts_with(b"GET /repos/a HTTP/1.1\r\n"));
            let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(KEY);
            let percent = KEY.bytes().map(|b| format!("%{b:02X}")).collect::<String>();
            let body = format!("raw={KEY};encoded={encoded};percent={percent}");
            let status = if index == 0 { "200 OK" } else { "302 Found" };
            write!(stream, "HTTP/1.1 {status}\r\nX-Echo: {KEY}\r\nLocation: /repos/a/next\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            stream.flush().unwrap();
        }
    });
    let carrier = Arc::new(ReleaseHost(std::sync::atomic::AtomicUsize::new(0)));
    let host = BuiltinHost::new(crate::builtins::BuiltinConfig {
        net: crate::builtins::net::NetConfig {
            resolver: Arc::new(crate::builtins::net::StaticResolver(
                std::collections::BTreeMap::from([("x.test".into(), vec![address])]),
            )),
            trust_anchors: Some(vec![certificate.to_vec()]),
            private_hosts: std::collections::BTreeSet::from(["x.test".into()]),
            ..crate::builtins::net::NetConfig::default()
        },
    })
    .with_secret_host(carrier.clone());
    for expected_status in [200, 302] {
        let frozen = crate::builtins::envelope(
            &json!({"url":"https://x.test/repos/a","options":{"secret":"key"}}),
            &grant(),
        );
        let context = request(frozen.clone());
        let Dispatched::Completed(outcome) = host.dispatch(&context).unwrap() else {
            panic!("synchronous HTTP reply")
        };
        assert_eq!(outcome.settlement, basal_proto::Settlement::Fulfilled);
        let value: Value = serde_json::from_str(outcome.value.as_str()).unwrap();
        assert_eq!(value["status"], expected_status);
        assert_eq!(value["headers"]["x-echo"], "{{ck-secret:key}}");
        if expected_status == 200 {
            assert!(
                value["body"]
                    .as_str()
                    .unwrap()
                    .matches("{{ck-secret:key}}")
                    .count()
                    >= 3
            );
        } else {
            // The redirect stays on the same host. A followed request would
            // attempt a third connection instead of returning this response.
            assert_eq!(value["headers"]["location"], "/repos/a/next");
        }
        assert!(!outcome.value.as_str().contains(KEY));
        assert!(!context.args.as_str().contains(KEY));
        assert_eq!(
            serde_json::from_str::<Value>(context.args.as_str()).unwrap(),
            frozen
        );
    }
    assert_eq!(carrier.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    server.join().unwrap();
}

#[test]
fn review_secret_request_controls_are_refused_before_vault_release() {
    let carrier = Arc::new(CountingHost(Mutex::new(0)));
    let host = BuiltinHost::default().with_secret_host(carrier.clone());
    let mut unsafe_args = Vec::new();
    for header in [
        "Range",
        "If-Range",
        "X-Original-URL",
        "X-Rewrite-URL",
        "X-Forwarded-For",
        "Forwarded",
        "X-Unapproved",
    ] {
        unsafe_args.push(json!({"url":"https://x.test/repos/a","options":{"secret":"key","headers":{(header):"unsafe"}}}));
    }
    for query in [
        "_method=DELETE",
        "%5Fmethod=DELETE",
        "%255fmethod=DELETE",
        "x=1;_METHOD=POST",
    ] {
        unsafe_args.push(
            json!({"url":format!("https://x.test/repos/a?{query}"),"options":{"secret":"key"}}),
        );
    }
    unsafe_args.push(json!({"url":"https://x.test/repos/a","options":{"secret":"key","method":"POST","headers":{"Content-Type":"application/x-www-form-urlencoded"},"body":"a=1&_method=DELETE"}}));
    unsafe_args.push(json!({"url":"https://x.test/repos/a","options":{"secret":"key","method":"POST","headers":{"Content-Type":"multipart/form-data; boundary=x"},"body":"--x"}}));
    let mut granted = grant();
    granted.secrets[0].methods = Some(vec!["GET".into(), "POST".into()]);
    for args in unsafe_args {
        let frozen = crate::builtins::envelope(&args, &granted);
        let Dispatched::Completed(outcome) = host.dispatch(&request(frozen)).unwrap() else {
            panic!("synchronous refusal")
        };
        assert_eq!(
            serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
            "secret_request_unsafe"
        );
    }
    assert_eq!(*carrier.0.lock().unwrap(), 0);
    for header in [
        "Accept",
        "Content-Type",
        "If-None-Match",
        "If-Modified-Since",
    ] {
        let args = json!({"url":"https://x.test/repos/a","options":{"secret":"key","headers":{(header):"safe"}}});
        host.dispatch(&request(crate::builtins::envelope(&args, &granted)))
            .unwrap();
    }
    assert_eq!(*carrier.0.lock().unwrap(), 4);
    let ordinary = crate::builtins::parse(Primitive::NetFetch, &json!({"url":"https://x.test/repos/a?_method=DELETE","options":{"headers":{"Range":"bytes=0-2"}}})).unwrap();
    assert!(crate::builtins::authorize(&ordinary, &granted).is_ok());
}

#[test]
fn review_secret_scope_readiness_uses_no_flow_scope_without_contacting_vault() {
    let good = description();
    let scope = crate::flow_scope::FlowScope {
        owner: good.scope.as_ref().unwrap().owner.clone(),
        scope_ref: "flow:scope".into(),
        epoch: 7,
    };
    let host = crate::mock::MockHost::new();
    let vault = Arc::new(Vault::default());
    for (field, value) in [
        ("status", json!("not_live")),
        ("scope", Value::Null),
        ("owner_synced", json!(false)),
        ("owner_configured", json!(false)),
    ] {
        let mut changed = serde_json::to_value(&good).unwrap();
        changed[field] = value;
        host.set_scope_description(
            scope.owner.clone(),
            &scope.scope_ref,
            Ok(serde_json::from_value(changed).unwrap()),
        );
        let error = use_attested(
            &host,
            vault.as_ref(),
            &scope,
            &request(json!({})),
            HASH,
            json!({"name":"key"}),
        )
        .unwrap_err();
        assert!(
            matches!(error, WireError::Typed(refusal) if refusal.reason == crate::flow_refusal::RefusalReason::NoFlowScope && refusal.readiness())
        );
    }
    let ops = crate::routing::ModuleOpsHost::new(
        vault.clone(),
        Arc::new(crate::subc_catalog::SubcCatalog::new(vault.clone())),
    );
    assert!(
        matches!(ops.secret_use(&request(json!({})), HASH, json!({})), Err(WireError::Typed(refusal)) if refusal.reason == crate::flow_refusal::RefusalReason::NoFlowScope)
    );
    assert!(vault.0.lock().unwrap().is_empty());
}

struct ErrorCarrier(Mutex<Option<WireError>>);
impl Host for ErrorCarrier {
    fn secret_use(&self, _: &CallRequest, _: &str, _: Value) -> Result<Value, WireError> {
        Err(self.0.lock().unwrap().take().unwrap())
    }
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> Result<Dispatched, TransportError> {
        panic!("not an op")
    }
    crate::live_host_services!();
}

#[test]
fn review_unknown_vault_refusals_are_final_and_keep_only_the_code() {
    for code in [
        "not_found",
        "invalid_params",
        "limit_exceeded",
        "secret_setup_incomplete",
        "pending_limit",
        "future_policy_refused",
    ] {
        let carrier = Arc::new(ErrorCarrier(Mutex::new(Some(WireError::RefusedDetails {
            code: code.into(),
            message: KEY.into(),
            detail: json!({"unsafe":KEY}),
        }))));
        let host = BuiltinHost::default().with_secret_host(carrier);
        let frozen = crate::builtins::envelope(
            &json!({"url":"https://x.test/repos/a","options":{"secret":"key"}}),
            &grant(),
        );
        let Dispatched::Completed(outcome) = host.dispatch(&request(frozen)).unwrap() else {
            panic!("final refusal")
        };
        assert_eq!(
            serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
            code
        );
        assert!(!outcome.value.as_str().contains(KEY));
    }
    for error in [
        WireError::Refused {
            code: "store_error".into(),
            message: KEY.into(),
        },
        WireError::NeverSent(KEY.into()),
    ] {
        let carrier = Arc::new(ErrorCarrier(Mutex::new(Some(error))));
        let host = BuiltinHost::default().with_secret_host(carrier);
        let frozen = crate::builtins::envelope(
            &json!({"url":"https://x.test/repos/a","options":{"secret":"key"}}),
            &grant(),
        );
        assert!(
            matches!(host.dispatch(&request(frozen)), Err(TransportError::Refused(refusal)) if refusal.reason == crate::flow_refusal::RefusalReason::SecretVaultUnavailable)
        );
    }
}

fn second_review_request_result(args: Value) -> (Value, usize) {
    let carrier = Arc::new(CountingHost(Mutex::new(0)));
    let host = BuiltinHost::default().with_secret_host(carrier.clone());
    let mut granted = grant();
    granted.secrets[0].methods = Some(vec!["GET".into(), "POST".into()]);
    let frozen = crate::builtins::envelope(&args, &granted);
    let Dispatched::Completed(outcome) = host.dispatch(&request(frozen)).unwrap() else {
        panic!("synchronous refusal")
    };
    let calls = *carrier.0.lock().unwrap();
    (serde_json::from_str(outcome.value.as_str()).unwrap(), calls)
}

#[test]
fn second_review_all_multipart_subtypes_are_refused_before_release() {
    for mime in [
        "multipart/mixed",
        "multipart/related",
        "Multipart/alternative; boundary=x",
        "multipart/byteranges",
        "multipart/form-data",
    ] {
        let (value, calls) = second_review_request_result(
            json!({"url":"https://x.test/repos/a","options":{"secret":"key","method":"POST","headers":{"Content-Type":mime},"body":"untrusted multipart payload"}}),
        );
        assert_eq!(value["code"], "secret_request_unsafe", "{mime}");
        assert_eq!(calls, 0);
    }
}

#[test]
fn second_review_php_normalized_method_names_are_refused_before_release() {
    for name in [
        ".method",
        "+method",
        "%20method",
        "%2Emethod",
        "[method",
        "%5Bmethod",
        "_MeThOd",
    ] {
        for body in [false, true] {
            let args = if body {
                json!({"url":"https://x.test/repos/a","options":{"secret":"key","method":"POST","headers":{"Content-Type":"application/x-www-form-urlencoded"},"body":format!("{name}=DELETE")}})
            } else {
                json!({"url":format!("https://x.test/repos/a?{name}=DELETE"),"options":{"secret":"key"}})
            };
            let (value, calls) = second_review_request_result(args);
            assert_eq!(
                value["code"], "secret_request_unsafe",
                "{name}, form={body}"
            );
            assert_eq!(calls, 0);
        }
    }
    let (value, calls) = second_review_request_result(
        json!({"url":"https://x.test/repos/a?ordinary.name=ok&balanced[x]=ok","options":{"secret":"key"}}),
    );
    assert_eq!(value["code"], "not_found");
    assert_eq!(calls, 1);
}

#[test]
fn second_review_json_method_overrides_are_refused_before_release() {
    for (mime, body) in [
        ("text/plain", r#"{"_method":"DELETE"}"#),
        ("application/json", r#"{"_METHOD":"DELETE"}"#),
        ("text/json", r#"{"_MeThOd":"DELETE"}"#),
        ("application/problem+json", r#"{"\u005fmethod":"DELETE"}"#),
        ("custom/json", r#"{"_method":"DELETE",}"#),
    ] {
        let (value, calls) = second_review_request_result(
            json!({"url":"https://x.test/repos/a","options":{"secret":"key","method":"POST","headers":{"Content-Type":mime},"body":body}}),
        );
        assert_eq!(value["code"], "secret_request_unsafe", "{mime}");
        assert_eq!(calls, 0);
    }
    let (value, calls) = second_review_request_result(
        json!({"url":"https://x.test/repos/a","options":{"secret":"key","method":"POST","body":r#"{"_method":"DELETE"}"#}}),
    );
    assert_eq!(value["code"], "secret_request_unsafe");
    assert_eq!(calls, 0);
    let (value, calls) = second_review_request_result(
        json!({"url":"https://x.test/repos/a","options":{"secret":"key","method":"POST","headers":{"Content-Type":"application/json"},"body":r#"{"nested":{"_method":"DELETE"},"text":"x&_method=GET"}"#}}),
    );
    assert_eq!(value["code"], "not_found");
    assert_eq!(calls, 1);
    let (value, calls) = second_review_request_result(
        json!({"url":"https://x.test/repos/a","options":{"secret":"key","method":"POST","headers":{"Content-Type":"application/x-www-form-urlencoded"},"body":r#"{"text":"x&_method=DELETE&other=x"}"#}}),
    );
    assert_eq!(value["code"], "secret_request_unsafe");
    assert_eq!(calls, 0);
}

#[test]
fn second_review_epoch_changes_are_scope_changed_not_readiness() {
    let good = description();
    let scope = crate::flow_scope::FlowScope {
        owner: good.scope.as_ref().unwrap().owner.clone(),
        scope_ref: "flow:scope".into(),
        epoch: 7,
    };
    let host = crate::mock::MockHost::new();
    let vault = Vault::default();
    for stamp_epoch in [false, true] {
        let mut changed = serde_json::to_value(&good).unwrap();
        if stamp_epoch {
            changed["scope"]["scope_epoch"] = json!(8);
        } else {
            changed["scope_epoch"] = json!(8);
        }
        host.set_scope_description(
            scope.owner.clone(),
            &scope.scope_ref,
            Ok(serde_json::from_value(changed).unwrap()),
        );
        let error = use_attested(
            &host,
            &vault,
            &scope,
            &request(json!({})),
            HASH,
            json!({"name":"key"}),
        )
        .unwrap_err();
        assert!(
            matches!(error, WireError::Typed(refusal) if refusal.reason == crate::flow_refusal::RefusalReason::ScopeChanged && !refusal.readiness())
        );
    }
    assert!(vault.0.lock().unwrap().is_empty());
}
