use std::{collections::BTreeSet, fs, path::PathBuf};

use basal_host::{InstallStatus, core_host::decode_install_status};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SUMS: &str = include_str!("../../../test-vectors/flow-install-status-v1/SHA256SUMS");
const SUMS_SHA256: &str = "1d25404bd08c53957245b361bdee9f34adb7badf7d1b53edf64e5cf0c91e4d99";
const README_SHA256: &str = "81f42a020f5b5570ed49a84b8125bc06fdb80ba7787c825a097ef1e362b6c8f8";

fn directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/flow-install-status-v1")
}

fn sample(name: &str) -> Value {
    serde_json::from_slice(&fs::read(directory().join(name)).unwrap()).unwrap()
}

#[test]
fn install_status_manifest_and_every_file_match_pinned_hashes() {
    let manifest = fs::read(directory().join("SHA256SUMS")).unwrap();
    assert_eq!(format!("{:x}", Sha256::digest(&manifest)), SUMS_SHA256);
    assert_eq!(manifest, SUMS.as_bytes());
    // Core's manifest lists the JSON samples, not its explanatory README.
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(fs::read(directory().join("README.md")).unwrap())
        ),
        README_SHA256
    );
    let mut covered = BTreeSet::from(["SHA256SUMS".to_owned(), "README.md".to_owned()]);
    for line in SUMS.lines() {
        let (expected, name) = line.split_once("  ").unwrap();
        assert!(!name.contains(['/', '\\']));
        assert!(
            covered.insert(name.to_owned()),
            "duplicate manifest member {name}"
        );
        let bytes = fs::read(directory().join(name)).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), expected, "{name}");
    }
    let actual: BTreeSet<_> = fs::read_dir(directory())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(covered, actual);
    assert_eq!(covered.len(), 7);
}

#[test]
fn catalog_active_revoked_and_unknown_samples_decode() {
    let bound = sample("bound.json");
    assert!(bound.get("pack").is_some());
    assert!(bound.get("scope").is_none());
    assert_eq!(
        decode_install_status(&bound),
        Ok(InstallStatus::Active {
            code_hash: bound["code_hash"].as_str().unwrap().to_owned(),
            scope: None,
        })
    );
    let revoked = sample("revoked.json");
    assert_eq!(
        decode_install_status(&revoked),
        Ok(InstallStatus::Revoked {
            code_hash: Some(revoked["code_hash"].as_str().unwrap().to_owned()),
        })
    );
    assert_eq!(
        decode_install_status(&sample("unknown.json")),
        Ok(InstallStatus::Unknown)
    );
}

#[test]
fn catalog_held_sample_decodes_as_nonterminal_hold() {
    let held = sample("held.json");
    assert!(held.get("pack").is_some());
    assert!(held.get("scope").is_none());
    assert_eq!(
        decode_install_status(&held),
        Ok(InstallStatus::Held {
            code_hash: held["code_hash"].as_str().unwrap().to_owned(),
            reason: "catalog_status_stale".to_owned(),
            stale_since_ms: Some(900100),
        })
    );
}

#[test]
fn held_accepts_future_reasons_and_optional_stale_time_but_requires_valid_hash() {
    for reason in [
        "ability_setup_pending",
        "ability_permission_denied",
        "future_core_reason",
    ] {
        let mut held = sample("held.json");
        held["catalog"] = json!({"code": reason});
        assert_eq!(
            decode_install_status(&held),
            Ok(InstallStatus::Held {
                code_hash: held["code_hash"].as_str().unwrap().to_owned(),
                reason: reason.to_owned(),
                stale_since_ms: None,
            })
        );
    }
    for hash in [Value::Null, json!("malformed"), json!(123)] {
        let mut held = sample("held.json");
        held["code_hash"] = hash;
        assert!(decode_install_status(&held).is_err());
    }
}
