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
    f.catalog.set_flow_capable("prefrontal", true);
    f.catalog.set_op(
        "prefrontal",
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
    assert_eq!(health["tokens"], json!([]));
    let details = common::call(
        &f,
        &Caller::Operator,
        "flow.details",
        json!({"flow_id":FLOW}),
    )
    .unwrap();
    assert_eq!(details["manifest"], legacy_manifest());

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
    let same = f
        .module
        .rt
        .install(&InstallRequest {
            script: SCRIPT.into(),
            manifest: legacy,
            author: "operator".into(),
            loop_override: false,
        })
        .unwrap();
    assert!(!same.new);
    assert_eq!(same.code_hash, hash);

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
    next.as_object_mut().unwrap().remove("llm");
    next["version"] = json!(2);
    next["ops"][0]["module"] = json!("prefrontal");
    next["explanation"] = json!("Reads boards to report work to Synapse.");
    next["changes"] = json!("Declares the board-read grant required for newly installed versions.");
    next["board_read"] = json!(["SYNAPSE"]);
    let installed = common::install(&f, &Caller::Operator, SCRIPT, &next);
    assert_eq!(installed["version"], 2);
    assert!(installed["card_id"].is_string());
    let list = common::call(&f, &Caller::Operator, "flow.list", json!({})).unwrap();
    assert_eq!(list["flows"][0]["approved_version"], 1);
    assert_eq!(list["flows"][0]["pending_version"], 2);
}

#[test]
fn stored_llm_member_is_ignored_without_loosening_the_manifest() {
    let mut value = legacy_manifest();
    value["llm"] = json!(["not a grant"]);
    let stored = basal_core::Manifest::parse_stored(&value.to_string()).unwrap();
    assert!(serde_json::to_value(&stored).unwrap().get("llm").is_none());
    assert!(matches!(
        stored.check(),
        Err(ManifestError::LlmGrantRemoved)
    ));

    let duplicate = format!(
        r#"{{"llm":null,{}"#,
        value.to_string().trim_start_matches('{')
    );
    assert!(matches!(
        basal_core::Manifest::parse_stored(&duplicate),
        Err(ManifestError::Decode(reason)) if reason.contains("duplicate field `llm`")
    ));
    value["unrecognized"] = json!(true);
    assert!(matches!(
        basal_core::Manifest::parse_stored(&value.to_string()),
        Err(ManifestError::Decode(reason)) if reason.contains("unknown field `unrecognized`")
    ));
}

#[test]
fn new_llm_grants_are_refused_at_intake_with_replacements() {
    let f = fixture("retired-llm-grant");
    let mut manifest = legacy_manifest();
    manifest["ops"] = json!([]);
    manifest["explanation"] = json!("Exercise manifest intake before any work is sent.");
    for grant in [manifest["llm"].clone(), Value::Null, json!({})] {
        manifest["llm"] = grant;
        let error = f
            .module
            .handle(
                &Caller::Operator,
                "flow.install",
                json!({"manifest":manifest.to_string(), "script":SCRIPT}),
            )
            .unwrap_err();
        assert_eq!(error.code, "llm_grant_removed");
        for required in ["llm_complete", "llm_classify", "model_cost_usd"] {
            assert!(error.message.contains(required), "{error}");
        }
        assert!(matches!(
            f.module.rt.register_package(SCRIPT, &manifest.to_string()),
            Err(basal_core::packages::PackageError::Install(
                InstallError::Manifest(ManifestError::LlmGrantRemoved)
            ))
        ));
    }
}

#[test]
fn new_install_still_requires_board_read_coupling() {
    let f = fixture("intake-manifest");
    for (ops, board_read) in [
        (json!([{"module":"prefrontal","op":"board.read"}]), None),
        (
            json!([{"module":"prefrontal","op":"board.read"}]),
            Some(json!([])),
        ),
        (json!([]), Some(json!(["SYNAPSE"]))),
    ] {
        let mut manifest = legacy_manifest();
        manifest.as_object_mut().unwrap().remove("llm");
        manifest["explanation"] = json!("Reads boards to report work to Synapse.");
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

#[test]
fn new_intake_refuses_the_retired_prefrontal_name() {
    let f = fixture("retired-name-intake");
    let mut manifest = legacy_manifest();
    // New intake refuses the retired `llm` grant first; drop it so this test
    // reaches the retired module name.
    manifest.as_object_mut().unwrap().remove("llm");
    manifest["board_read"] = json!(["$self"]);
    manifest["explanation"] = json!("Reads boards to report outstanding work.");
    let text = manifest.to_string();
    let request = InstallRequest {
        script: SCRIPT.into(),
        manifest: text.clone(),
        author: "operator".into(),
        loop_override: false,
    };
    let retired = |error: &ManifestError| matches!(error, ManifestError::Invalid { field, reason } if field == "ops[0].module" && reason == "the module is now `prefrontal`");
    assert!(
        matches!(f.module.rt.install(&request), Err(InstallError::Manifest(error)) if retired(&error))
    );
    assert!(
        matches!(f.module.rt.register_package(SCRIPT, &text), Err(basal_core::packages::PackageError::Install(InstallError::Manifest(error))) if retired(&error))
    );
    let error = f
        .module
        .dry
        .run(&basal_module::dryrun::DryRunRequest {
            script: SCRIPT.into(),
            manifest: text,
            author: "operator".into(),
            loop_override: false,
            mode: basal_module::dryrun::Mode::Capture,
            trigger: basal_module::dryrun::DryTrigger::Schedule { window: None },
            now_ms: common::T0,
        })
        .unwrap_err();
    assert!(error.to_string().contains("ops[0].module"), "{error}");
    assert!(
        error.to_string().contains("the module is now `prefrontal`"),
        "{error}"
    );
}

#[test]
fn intake_reports_the_field_naming_the_retired_module() {
    for (field, patch, expected) in [
        (
            "sinks",
            json!([{"agent":"prefrontal-core","digest_max":"silent"}]),
            "sinks",
        ),
        (
            "audience",
            json!({"kind":"workspace","id":"prefrontal-core"}),
            "audience.id",
        ),
        ("board_read", json!(["prefrontal-core"]), "board_read"),
        (
            "private_content_providers",
            json!(["prefrontal-core"]),
            "private_content_providers",
        ),
    ] {
        let mut manifest = legacy_manifest();
        manifest.as_object_mut().unwrap().remove("llm");
        manifest["ops"] = json!([]);
        manifest["explanation"] = json!("Exercises the field-specific retired-name refusal.");
        manifest[field] = patch;
        let refused = basal_core::Manifest::parse(&manifest.to_string());
        assert!(
            matches!(refused, Err(ManifestError::Invalid { field, reason }) if field == expected && reason == "the module is now `prefrontal`"),
            "{field}"
        );
    }
}

#[test]
fn stored_prefrontal_template_reregisters_and_runs_a_new_instance() {
    let f = common::fixture("retired-name-template", common::Options::with_worker());
    f.catalog.set_flow_capable("prefrontal", true);
    f.catalog.set_op(
        "prefrontal",
        "board.read",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    let template = json!({"id":"legacy-boards","version":1,"purpose":"Read boards",
        "params":{"interval":{"type":"duration","min":"30m","max":"2h"}},
        "template":{"max_per_agent":1,"max_lifetime":"2h"},
        "trigger":{"schedule":{"interval":"$params.interval"}},"board_read":["$self"],
        "ops":[{"module":"prefrontal-core","op":"board.read"}]})
    .to_string();
    let script = "return await ops.call('prefrontal-core','board.read', {});";
    let hash = basal_core::test_support::seed_stored_version(&f.module.rt, script, &template, true)
        .unwrap();
    let registration = f.module.rt.register_package(script, &template).unwrap();
    assert_eq!(registration["new"], false);
    assert_eq!(registration["code_hash"], basal_proto::util::hex(&hash));
    assert!(
        f.module
            .rt
            .register_package("return 2;", &template)
            .is_err()
    );
    f.mock.inject(
        "prefrontal-core",
        "board.read",
        &[basal_host::mock::Fault {
            proven_unsent: true,
            effect_applied: false,
        }],
    );
    let instance = f
        .module
        .rt
        .instantiate(FLOW, 1, "SYNAPSE", &json!({"interval":"1h"}), "1h")
        .unwrap();
    assert_eq!(instance["new"], true);
    let id = instance["flow_id"].as_str().unwrap();
    let run = common::admit(&f, id, "legacy-template");
    f.module.engine.run_until_idle(20).unwrap();
    let run = f.module.rt.run(&run).unwrap();
    assert_eq!(run.state, basal_core::RunState::Succeeded, "{run:?}");
    assert_eq!(run.manifest, template);
    assert_eq!(run.code_hash, hash);
    assert_eq!(f.mock.total_sends(), 1);
}
