mod common;
use basal_host::{OpDecl, OpKind};
use basal_module::caller::Caller;
use common::{Options, events_manifest, fixture};
use serde_json::json;

#[test]
fn core_op_install_requires_catalog_capability_for_every_author() {
    let f = fixture("core-op-cap", Options::default());
    f.catalog.set_op(
        "prefrontal-core",
        "board.write",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    for caller in [Caller::Operator, common::agent("SYNAPSE")] {
        let mut manifest = events_manifest(if caller == Caller::Operator {
            "flow-op-global"
        } else {
            "flow-op-owned"
        });
        manifest["ops"] = json!([{"module":"prefrontal-core","op":"board.write"}]);
        let params = json!({"script":"return 7;","manifest":manifest.to_string()});
        let error = f
            .module
            .handle(&caller, "flow.install", params.clone())
            .unwrap_err();
        assert!(
            error.message.contains("prefrontal-core") && error.message.contains("board.write"),
            "{error:?}"
        );
        f.catalog.set_flow_capable("prefrontal-core", true);
        assert!(f.module.handle(&caller, "flow.install", params).is_ok());
        f.catalog.set_flow_capable("prefrontal-core", false);
    }
}

#[test]
fn dry_run_rechecks_core_op_catalog_capability() {
    let f = fixture("dry-core-cap", Options::default());
    f.catalog.set_op(
        "prefrontal-core",
        "board.write",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    let mut manifest = events_manifest("flow-dry-core");
    manifest["ops"] = json!([{"module":"prefrontal-core","op":"board.write"}]);
    f.catalog.set_flow_capable("prefrontal-core", true);
    f.module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({"script":"return 7;","manifest":manifest.to_string()}),
        )
        .unwrap();
    let params = json!({"flow_id":"flow-dry-core","trigger":{}});
    assert!(
        f.module
            .handle(&Caller::Operator, "flow.dry_run", params.clone())
            .is_ok()
    );
    f.catalog.set_flow_capable("prefrontal-core", false);
    let error = f
        .module
        .handle(&Caller::Operator, "flow.dry_run", params)
        .unwrap_err();
    assert!(
        error.message.contains("prefrontal-core") && error.message.contains("board.write"),
        "{error:?}"
    );
}

#[test]
fn board_hygiene_manifest_installs_without_core_flow_scopes() {
    let f = fixture("board-carrier-cap", Options::default());
    f.catalog.set_flow_capable("prefrontal-core", false);
    for op in ["board.read", "work.unsettled"] {
        f.catalog.set_op(
            "prefrontal-core",
            op,
            OpDecl {
                kind: Some(OpKind::Query),
                cause_echo: false,
                shell_capable: false,
            },
        );
    }
    f.module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({
                "script": include_str!("../../../flows/board-hygiene/flow.js"),
                "manifest": include_str!("../../../flows/board-hygiene/manifest.json"),
            }),
        )
        .unwrap();
    f.module
        .handle(
            &Caller::Operator,
            "flow.dry_run",
            json!({"flow_id":"board-hygiene","trigger":{}}),
        )
        .unwrap();
}

#[test]
fn work_unsettled_installs_without_core_flow_scopes() {
    // work.unsettled is a core carrier op: basal sends it on its own route, so
    // neither install nor a dry run asks core for flow-scopes/v1. An ordinary
    // core op in the same manifest would be refused (see the test above).
    let f = fixture("work-unsettled-cap", Options::default());
    f.catalog.set_flow_capable("prefrontal-core", false);
    f.catalog.set_op(
        "prefrontal-core",
        "work.unsettled",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    let mut manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../flows/board-hygiene/manifest.json")).unwrap();
    manifest["id"] = json!("work-unsettled-only");
    manifest["ops"] = json!([{"module":"prefrontal-core","op":"work.unsettled"}]);
    manifest.as_object_mut().unwrap().remove("board_read");
    f.module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({"script":"return 7;","manifest":manifest.to_string()}),
        )
        .unwrap();
    f.module
        .handle(
            &Caller::Operator,
            "flow.dry_run",
            json!({"flow_id":"work-unsettled-only","trigger":{}}),
        )
        .unwrap();
}

#[test]
fn board_read_op_and_target_grants_are_required_at_install_dry_run_and_package_registration() {
    let f = fixture("board-read-required", Options::default());
    for missing in ["board_read", "ops"] {
        let mut manifest: serde_json::Value =
            serde_json::from_str(include_str!("../../../flows/board-hygiene/manifest.json"))
                .unwrap();
        manifest["id"] = json!(format!("missing-{missing}"));
        manifest.as_object_mut().unwrap().remove(missing);
        for method in ["flow.install", "package.register"] {
            let error = f
                .module
                .handle(
                    &Caller::Operator,
                    method,
                    json!({"script":"return 7;","manifest":manifest.to_string()}),
                )
                .unwrap_err();
            assert!(error.message.contains("board_read"), "{method}: {error}");
        }
        let mut template = manifest.clone();
        template["params"] = json!({"count":{"type":"integer","min":1,"max":2}});
        template["template"] = json!({"max_per_agent":1,"max_lifetime":"1h"});
        let error = f
            .module
            .handle(
                &Caller::Operator,
                "package.register",
                json!({"script":"return 7;","manifest":template.to_string()}),
            )
            .unwrap_err();
        assert!(error.message.contains("board_read"), "template: {error}");

        let request = basal_module::dryrun::DryRunRequest {
            script: "return 7;".into(),
            manifest: manifest.to_string(),
            author: "operator".into(),
            loop_override: false,
            mode: basal_module::dryrun::Mode::Capture,
            trigger: basal_module::dryrun::DryTrigger::Synthetic(json!({})),
            now_ms: common::T0,
        };
        let error = f.module.dry.run(&request).unwrap_err();
        assert!(error.to_string().contains("board_read"), "dry run: {error}");
    }
}
