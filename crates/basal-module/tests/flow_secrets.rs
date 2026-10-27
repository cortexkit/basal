mod common;
mod wire;

use basal_core::NoHooks;
use basal_host::core_consent::{CoreConsent, FLOW_INSTALL_SECRETS_V1};
use basal_host::core_host::CoreHost;
use basal_host::routing::{ModuleOpsHost, RoutingHost};
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{Transport, WireError};
use basal_module::caller::Caller;
use basal_module::module::Hosts;
use basal_module::unconfigured::{UnconfiguredHost, UnconfiguredTransport};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use wire::Fake;

const FLOW: &str = "secret-sample";
const SCRIPT: &str = "return 1;\n";
const DIRECTORY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test-vectors/flow-install-secrets-v1"
);

fn manifest(version: u32) -> Value {
    json!({"id":FLOW,"version":version,"purpose":"Read an authenticated API without exposing its key",
        "trigger":{"schedule":{"interval":"1h"}},"net":{"fetch":[{"host":"x.test","methods":["GET"]}]},
        "secrets":[{"name":"key","target":{"scheme":"https","host":"x.test","path":"/repos/a"},"methods":["GET"]}]})
}

fn capability(fake: &Fake) {
    fake.catalog.lock().unwrap()["modules"].as_array_mut().unwrap().push(
        json!({"module_id":"prefrontal-core","capabilities":{"provides":[FLOW_INSTALL_SECRETS_V1]}}));
}

fn fixture<T: Transport + 'static>(fake: Arc<T>) -> common::Fixture {
    let catalog = Arc::new(SubcCatalog::new(fake.clone()));
    let hosts = Hosts {
        transport: Arc::new(UnconfiguredTransport),
        host: Arc::new(RoutingHost::new(
            Arc::new(ModuleOpsHost::new(fake.clone(), catalog.clone())),
            Arc::new(CoreHost::new(fake.clone())),
            Arc::new(UnconfiguredHost::new()),
        )),
        catalog,
        consent: Arc::new(CoreConsent::new(fake)),
        hooks: Arc::new(NoHooks),
    };
    common::fixture(
        "flow-secrets",
        common::Options {
            hosts: Some(hosts),
            worker_binary: Some("unused-worker".into()),
            dry_run_max_fires: Some(0),
            ..common::Options::default()
        },
    )
}

fn install(f: &common::Fixture, version: u32) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(
        &Caller::Operator,
        "flow.install",
        json!({"script":SCRIPT,"manifest":manifest(version).to_string()}),
    )
}

fn approval(fake: &Fake) {
    fake.enqueue(
        "secret.request_flow_approval",
        vec![Ok(json!({"status":"approval_required","request_id":"r",
        "statement_b64url":"e30","expires_at_ms":2000000000000_i64}))],
    );
}

fn status(fake: &Fake, hash: &str, approval: Option<Value>) {
    let mut reply = json!({"state":"active","code_hash":hash,"attempt":1});
    if let Some(approval) = approval {
        reply["approval"] = approval;
    }
    fake.enqueue("flow.install_status", vec![Ok(reply)]);
}

fn hash(f: &common::Fixture, version: u32) -> String {
    basal_proto::util::hex(
        &f.module
            .rt
            .cards(FLOW)
            .unwrap()
            .into_iter()
            .find(|c| c.version == version)
            .unwrap()
            .code_hash,
    )
}

fn decided(f: &common::Fixture, version: u32) {
    f.module
        .handle(
            &Caller::Core,
            "flow.install_decided",
            json!({"flow_id":FLOW,"version":version,"attempt":1}),
        )
        .unwrap();
}

#[test]
fn flow_secrets_real_install_path_generates_the_phone_vector() {
    let fake = Fake::new();
    let f = fixture(fake.clone());
    assert_eq!(
        install(&f, 1).unwrap_err().code,
        "install_secrets_unsupported"
    );
    assert!(fake.calls("flow.install_request").is_empty());
    assert!(fake.calls("secret.request_flow_approval").is_empty());
    f.module.rt.store().write(|tx| {
        assert_eq!(tx.execute("UPDATE install_cards SET install_id='secret_sample_v1' WHERE flow_id=?1 AND version=1", [FLOW])?, 1);
        Ok(())
    }).unwrap();
    capability(&fake);
    let statement = json!({"durations":["version"],"flow":{"flow_id":FLOW,"version":1,"code_hash":hash(&f, 1),"install_id":"secret_sample_v1"},
        "member_id":"fed-computer:illustrative","request_expires_at_ms":2000000000000_i64,"request_id":"illustrative-request-1",
        "secret":{"incarnation":"illustrative-incarnation-1","kind":"agent_api_key","name":"key"},
        "use":{"host":"x.test","location":"header:X-Api-Key","methods":["GET"],"path_prefix":"/repos/a"},"v":"ck-secret-flow-statement/v1"});
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(statement.to_string());
    fake.enqueue(
        "secret.request_flow_approval",
        vec![Ok(
            json!({"status":"approval_required","request_id":"illustrative-request-1",
        "statement_b64url":encoded,"expires_at_ms":2000000000000_i64}),
        )],
    );
    assert_eq!(install(&f, 1).unwrap()["state"], "pending");
    let calls = fake.calls("flow.install_request");
    assert_eq!(calls.len(), 1);
    let request = &calls[0]["params"];
    assert_eq!(
        request["flow_install"]["secrets"],
        json!([{"request_id":"illustrative-request-1","statement_b64url":encoded}])
    );
    assert_eq!(request["flow_install"]["install_id"], "secret_sample_v1");
    assert_eq!(
        fake.calls("secret.request_flow_approval")[0]["params"]["flow"],
        statement["flow"]
    );
    let bytes = (serde_json::to_string_pretty(request).unwrap() + "\n").into_bytes();
    let readme = std::fs::read(format!("{DIRECTORY}/README.md")).unwrap();
    let sums = format!(
        "{}  README.md\n{}  install-request.json\n",
        basal_proto::util::hex(&Sha256::digest(readme)),
        basal_proto::util::hex(&Sha256::digest(&bytes))
    );
    if std::env::var_os("BASAL_WRITE_VECTORS").is_some() {
        std::fs::write(format!("{DIRECTORY}/install-request.json"), &bytes).unwrap();
        std::fs::write(format!("{DIRECTORY}/SHA256SUMS"), &sums).unwrap();
        println!(
            "VECTOR_B64={}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        println!("VECTOR_SUMS={sums}");
    }
    assert_eq!(
        std::fs::read(format!("{DIRECTORY}/install-request.json")).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::read_to_string(format!("{DIRECTORY}/SHA256SUMS")).unwrap(),
        sums
    );
}

#[test]
fn flow_secrets_policy_and_missing_provenance_are_refused_live_and_recovered() {
    for recovery in [false, true] {
        for provenance in [None, Some(json!({"kind":"policy","rule":"self"}))] {
            let fake = Fake::new();
            capability(&fake);
            let f = fixture(fake.clone());
            approval(&fake);
            install(&f, 1).unwrap();
            status(&fake, &hash(&f, 1), provenance);
            if recovery {
                f.module.rt.recover_cardless_installs().unwrap();
            } else {
                decided(&f, 1);
            }
            let (state, reason) = f.module.rt.store().read(|c| Ok(c.query_row("SELECT state,install_check_error FROM installs WHERE flow_id=?1 AND version=1", [FLOW], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?)).unwrap();
            assert_eq!(state, "validated");
            assert!(reason.contains("explicit card approval provenance"));
            assert_eq!(
                f.module.rt.cards(FLOW).unwrap()[0].state,
                basal_core::cards::CardState::Pending
            );
            status(&fake, &hash(&f, 1), Some(json!({"kind":"card"})));
            decided(&f, 1);
            assert_eq!(
                f.module.rt.cards(FLOW).unwrap()[0].state,
                basal_core::cards::CardState::Approved
            );
        }
    }
    let fake = Fake::new();
    capability(&fake);
    let f = fixture(fake.clone());
    approval(&fake);
    let hash = basal_proto::util::hex(&basal_core::ids::code_hash(
        SCRIPT,
        &manifest(1).to_string(),
    ));
    fake.enqueue(
        "flow.install_request",
        vec![Ok(
            json!({"outcome":"approved_by_policy","rule":"self","code_hash":hash}),
        )],
    );
    status(&fake, &hash, Some(json!({"kind":"policy","rule":"self"})));
    assert_eq!(
        install(&f, 1).unwrap_err().code,
        "install_policy_unconfirmed"
    );
}

#[test]
fn flow_secrets_declined_expired_and_rejected_cards_revoke_their_version() {
    for state in ["declined", "expired", "rejected"] {
        let fake = Fake::new();
        capability(&fake);
        let f = fixture(fake.clone());
        approval(&fake);
        install(&f, 1).unwrap();
        let mut reply = json!({"state":state,"attempt":1});
        if state != "rejected" {
            reply["code_hash"] = json!(hash(&f, 1));
        }
        fake.enqueue("flow.install_status", vec![Ok(reply)]);
        decided(&f, 1);
        assert_ne!(
            f.module.rt.cards(FLOW).unwrap()[0].state,
            basal_core::cards::CardState::Pending
        );
        fake.enqueue(
            "secret.revoke_flow_grants",
            vec![Ok(json!({"status":"revoked","grants":0}))],
        );
        f.module.rt.drain_secret_revocations().unwrap();
        let calls = fake.calls("secret.revoke_flow_grants");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["params"]["flow_version"], 1);
        assert_eq!(
            calls[0]["params"]["flow_install_id"],
            f.module.rt.cards(FLOW).unwrap()[0]
                .install_id
                .clone()
                .unwrap()
        );
    }
}

#[test]
fn flow_secrets_uninstall_revoke_is_durable_and_retried_without_version() {
    let fake = Fake::new();
    capability(&fake);
    let f = fixture(fake.clone());
    approval(&fake);
    install(&f, 1).unwrap();
    f.module
        .handle(&Caller::Operator, "flow.uninstall", json!({"flow_id":FLOW}))
        .unwrap();
    fake.enqueue(
        "secret.revoke_flow_grants",
        vec![Err(WireError::NeverSent("vault restarting".into()))],
    );
    f.module.rt.drain_secret_revocations().unwrap();
    let pending: i64 = f.module.rt.store().read(|c| Ok(c.query_row("SELECT count(*) FROM outbox WHERE kind='secret.revoke_flow_grants' AND delivered_at IS NULL AND attempts=1", [], |r| r.get(0))?)).unwrap();
    assert_eq!(pending, 1);
    f.clock.advance(1000);
    fake.enqueue(
        "secret.revoke_flow_grants",
        vec![Ok(json!({"status":"revoked","grants":0}))],
    );
    f.module.rt.drain_secret_revocations().unwrap();
    let calls = fake.calls("secret.revoke_flow_grants");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["params"], calls[1]["params"]);
    assert!(calls[1]["params"].get("flow_version").is_none());
    let pending: i64 = f.module.rt.store().read(|c| Ok(c.query_row("SELECT count(*) FROM outbox WHERE kind='secret.revoke_flow_grants' AND delivered_at IS NULL", [], |r| r.get(0))?)).unwrap();
    assert_eq!(pending, 0);
}

#[test]
fn flow_secrets_replacement_waits_for_the_old_versions_last_run() {
    let fake = Fake::new();
    capability(&fake);
    let f = fixture(fake.clone());
    approval(&fake);
    install(&f, 1).unwrap();
    status(&fake, &hash(&f, 1), Some(json!({"kind":"card"})));
    decided(&f, 1);
    let old_id = f.module.rt.cards(FLOW).unwrap()[0]
        .install_id
        .clone()
        .unwrap();
    let run = common::admit(&f, FLOW, "old-run");
    approval(&fake);
    install(&f, 2).unwrap();
    status(&fake, &hash(&f, 2), Some(json!({"kind":"card"})));
    decided(&f, 2);
    f.module.rt.drain_secret_revocations().unwrap();
    assert!(fake.calls("secret.revoke_flow_grants").is_empty());
    f.module.rt.cancel(&run, "test").unwrap();
    f.clock.advance(1000);
    fake.enqueue(
        "secret.revoke_flow_grants",
        vec![Ok(json!({"status":"revoked","grants":1}))],
    );
    f.module.rt.drain_secret_revocations().unwrap();
    let calls = fake.calls("secret.revoke_flow_grants");
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["params"],
        json!({"flow_install_id":old_id,"flow_version":1})
    );
}

#[test]
fn flow_secrets_reapproval_after_pending_uninstall_requires_another_revoke() {
    let fake = Fake::new();
    capability(&fake);
    let f = fixture(fake.clone());
    approval(&fake);
    install(&f, 1).unwrap();
    for approved in [false, true] {
        if approved {
            status(&fake, &hash(&f, 1), Some(json!({"kind":"card"})));
            decided(&f, 1);
        }
        f.module
            .handle(&Caller::Operator, "flow.uninstall", json!({"flow_id":FLOW}))
            .unwrap();
        fake.enqueue(
            "secret.revoke_flow_grants",
            vec![Ok(json!({"status":"revoked","grants":0}))],
        );
        f.module.rt.drain_secret_revocations().unwrap();
    }
    let calls = fake.calls("secret.revoke_flow_grants");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["params"], calls[1]["params"]);
}

#[test]
fn flow_secrets_packages_and_templates_refuse_registration_and_instance_creation() {
    for template in [false, true] {
        let fake = Fake::new();
        let f = fixture(fake);
        let mut manifest = manifest(1);
        let id = if template {
            "secret-template"
        } else {
            "secret-package"
        };
        manifest["id"] = json!(id);
        if template {
            manifest["params"] = json!({"note":{"type":"string","max_bytes":32}});
            manifest["template"] = json!({"max_per_agent":1,"max_live":2,"max_lifetime":"1d"});
        }
        let text = manifest.to_string();
        let error = f
            .module
            .handle(
                &Caller::Operator,
                "package.register",
                json!({"script":SCRIPT,"manifest":text}),
            )
            .unwrap_err();
        assert_eq!(error.code, "secrets_require_flow_install");
        assert!(error.message.contains("ordinary flow installs"));
        let count: i64 = f
            .module
            .rt
            .store()
            .read(|c| Ok(c.query_row("SELECT count(*) FROM package_versions", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 0);
        // Instance creation must refuse secrets independently of registration
        // checks, so seed an otherwise readable row directly.
        f.module.rt.store().write(|tx| {
            tx.execute("INSERT INTO package_versions(package,version,script,manifest,code_hash,registered_at) VALUES (?1,1,?2,?3,?4,0)",
                rusqlite::params![id, SCRIPT, text, basal_core::ids::code_hash(SCRIPT, &text).as_slice()])?;
            Ok(())
        }).unwrap();
        let error = if template {
            f.module.handle(&Caller::Operator, "flow.instantiate", json!({"template":id,"version":1,"agent_id":"ag_synapse","params":{"note":"test"},"expires_in":"1h"})).unwrap_err()
        } else {
            f.module
                .handle(
                    &Caller::Core,
                    "flow.instance.ensure",
                    json!({"package":id,"version":1,"agent_id":"ag_synapse","generation":1}),
                )
                .unwrap_err()
        };
        assert_eq!(error.code, "secrets_require_flow_install");
        assert!(error.message.contains("ordinary flow installs"));
        let count: i64 = f
            .module
            .rt
            .store()
            .read(|c| Ok(c.query_row("SELECT count(*) FROM flows", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[test]
fn review_superseded_pending_secret_cards_queue_version_cleanup() {
    let fake = Fake::new();
    capability(&fake);
    let f = fixture(fake.clone());
    approval(&fake);
    install(&f, 1).unwrap();
    let old_id = f.module.rt.cards(FLOW).unwrap()[0]
        .install_id
        .clone()
        .unwrap();
    approval(&fake);
    install(&f, 2).unwrap();
    status(&fake, &hash(&f, 2), Some(json!({"kind":"card"})));
    decided(&f, 2);
    assert_eq!(
        f.module.rt.cards(FLOW).unwrap()[0].state,
        basal_core::cards::CardState::Stale
    );
    fake.enqueue(
        "secret.revoke_flow_grants",
        vec![Ok(json!({"status":"revoked","grants":0}))],
    );
    f.module.rt.drain_secret_revocations().unwrap();
    assert_eq!(
        fake.calls("secret.revoke_flow_grants")[0]["params"],
        json!({"flow_install_id":old_id,"flow_version":1})
    );
}

#[test]
fn review_core_refusal_after_secret_preparation_cleans_up_durably() {
    let fake = Fake::new();
    capability(&fake);
    let f = fixture(fake.clone());
    approval(&fake);
    fake.enqueue(
        "flow.install_request",
        vec![Err(WireError::Refused {
            code: "secret_statement_mismatch".into(),
            message: "refused".into(),
        })],
    );
    fake.enqueue(
        "secret.revoke_flow_grants",
        vec![Err(WireError::Refused {
            code: "store_error".into(),
            message: "vault restarting".into(),
        })],
    );
    assert_eq!(
        install(&f, 1).unwrap_err().code,
        "secret_statement_mismatch"
    );
    assert_eq!(fake.calls("secret.request_flow_approval").len(), 1);
    let request = fake.calls("secret.revoke_flow_grants")[0]["params"].clone();
    assert_eq!(request["flow_version"], 1);
    assert_eq!(
        request["flow_install_id"],
        fake.calls("secret.request_flow_approval")[0]["params"]["flow"]["install_id"]
    );
    assert!(f.module.rt.cards(FLOW).unwrap()[0].install_id.is_none());
    let pending: i64 = f.module.rt.store().read(|c| Ok(c.query_row("SELECT count(*) FROM outbox WHERE kind='secret.revoke_flow_grants' AND delivered_at IS NULL AND failed_at IS NULL", [], |r| r.get(0))?)).unwrap();
    assert_eq!(pending, 1);
    f.clock.advance(1000);
    fake.enqueue(
        "secret.revoke_flow_grants",
        vec![Ok(json!({"status":"revoked","grants":0}))],
    );
    f.module.rt.drain_secret_revocations().unwrap();
    assert_eq!(
        fake.calls("secret.revoke_flow_grants")[1]["params"],
        request
    );
}

struct Warnings(Arc<std::sync::Mutex<Vec<String>>>);
impl tracing::field::Visit for Warnings {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.lock().unwrap().push(format!("{value:?}"));
        }
    }
}
impl tracing::Subscriber for Warnings {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        event.record(&mut Warnings(self.0.clone()));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn review_permanent_revoke_refusals_are_recorded_once_and_never_retried() {
    for code in ["invalid_params", "future_refusal", "store_error"] {
        let fake = Fake::new();
        capability(&fake);
        let f = fixture(fake.clone());
        approval(&fake);
        install(&f, 1).unwrap();
        f.module
            .handle(&Caller::Operator, "flow.uninstall", json!({"flow_id":FLOW}))
            .unwrap();
        fake.enqueue(
            "secret.revoke_flow_grants",
            vec![Err(WireError::RefusedDetails {
                code: code.into(),
                message: "must-not-log".into(),
                detail: json!({"unsafe":"must-not-log"}),
            })],
        );
        let warnings = Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::with_default(Warnings(warnings.clone()), || {
            f.module.rt.drain_secret_revocations().unwrap();
            f.clock.advance(10000);
            if code == "store_error" {
                fake.enqueue(
                    "secret.revoke_flow_grants",
                    vec![Ok(json!({"status":"revoked","grants":0}))],
                );
            }
            f.module.rt.drain_secret_revocations().unwrap();
        });
        let (failed, failure, delivered): (Option<i64>,Option<String>,Option<i64>) = f.module.rt.store().read(|c| Ok(c.query_row("SELECT failed_at,failure_code,delivered_at FROM outbox WHERE kind='secret.revoke_flow_grants'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?)).unwrap();
        if code == "store_error" {
            assert!(failed.is_none() && failure.is_none() && delivered.is_some());
            assert_eq!(fake.calls("secret.revoke_flow_grants").len(), 2);
            assert!(warnings.lock().unwrap().is_empty());
        } else {
            assert!(failed.is_some() && delivered.is_none());
            assert_eq!(failure.as_deref(), Some(code));
            assert_eq!(fake.calls("secret.revoke_flow_grants").len(), 1);
            assert_eq!(warnings.lock().unwrap().len(), 1);
            assert!(!warnings.lock().unwrap()[0].contains("must-not-log"));
        }
    }
}

#[test]
fn review_unreadable_historical_manifests_do_not_block_cleanup_or_card_settlement() {
    for decline in [false, true] {
        let fake = Fake::new();
        capability(&fake);
        let f = fixture(fake.clone());
        approval(&fake);
        install(&f, 1).unwrap();
        let broken = format!("{{\"secrets\":[],\"old_schema\":\"review-{decline}\"}}");
        f.module
            .rt
            .store()
            .write(|tx| {
                tx.execute(
                    "UPDATE installs SET manifest=?1 WHERE flow_id=?2",
                    rusqlite::params![broken, FLOW],
                )?;
                Ok(())
            })
            .unwrap();
        let warnings = Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::with_default(Warnings(warnings.clone()), || {
            if decline {
                fake.enqueue(
                    "flow.install_status",
                    vec![Ok(
                        json!({"state":"declined","code_hash":hash(&f, 1),"attempt":1}),
                    )],
                );
                decided(&f, 1);
                assert_eq!(
                    f.module.rt.cards(FLOW).unwrap()[0].state,
                    basal_core::cards::CardState::Declined
                );
            } else {
                f.module
                    .handle(&Caller::Operator, "flow.uninstall", json!({"flow_id":FLOW}))
                    .unwrap();
            }
            f.module.rt.enqueue_secret_install_cleanup(FLOW, 1).unwrap();
        });
        assert_eq!(warnings.lock().unwrap().len(), 1);
    }
    let fake = Fake::new();
    capability(&fake);
    let f = fixture(fake.clone());
    approval(&fake);
    install(&f, 1).unwrap();
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute(
                "UPDATE installs SET manifest='{\"old_schema\":true}' WHERE flow_id=?1",
                [FLOW],
            )?;
            Ok(())
        })
        .unwrap();
    approval(&fake);
    install(&f, 2).unwrap();
    let id = f
        .module
        .rt
        .cards(FLOW)
        .unwrap()
        .into_iter()
        .find(|c| c.version == 2)
        .unwrap()
        .install_id
        .unwrap();
    f.module
        .handle(&Caller::Operator, "flow.uninstall", json!({"flow_id":FLOW}))
        .unwrap();
    fake.enqueue(
        "secret.revoke_flow_grants",
        vec![Ok(json!({"status":"revoked","grants":0}))],
    );
    f.module.rt.drain_secret_revocations().unwrap();
    assert_eq!(
        fake.calls("secret.revoke_flow_grants")[0]["params"],
        json!({"flow_install_id":id})
    );
}

#[test]
fn review_nonsecret_legacy_approval_keeps_stored_manifest_compatibility() {
    let fake = Fake::new();
    let f = fixture(fake.clone());
    let mut legacy = manifest(1);
    legacy.as_object_mut().unwrap().remove("secrets");
    f.module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({"script":SCRIPT,"manifest":legacy.to_string()}),
        )
        .unwrap();
    legacy["ops"] = json!([{"module":"prefrontal-core","op":"board.read"}]);
    let text = legacy.to_string();
    assert!(basal_core::Manifest::parse(&text).is_err());
    assert!(basal_core::Manifest::parse_stored(&text).is_ok());
    let code_hash = basal_core::ids::code_hash(SCRIPT, &text);
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute(
                "UPDATE installs SET manifest=?1,code_hash=?2 WHERE flow_id=?3",
                rusqlite::params![text, code_hash.as_slice(), FLOW],
            )?;
            tx.execute(
                "UPDATE install_cards SET code_hash=?1 WHERE flow_id=?2",
                rusqlite::params![code_hash.as_slice(), FLOW],
            )?;
            Ok(())
        })
        .unwrap();
    status(&fake, &basal_proto::util::hex(&code_hash), None);
    f.module.rt.recover_cardless_installs().unwrap();
    let approval: String = f
        .module
        .rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT approval_kind FROM installs WHERE flow_id=?1 AND state='approved'",
                [FLOW],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(approval, "unknown");
}

struct GrantVault {
    inner: Arc<Fake>,
    pending: std::sync::Mutex<std::collections::HashSet<String>>,
    grants: std::sync::Mutex<std::collections::HashSet<String>>,
    revokes: std::sync::Mutex<Vec<String>>,
    attempts: std::sync::atomic::AtomicUsize,
}
impl Transport for GrantVault {
    fn catalog(&self) -> Result<Value, WireError> {
        self.inner.catalog()
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        if op == "secret.revoke_flow_grants" {
            let id = params["flow_install_id"].as_str().unwrap().to_owned();
            self.revokes.lock().unwrap().push(id.clone());
            if self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                return Err(WireError::Refused {
                    code: "store_error".into(),
                    message: "retry later".into(),
                });
            }
            self.pending.lock().unwrap().remove(&id);
            let removed = self.grants.lock().unwrap().remove(&id);
            return Ok(json!({"status":"revoked","grants":u32::from(removed)}));
        }
        let prepared_id = (op == "secret.request_flow_approval")
            .then(|| params["flow"]["install_id"].as_str().unwrap().to_owned());
        let result = self.inner.management(module, op, params);
        if result.is_ok()
            && let Some(id) = prepared_id
        {
            self.pending.lock().unwrap().insert(id);
        }
        result
    }
    fn tool(&self, module: &str, op: &str, params: Value, key: &str) -> Result<Value, WireError> {
        self.inner.tool(module, op, params, key)
    }
}

#[test]
fn review_delayed_refusal_cleanup_cannot_revoke_a_fresh_retry_install() {
    let fake = Fake::new();
    capability(&fake);
    let vault = Arc::new(GrantVault {
        inner: fake.clone(),
        pending: Default::default(),
        grants: Default::default(),
        revokes: Default::default(),
        attempts: Default::default(),
    });
    let f = fixture(vault.clone());
    approval(&fake);
    fake.enqueue(
        "flow.install_request",
        vec![Err(WireError::Refused {
            code: "secret_statement_mismatch".into(),
            message: "refused".into(),
        })],
    );
    install(&f, 1).unwrap_err();
    let old = fake.calls("secret.request_flow_approval")[0]["params"]["flow"]["install_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(vault.pending.lock().unwrap().contains(&old));
    approval(&fake);
    install(&f, 1).unwrap();
    let new = fake.calls("secret.request_flow_approval")[1]["params"]["flow"]["install_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(old, new);
    vault.pending.lock().unwrap().remove(&new);
    vault.grants.lock().unwrap().insert(new.clone());
    status(&fake, &hash(&f, 1), Some(json!({"kind":"card"})));
    decided(&f, 1);
    f.clock.advance(1000);
    f.module.rt.drain_secret_revocations().unwrap();
    assert!(!vault.pending.lock().unwrap().contains(&old));
    assert!(vault.grants.lock().unwrap().contains(&new));
    assert_eq!(*vault.revokes.lock().unwrap(), vec![old.clone(), old]);
}
