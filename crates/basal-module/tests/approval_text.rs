mod common;

use basal_core::{InstallRequest, test_support::seed_stored_version};
use basal_module::{
    caller::Caller,
    dryrun::{DryRunError, DryRunRequest, DryTrigger, Mode},
};
use common::{Options, fixture};
use serde_json::{Value, json};

fn manifest(id: &str, version: u32) -> Value {
    json!({"id":id,"version":version,"purpose":"Check manifest intake",
        "explanation":"Watches the hourly clock without waking anyone or reading or writing data; used to test approval text.",
        "trigger":{"schedule":{"interval":"1h"}}})
}

fn install_params(manifest: &Value) -> Value {
    json!({"script":"return 1;","manifest":manifest.to_string()})
}

#[test]
fn approval_text_refusal_codes_reach_install_and_registration() {
    let f = fixture("approval-text-codes", Options::default());
    for op in ["flow.install", "package.register"] {
        let mut missing = manifest("missing", 1);
        missing.as_object_mut().unwrap().remove("explanation");
        let error = f
            .module
            .handle(&Caller::Operator, op, install_params(&missing))
            .unwrap_err();
        assert_eq!(error.code, "explanation_required");
        for words in [
            "what this flow watches",
            "who it wakes and when",
            "what it reads or writes",
            "why it exists",
            "approval card",
        ] {
            assert!(error.message.contains(words), "{}", error.message);
        }
        let mut first = manifest("first", 1);
        first["changes"] = json!("Polls hourly instead of daily to reduce delay.");
        let error = f
            .module
            .handle(&Caller::Operator, op, install_params(&first))
            .unwrap_err();
        assert_eq!(error.code, "changes_on_first_version");
        assert!(error.message.contains("remove `changes`"));
    }
}

#[test]
fn approval_text_flow_upgrade_uses_every_stored_state() {
    let f = fixture("approval-text-states", Options::default());
    for state in ["validated", "approved", "superseded", "revoked"] {
        let old = manifest(state, 1);
        f.module
            .rt
            .install(&InstallRequest {
                script: "return 1;".into(),
                manifest: old.to_string(),
                author: "operator".into(),
                loop_override: false,
            })
            .unwrap();
        f.module
            .rt
            .store()
            .write(|tx| {
                tx.execute(
                    "UPDATE installs SET state=?2,approval_ref=?3,revoked_at=?4 WHERE flow_id=?1",
                    rusqlite::params![
                        state,
                        if state == "revoked" {
                            "superseded"
                        } else {
                            state
                        },
                        (state != "validated").then_some("historical-approval"),
                        (state == "revoked").then_some(common::T0)
                    ],
                )?;
                Ok(())
            })
            .unwrap();
        let mut next = manifest(state, 2);
        let error = f
            .module
            .handle(&Caller::Operator, "flow.install", install_params(&next))
            .unwrap_err();
        assert_eq!(error.code, "changes_required", "{state}");
        assert!(
            error
                .message
                .contains("what this version changes from the previous one, and why")
        );
        next["changes"] = json!("Returns a newer result to test an upgrade.");
        let request = InstallRequest {
            script: "return 2;".into(),
            manifest: next.to_string(),
            author: "operator".into(),
            loop_override: false,
        };
        assert!(f.module.rt.install(&request).unwrap().new);
        assert!(!f.module.rt.install(&request).unwrap().new);
    }
}

#[test]
fn approval_text_template_and_package_upgrades_require_changes() {
    let f = fixture("approval-text-packages", Options::default());
    for template in [false, true] {
        let mut value = manifest(if template { "template" } else { "package" }, 1);
        if template {
            value["params"] = json!({});
            value["template"] = json!({"max_per_agent":2,"max_lifetime":"1d"});
        }
        let first = f
            .module
            .handle(
                &Caller::Operator,
                "package.register",
                install_params(&value),
            )
            .unwrap();
        assert_eq!(first["new"], true);
        value["version"] = json!(2);
        let error = f
            .module
            .handle(
                &Caller::Operator,
                "package.register",
                install_params(&value),
            )
            .unwrap_err();
        assert_eq!(error.code, "changes_required");
        value["changes"] = json!("Records the new version for an upgrade check.");
        assert_eq!(
            f.module
                .handle(
                    &Caller::Operator,
                    "package.register",
                    install_params(&value)
                )
                .unwrap()["new"],
            true
        );
    }
}

#[test]
fn approval_text_dry_run_checks_real_history_before_scratch_replay() {
    let f = fixture(
        "approval-text-dry-history",
        Options {
            dry_run_max_fires: Some(0),
            ..Options::default()
        },
    );
    let old = manifest("dry-history", 1);
    seed_stored_version(&f.module.rt, "return 1;", &old.to_string(), false).unwrap();
    let production_writes = f.module.rt.store().write_transactions_for_test();
    let make_request = |value: &Value| DryRunRequest {
        script: "return 2;".into(),
        manifest: value.to_string(),
        author: "operator".into(),
        loop_override: false,
        mode: Mode::Capture,
        trigger: DryTrigger::Schedule { window: None },
        now_ms: common::T0,
    };
    let next = manifest("dry-history", 2);
    assert!(matches!(
        f.module.dry.run(&make_request(&next)),
        Err(DryRunError::InstallRefused(
            basal_core::InstallError::Manifest(basal_core::ManifestError::ChangesRequired)
        ))
    ));
    let mut explained = next.clone();
    explained["changes"] = json!("Returns the new result in capture replay.");
    assert_eq!(
        f.module.dry.run(&make_request(&explained)).unwrap()["runs"],
        json!([])
    );
    let mut first = manifest("new-dry-history", 42);
    first["changes"] = json!("There is no previous version.");
    assert!(matches!(
        f.module.dry.run(&make_request(&first)),
        Err(DryRunError::InstallRefused(
            basal_core::InstallError::Manifest(basal_core::ManifestError::ChangesOnFirstVersion)
        ))
    ));
    first.as_object_mut().unwrap().remove("explanation");
    assert!(matches!(
        f.module.dry.run(&make_request(&first)),
        Err(DryRunError::InstallRefused(
            basal_core::InstallError::Manifest(basal_core::ManifestError::ExplanationRequired)
        ))
    ));
    assert_eq!(
        f.module.rt.store().write_transactions_for_test(),
        production_writes
    );
}

#[test]
fn approval_text_all_deployed_manifests_remain_readable_without_reapproval() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut count = 0;
    for directory in ["flows", "templates"] {
        for entry in std::fs::read_dir(root.join(directory)).unwrap() {
            let path = entry.unwrap().path().join("manifest.json");
            if path.is_file() {
                let text = std::fs::read_to_string(&path).unwrap();
                basal_core::Manifest::parse_stored(&text)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                count += 1;
            }
        }
    }
    assert!(count > 0);
}
