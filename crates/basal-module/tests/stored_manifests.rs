mod common;

use basal_core::{Admission, InstallError, InstallRequest, ManifestError};
use basal_host::{OpDecl, OpKind};
use basal_module::caller::Caller;
use basal_proto::JsonText;
use rusqlite::params;
use serde_json::{Value, json};

const FLOW: &str = "legacy-boards";
const SCRIPT: &str = "return 1;";

fn legacy_manifest() -> Value {
    json!({
        "id": FLOW, "version": 1, "purpose": "Read boards",
        "trigger": {"schedule": {"interval": "1h"}},
        "ops": [{"module": "prefrontal-core", "op": "board.read"}],
        "llm": {"iq": 1, "max_output": 100,
            "token_cap": {"tokens": 1000, "window": "1h"}}
    })
}

fn fixture(tag: &str) -> common::Fixture {
    let f = common::fixture(tag, common::Options::default());
    f.catalog.set_flow_capable("prefrontal-core", true);
    f.catalog.set_op(
        "prefrontal-core",
        "board.read",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    f
}

#[test]
fn legacy_approved_manifest_remains_readable_and_upgradeable() {
    let f = fixture("stored-manifest");
    let legacy = legacy_manifest().to_string();
    let hash = basal_core::ids::code_hash(SCRIPT, &legacy);
    // Seed the exact bytes and approval an older build could have written,
    // without passing the legacy manifest through today's intake validator.
    f.module.rt.store().write(|tx| {
        tx.execute(
            "INSERT INTO flows(flow_id,created_at,approved_version,owner,first_approved_at) VALUES (?1,?2,1,'operator',?2)",
            params![FLOW, common::T0],
        )?;
        tx.execute(
            "INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,loop_override,state,installed_at,approved_at,approval_ref,approval_kind) VALUES (?1,1,?2,?3,?4,'operator',0,'approved',?5,?5,'legacy-approval','card')",
            params![FLOW, hash.as_slice(), legacy, SCRIPT, common::T0],
        )?;
        Ok(())
    }).unwrap();

    let list = common::call(&f, &Caller::Operator, "flow.list", json!({})).unwrap();
    assert_eq!(list["flows"][0]["flow_id"], FLOW);
    assert_eq!(list["flows"][0]["approved_version"], 1);
    assert_eq!(list["flows"][0]["state"], "enabled");
    let health = common::call(&f, &Caller::Core, "flow.health", json!({})).unwrap();
    assert_eq!(health["flows"][0]["state"], "enabled");
    assert_eq!(health["tokens"][0]["flow_id"], FLOW);
    assert_eq!(health["tokens"][0]["window_ms"], common::HOUR);
    assert_eq!(health["tokens"][0]["actual"], 0);
    let usage = f.module.rt.token_usage(FLOW, common::T0).unwrap().unwrap();
    assert_eq!(usage.window_ms, common::HOUR);
    assert_eq!(usage.counted(), 0);
    let batch = f.module.rt.token_usage_many(&[FLOW], common::T0).unwrap();
    assert_eq!(batch[FLOW], usage);

    let admitted = f
        .module
        .rt
        .admit_trigger(FLOW, "legacy-run", JsonText::null())
        .unwrap();
    let Admission::Admitted { run_id } = admitted else {
        panic!("legacy version was not admitted: {admitted:?}");
    };
    let run = f.module.rt.run(&run_id).unwrap();
    assert_eq!(run.flow_version, Some(1));
    assert_eq!(run.manifest, legacy);
    assert_eq!(run.code_hash, hash);

    let dry = common::call(
        &f,
        &Caller::Operator,
        "flow.dry_run",
        json!({
            "flow_id": FLOW, "version": 1, "mode": "capture"
        }),
    )
    .unwrap();
    assert_eq!(dry["version"], 1);
    assert!(!dry["summary"]["runs"].as_array().unwrap().is_empty());

    let mut next = legacy_manifest();
    next["version"] = json!(2);
    next["board_read"] = json!(["SYNAPSE"]);
    let installed = common::install(&f, &Caller::Operator, SCRIPT, &next);
    assert_eq!(installed["version"], 2);
    assert!(installed["card_id"].is_string());
    let list = common::call(&f, &Caller::Operator, "flow.list", json!({})).unwrap();
    assert_eq!(list["flows"][0]["approved_version"], 1);
    assert_eq!(list["flows"][0]["pending_version"], 2);
}

#[test]
fn new_install_still_requires_board_read_coupling() {
    let f = fixture("intake-manifest");
    for (ops, board_read) in [
        (
            json!([{"module":"prefrontal-core","op":"board.read"}]),
            None,
        ),
        (
            json!([{"module":"prefrontal-core","op":"board.read"}]),
            Some(json!([])),
        ),
        (json!([]), Some(json!(["SYNAPSE"]))),
    ] {
        let mut manifest = legacy_manifest();
        manifest["ops"] = ops;
        if let Some(board_read) = board_read {
            manifest["board_read"] = board_read;
        }
        let request = InstallRequest {
            script: SCRIPT.into(),
            manifest: manifest.to_string(),
            author: "operator".into(),
            loop_override: false,
        };
        assert!(matches!(
            f.module.rt.install(&request),
            Err(InstallError::Manifest(ManifestError::Invalid { field, .. })) if field == "board_read"
        ));
        let error = f
            .module
            .handle(
                &Caller::Operator,
                "flow.install",
                json!({
                    "script": SCRIPT, "manifest": request.manifest
                }),
            )
            .unwrap_err();
        assert!(error.message.contains("board_read"), "{error}");
        assert!(f.module.rt.flow(FLOW).unwrap().is_none());
        assert!(matches!(
            f.module.rt.register_package(SCRIPT, &request.manifest),
            Err(basal_core::packages::PackageError::Install(InstallError::Manifest(
                ManifestError::Invalid { field, .. }
            ))) if field == "board_read"
        ));
        let error = f
            .module
            .dry
            .run(&basal_module::dryrun::DryRunRequest {
                script: SCRIPT.into(),
                manifest: request.manifest,
                author: "operator".into(),
                loop_override: false,
                mode: basal_module::dryrun::Mode::Capture,
                trigger: basal_module::dryrun::DryTrigger::Schedule { window: None },
                now_ms: common::T0,
            })
            .unwrap_err();
        assert!(error.to_string().contains("board_read"), "{error}");
    }
}
