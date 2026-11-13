mod common;

use basal_core::{InstallError, ManifestError, test_support::seed_stored_version};
use basal_module::{
    caller::Caller,
    dryrun::{DryRunError, DryRunRequest, DryTrigger, Mode},
};
use common::{Options, fixture};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn manifest() -> Value {
    json!({
        "id":"family-planning", "version":1,
        "purpose":"Plan family commitments",
        "explanation":"Read the selected calendar and reminder list and send a planning summary to the bound destinations.",
        "audience":{"kind":"agent","id":"ag_synapse"},
        "trigger":{"schedule":{"interval":"1h"}},
        "abilities":[
            {"key":"family_calendar","token":"cap/calendar.read@1","scope":["calendar"]},
            {"key":"family_reminders","token":"cap/reminders.read@1","scope":["list"]}
        ],
        "destinations":[
            {"key":"planning","kind":"room"},
            {"key":"me","kind":"agent_inbox"},
            {"key":"alerts","kind":"notice","severity_max":"attention"}
        ]
    })
}

fn params(value: &Value) -> Value {
    json!({"script":"return 1;", "manifest":value.to_string()})
}

fn request(value: &Value) -> DryRunRequest {
    DryRunRequest {
        script: "return 1;".into(),
        manifest: value.to_string(),
        author: "operator".into(),
        loop_override: false,
        mode: Mode::Capture,
        trigger: DryTrigger::Schedule { window: None },
        now_ms: common::T0,
    }
}

fn cases() -> Vec<(&'static str, Value, &'static str, &'static str)> {
    let mut cases = vec![(
        "valid-pack.json",
        manifest(),
        "pack_runtime_unavailable",
        "",
    )];
    for (name, pointer, replacement, field) in [
        (
            "bad-key.json",
            "/abilities/0/key",
            json!("Family"),
            "abilities[0].key",
        ),
        (
            "bad-token.json",
            "/abilities/0/token",
            json!("cap/calendar.read@0"),
            "abilities[0].token",
        ),
        (
            "duplicate-key.json",
            "/abilities/1/key",
            json!("family_calendar"),
            "abilities[1].key",
        ),
        (
            "empty-scope.json",
            "/abilities/0/scope",
            json!([]),
            "abilities[0].scope",
        ),
        (
            "unknown-kind.json",
            "/destinations/0/kind",
            json!("email"),
            "destinations[0].kind",
        ),
        (
            "notice-without-severity-max.json",
            "/destinations/0/kind",
            json!("notice"),
            "destinations[0].severity_max",
        ),
        (
            "urgent-severity.json",
            "/destinations/2/severity_max",
            json!("urgent"),
            "destinations[2].severity_max",
        ),
    ] {
        let mut value = manifest();
        *value.pointer_mut(pointer).unwrap() = replacement;
        cases.push((name, value, "install_refused", field));
    }
    let mut value = manifest();
    value["net"] = json!({"fetch":[{"host":"calendar.example"}]});
    cases.push((
        "net-fetch.json",
        value,
        "pack_net_fetch_forbidden",
        "net.fetch",
    ));
    let mut value = manifest();
    value["ops"] = json!([{"module":"calendar","op":"events.list"}]);
    cases.push((
        "literal-grant.json",
        value,
        "pack_literal_grant",
        "sinks/ops",
    ));
    let mut value = manifest();
    value["destinations"][0]["severity_max"] = json!("attention");
    cases.push((
        "severity-max-on-room.json",
        value,
        "install_refused",
        "destinations[0].severity_max",
    ));
    let mut value = manifest();
    value["abilities"][0]["provider"] = json!("calendar");
    cases.push((
        "unknown-ability-member.json",
        value,
        "install_refused",
        "abilities[0].provider",
    ));
    let mut value = manifest();
    value["abilities"] = json!(
        (0..17)
            .map(|i| json!({
                "key":format!("calendar_{i}"),"token":"cap/calendar.read@1","scope":["calendar"]
            }))
            .collect::<Vec<_>>()
    );
    cases.push(("too-many.json", value, "install_refused", "abilities"));
    cases
}

#[test]
fn pack_manifest_vectors_are_generated_from_real_intake() {
    let f = fixture("pack-vectors", Options::default());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/pack-manifest-v1");
    let write = std::env::var_os("BASAL_WRITE_VECTORS").is_some();
    let mut files = vec![("README.md", std::fs::read(root.join("README.md")).unwrap())];
    for (name, value, code, field) in cases() {
        let error = f
            .module
            .handle(&Caller::Operator, "flow.install", params(&value))
            .unwrap_err();
        assert_eq!(error.code, code, "{name}: {error:?}");
        assert!(error.message.contains(field), "{name}: {error:?}");
        let mut body = json!({"code":error.code,"message":error.message});
        if let Some(detail) = error.detail {
            body["detail"] = detail;
        }
        let vector = json!({"manifest":value,"reply":{"error":body}});
        let bytes = (serde_json::to_string_pretty(&vector).unwrap() + "\n").into_bytes();
        files.push((name, bytes));

        let dry = f.module.dry.run(&request(&value)).unwrap_err();
        if code == "install_refused" {
            assert!(
                matches!(&dry, DryRunError::Invalid(message) if message.contains(field)),
                "{name}: {dry:?}"
            );
        } else {
            assert!(
                matches!(&dry, DryRunError::InstallRefused(InstallError::Manifest(ManifestError::Pack { code: actual, .. })) if *actual == code),
                "{name}: {dry:?}"
            );
        }
    }
    let sums: String = files
        .iter()
        .map(|(name, bytes)| format!("{:x}  {name}\n", Sha256::digest(bytes)))
        .collect();
    files.push(("SHA256SUMS", sums.into_bytes()));
    for (name, bytes) in files {
        if write {
            std::fs::write(root.join(name), &bytes).unwrap();
        }
        assert_eq!(
            std::fs::read(root.join(name)).unwrap_or_default(),
            bytes,
            "{name} drifted; regenerate with BASAL_WRITE_VECTORS=1 RUST_TEST_THREADS=1 cargo test --locked -p basal-module --test pack_manifest pack_manifest_vectors"
        );
    }
    let count: i64 = f
        .module
        .rt
        .store()
        .read(|conn| Ok(conn.query_row("SELECT count(*) FROM flows", [], |row| row.get(0))?))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn pack_manifest_registration_refuses_packages_and_templates() {
    let f = fixture("pack-registration", Options::default());
    for template in [false, true] {
        for abilities in [manifest()["abilities"].clone(), json!([])] {
            let mut value = manifest();
            value["abilities"] = abilities;
            if template {
                value["params"] = json!({"note":{"type":"string","max_bytes":32}});
                value["template"] = json!({"max_per_agent":1,"max_live":2,"max_lifetime":"1d"});
            }
            let error = f
                .module
                .handle(&Caller::Operator, "package.register", params(&value))
                .unwrap_err();
            assert_eq!(error.code, "pack_template_unsupported");
        }
    }
    let count: i64 = f
        .module
        .rt
        .store()
        .read(|conn| {
            Ok(
                conn.query_row("SELECT count(*) FROM package_versions", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn pack_manifest_stored_dry_run_refuses_without_reapplying_other_intake_policy() {
    let f = fixture("pack-stored-dry", Options::default());
    let mut value = manifest();
    value.as_object_mut().unwrap().remove("explanation");
    value.as_object_mut().unwrap().remove("audience");
    seed_stored_version(&f.module.rt, "return 1;", &value.to_string(), false).unwrap();
    let error = f
        .module
        .handle(
            &Caller::Operator,
            "flow.dry_run",
            json!({"flow_id":"family-planning"}),
        )
        .unwrap_err();
    assert_eq!(error.code, "pack_runtime_unavailable");
    value["abilities"][0]["scope"] = json!([]);
    value["id"] = json!("invalid-stored-pack");
    seed_stored_version(&f.module.rt, "return 1;", &value.to_string(), false).unwrap();
    let error = f
        .module
        .handle(
            &Caller::Operator,
            "flow.dry_run",
            json!({"flow_id":"invalid-stored-pack"}),
        )
        .unwrap_err();
    assert_eq!(error.code, "install_refused");
    assert!(error.message.contains("abilities[0].scope"));
}
