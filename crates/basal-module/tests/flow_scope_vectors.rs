use basal_host::core_host::{CoreHost, decode_install_status};
use basal_host::transport::{Transport, WireError, management_body};
use basal_host::{Host, InstallStatus};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/flow-scope-v1");
fn vector(name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(format!("{VECTORS}/{name}")).unwrap()).unwrap()
}

#[test]
fn core_published_flow_scope_vectors_are_pinned_and_decode_exactly() {
    for (name, digest) in [
        (
            "install-status-before-registration.json",
            "9772e680739317ad1a5f471c73fbc19d651e83a7f8ee6ba7e3256f8c27ee18f9",
        ),
        (
            "install-status-registered.json",
            "b26d68f1f8d47ccdfd5ae7aa2f1b111ede8db2cddc3db4842d74958345b5c6db",
        ),
        (
            "install-status-request.json",
            "2858fc17478de5f9d8a771871385611502af52f0299ca154f6824f9027c8b281",
        ),
        (
            "install-status-revoked.json",
            "0aac6a3ee613c7131c55b104bd475cdee18811286242842759e5ada40dfb88e2",
        ),
        (
            "registered-scope.json",
            "7ec1b7b92ff51d95713072155fbb5707f3676a1380da9c379e209ae52859c6f8",
        ),
        (
            "registered-global-scope.json",
            "9ea04c51cf987028ba00040c6abfada7e4780d47d48bcb0008d6f789994a518c",
        ),
    ] {
        let bytes = std::fs::read(format!("{VECTORS}/{name}")).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), digest, "{name}");
    }
    let zero = "0".repeat(64);
    assert_eq!(
        decode_install_status(&vector("install-status-before-registration.json")).unwrap(),
        InstallStatus::Active {
            code_hash: zero.clone(),
            scope: None
        }
    );
    assert_eq!(
        decode_install_status(&vector("install-status-revoked.json")).unwrap(),
        InstallStatus::Revoked {
            code_hash: zero.clone()
        }
    );
    let InstallStatus::Active {
        code_hash,
        scope: Some(scope),
    } = decode_install_status(&vector("install-status-registered.json")).unwrap()
    else {
        panic!("registered scope missing")
    };
    assert_eq!(code_hash, zero);
    assert_eq!(scope.targets, ["broca".into()].into_iter().collect());
    let registered = vector("registered-scope.json");
    let record: subc_protocol::scope::ScopeRecord =
        serde_json::from_value(registered["record"].clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&scope.selector.owner).unwrap(),
        registered["owner"]
    );
    assert_eq!(
        scope.selector.selector().scope_epoch,
        Some(record.scope_epoch)
    );
    assert_eq!(scope.selector.scope_ref, record.scope_ref);
    assert_eq!(record.attributes.flow_id.as_deref(), Some("flow-x"));
    assert_eq!(record.attributes.agent_id.as_deref(), Some("ag_fixture"));
    assert!(record.attributes.delegates);
    assert_eq!(record.carriers.len(), 1);
    assert_eq!(
        record.carriers[0].principal,
        subc_protocol::Principal::Reserved {
            module_id: "basal".into()
        }
    );
    assert_eq!(
        record.carriers[0].targets.as_deref(),
        Some(["broca".into(), "plexus".into(), "prefrontal-core".into()].as_slice())
    );
    let global = vector("registered-global-scope.json");
    let global_record: subc_protocol::scope::ScopeRecord =
        serde_json::from_value(global["record"].clone()).unwrap();
    assert_eq!(global_record.attributes.flow_id.as_deref(), Some("global"));
    assert_eq!(global_record.attributes.agent_id, None);
    assert!(!global_record.attributes.delegates);
    assert_eq!(global_record.carriers, record.carriers);
    let global_status = json!({"state":"active","code_hash":"0".repeat(64),"scope":{"owner":global["owner"],"ref":global_record.scope_ref,"epoch":global_record.scope_epoch},"flow_scope_targets":["broca"]});
    let InstallStatus::Active {
        scope: Some(global_scope),
        ..
    } = decode_install_status(&global_status).unwrap()
    else {
        panic!("global selector missing")
    };
    assert_eq!(global_scope.selector, scope.selector);
}

#[derive(Default)]
struct Capture(Mutex<Vec<(String, Value)>>);
impl Transport for Capture {
    fn catalog(&self) -> Result<Value, WireError> {
        unreachable!()
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        self.0
            .lock()
            .unwrap()
            .push((module.into(), management_body(op, params)));
        Ok(vector("install-status-before-registration.json"))
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        unreachable!()
    }
}

#[test]
fn install_status_encoder_reproduces_core_request_bytes() {
    let transport = Arc::new(Capture::default());
    CoreHost::new(transport.clone())
        .install_status("flow-x", 1)
        .unwrap();
    let records = transport.0.lock().unwrap();
    assert_eq!(records[0].0, "prefrontal-core");
    assert_eq!(
        records[0].1,
        json!({"method":"flow.install_status","params":{"flow_id":"flow-x","version":1}})
    );
    // The LF terminates the vector file, not the framed request payload.
    let text = serde_json::to_string(&records[0].1).unwrap() + "\n";
    assert_eq!(
        text.as_bytes(),
        std::fs::read(format!("{VECTORS}/install-status-request.json")).unwrap()
    );
}
