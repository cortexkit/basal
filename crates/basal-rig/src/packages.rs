//! Independent package identity checks, derived from the public package contract.

use serde_json::Value;

/// Stopping an instance by removal or revoked approval must not change whether
/// it is enabled, or undo an existing disable's actor, reason and timestamp.
pub fn same_enable_state(before: &Value, after: &Value) -> bool {
    ["state", "disabled_by", "disabled_reason", "disabled_at"]
        .iter()
        .all(|field| before.get(*field).is_some() && before.get(*field) == after.get(*field))
}

/// Names and versions deliberately do not enter an instance's durable identity.
pub fn instance_id(package: &str, agent_id: &str) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"basal-instance-id-v1\0");
    for field in [package, agent_id] {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    format!("{package}_{}", &hash.finalize().to_hex()[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_identity_matches_all_documented_vectors() {
        for (package, agent, expected) in [
            (
                "dark-wake",
                "agent_47120287c700722b",
                "dark-wake_d04794834aa77fe7",
            ),
            (
                "dark-wake",
                "agent_d1e7002e2c9b9f43",
                "dark-wake_4dc17caa47226d32",
            ),
            (
                "ci-watch",
                "agent_47120287c700722b",
                "ci-watch_461d8a6c652aa677",
            ),
        ] {
            assert_eq!(instance_id(package, agent), expected);
        }
    }
}

#[cfg(test)]
mod enable_tests {
    use super::*;
    #[test]
    fn enable_state_evidence_rejects_changed_or_missing_switches() {
        let before = serde_json::json!({"state":"enabled","disabled_by":null,"disabled_reason":null,"disabled_at":null});
        assert!(same_enable_state(&before, &before));
        for field in ["state", "disabled_by", "disabled_reason", "disabled_at"] {
            let mut after = before.clone();
            after[field] = serde_json::json!("changed");
            assert!(!same_enable_state(&before, &after));
            after.as_object_mut().unwrap().remove(field);
            assert!(!same_enable_state(&before, &after));
        }
        assert!(!same_enable_state(&Value::Null, &Value::Null));
    }
}
