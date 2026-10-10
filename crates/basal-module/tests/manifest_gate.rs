//! The build gate for the subc manifest: the built `ck-basal --manifest`
//! must print a manifest subc-protocol's own type accepts and its own
//! checks pass, declaring exactly the ops basal serves. A manifest
//! regression fails here, in the build, rather than at the daemon's
//! registration.

use basal_testkit::dev_binary;
use std::collections::BTreeSet;
use std::process::Command;

use basal_module::manifest::{BUILD_GIT_DIRTY, BUILD_GIT_SHA, MODULE_ID, OPERATIONS};
use serde_json::Value;
use subc_protocol::PROTOCOL_VERSION;
use subc_protocol::manifest::{
    ManagementOperationKind, ModuleManifest, ProviderRole, validate_manifest_capability_grammar,
};

#[test]
fn the_built_binary_prints_a_manifest_subc_protocol_accepts() {
    let output = Command::new(dev_binary(env!("CARGO_BIN_EXE_ck-basal")))
        .arg("--manifest")
        // Offline inspection must not need supervision.
        .env_remove("SUBC_MODULE_ID")
        .env_remove("SUBC_LAUNCH_NONCE")
        .output()
        .expect("run ck-basal --manifest");
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let raw: Value = serde_json::from_slice(&output.stdout).expect("--manifest prints JSON");
    // The daemon's own checks on the raw form, then the typed decode.
    validate_manifest_capability_grammar(&raw).expect("capability grammar");
    let manifest: ModuleManifest =
        serde_json::from_value(raw.clone()).expect("decodes as subc-protocol's ModuleManifest");
    manifest
        .validate_capability_grammar()
        .expect("typed capability grammar");
    if let Some(provenance) = &manifest.provenance {
        provenance.validate().expect("provenance");
    }
    // Nothing is lost or invented by the decode: re-encoding gives the same
    // document, so no field the binary prints is unknown to the protocol.
    assert_eq!(
        serde_json::to_value(&manifest).expect("re-encode"),
        raw,
        "the printed manifest has fields subc-protocol does not know"
    );

    assert_eq!(manifest.module_id, MODULE_ID);
    assert_eq!(manifest.protocol_ver, PROTOCOL_VERSION);
    assert_eq!(manifest.module_version, env!("CARGO_PKG_VERSION"));
    let [
        ProviderRole::ToolProvider { tools, .. },
        ProviderRole::ManagementSurface { operations, .. },
    ] = manifest.provides.as_slice()
    else {
        panic!(
            "one tool provider and one management surface, got {:?}",
            manifest.provides
        );
    };
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "codemode");
    assert_eq!(tools[0].schema, basal_module::tool::schema());
    let declared: BTreeSet<(String, bool)> = operations
        .iter()
        .map(|o| (o.name.clone(), o.kind == ManagementOperationKind::Query))
        .collect();
    let served: BTreeSet<(String, bool)> = OPERATIONS
        .iter()
        .map(|(n, k, _)| ((*n).to_owned(), *k == ManagementOperationKind::Query))
        .collect();
    assert_eq!(declared, served);
    for name in [
        "package.register",
        "package.get",
        "flow.instance.ensure",
        "flow.instance.remove",
        "flow.install",
        "flow.dry_run",
        "flow.health",
        "flow.list",
        "flow.reconcile",
        "flow.drain",
        "flow.disable",
        "flow.enable",
    ] {
        assert!(
            declared.iter().any(|(n, _)| n == name),
            "{name} is not declared"
        );
    }
    assert!(operations.iter().all(|o| o.description.is_some()));
    assert!(
        operations
            .iter()
            .any(|o| o.name == "flow.list" && o.kind == ManagementOperationKind::Query)
    );
}

#[test]
fn the_manifest_and_version_declare_the_embedded_build_revision() {
    let output = Command::new(dev_binary(env!("CARGO_BIN_EXE_ck-basal")))
        .arg("--manifest")
        .output()
        .expect("run ck-basal --manifest");
    let manifest: ModuleManifest = serde_json::from_slice(&output.stdout).expect("manifest");
    let provenance = manifest
        .provenance
        .expect("the manifest declares build provenance");
    // A revision is declared only for a clean build that was told its commit
    // (script/stage.sh does both); every other build says it never derived one.
    let declared = (BUILD_GIT_SHA != "unknown" && !BUILD_GIT_DIRTY).then_some(BUILD_GIT_SHA);
    assert_eq!(provenance.build_git_sha.as_deref(), declared);
    assert_eq!(
        provenance.build_git_sha_absence_reason.is_some(),
        declared.is_none()
    );

    let output = Command::new(dev_binary(env!("CARGO_BIN_EXE_ck-basal")))
        .arg("--version")
        .output()
        .expect("run ck-basal --version");
    assert!(output.status.success());
    let line = String::from_utf8_lossy(&output.stdout);
    assert!(
        line.starts_with(&format!(
            "ck-basal {} ({BUILD_GIT_SHA}",
            env!("CARGO_PKG_VERSION")
        )),
        "{line}"
    );
}

#[test]
fn anything_but_the_manifest_or_version_flag_or_a_subc_connection_is_refused() {
    for args in [
        vec![],
        vec!["--help"],
        vec!["serve"],
        vec!["--version", "x"],
    ] {
        let output = Command::new(dev_binary(env!("CARGO_BIN_EXE_ck-basal")))
            .args(&args)
            .output()
            .expect("run ck-basal");
        assert_eq!(output.status.code(), Some(64), "{args:?}");
    }
}
