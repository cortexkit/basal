use serde_json::Value;

use super::{Manifest, ManifestError, invalid};

const MAX_PACK_ENTRIES: usize = 16;

pub(super) fn refused(code: &'static str, reason: &str) -> ManifestError {
    ManifestError::Pack {
        code,
        reason: reason.into(),
    }
}

// Preflight only JSON types and members, which serde also enforces. Semantic
// limits belong to intake; stored versions must not acquire today's policy.
pub(super) fn check_decode_shape(value: &Value) -> Result<(), ManifestError> {
    for (list, required, optional) in [
        ("abilities", &["key", "token", "scope"][..], &[][..]),
        ("destinations", &["key", "kind"][..], &["severity_max"][..]),
    ] {
        let Some(entries) = value.get(list) else {
            continue;
        };
        let entries = entries
            .as_array()
            .ok_or_else(|| invalid(list, "must be an array"))?;
        for (i, entry) in entries.iter().enumerate() {
            let path = format!("{list}[{i}]");
            let fields = entry
                .as_object()
                .ok_or_else(|| invalid(&path, "must be an object"))?;
            for field in fields.keys() {
                if !required.contains(&field.as_str()) && !optional.contains(&field.as_str()) {
                    return Err(invalid(format!("{path}.{field}"), "unknown field"));
                }
            }
            for field in required.iter().chain(optional) {
                let field_path = format!("{path}.{field}");
                let Some(value) = fields.get(*field) else {
                    if required.contains(field) {
                        return Err(invalid(field_path, "required"));
                    }
                    continue;
                };
                if *field == "scope" {
                    let keys = value
                        .as_array()
                        .ok_or_else(|| invalid(&field_path, "must be an array of strings"))?;
                    for (j, key) in keys.iter().enumerate() {
                        if !key.is_string() {
                            return Err(invalid(format!("{field_path}[{j}]"), "must be a string"));
                        }
                    }
                } else if !value.is_string() {
                    return Err(invalid(field_path, "must be a string"));
                }
            }
        }
    }
    Ok(())
}

fn check_key(path: &str, key: &str) -> Result<(), ManifestError> {
    if key.is_empty()
        || key.len() > 40
        || !key.as_bytes()[0].is_ascii_lowercase()
        || !key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(invalid(path, "must match [a-z][a-z0-9_]{0,39}"));
    }
    Ok(())
}

// Exactly ^cap/[a-z][a-z0-9_.-]*@[1-9][0-9]*$: majors have no leading
// zeroes and are checked lexically, without a machine-integer size limit.
pub(super) fn valid_token(token: &str) -> bool {
    let Some((name, major)) = token.strip_prefix("cap/").and_then(|s| s.split_once('@')) else {
        return false;
    };
    !name.is_empty()
        && name.as_bytes()[0].is_ascii_lowercase()
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
        && major
            .as_bytes()
            .first()
            .is_some_and(|b| matches!(b, b'1'..=b'9'))
        && major.bytes().all(|b| b.is_ascii_digit())
}

fn check_keys<'a>(list: &str, keys: impl Iterator<Item = &'a str>) -> Result<(), ManifestError> {
    let mut seen = std::collections::BTreeSet::new();
    for (i, key) in keys.enumerate() {
        if i >= MAX_PACK_ENTRIES {
            return Err(invalid(list, "at most 16 entries"));
        }
        let path = format!("{list}[{i}].key");
        check_key(&path, key)?;
        if !seen.insert(key) {
            return Err(invalid(path, "duplicate key"));
        }
    }
    Ok(())
}

pub(super) fn check(manifest: &Manifest) -> Result<(), ManifestError> {
    if let Some(abilities) = &manifest.abilities {
        check_keys("abilities", abilities.iter().map(|a| a.key.as_str()))?;
        for (i, ability) in abilities.iter().enumerate() {
            if !valid_token(&ability.token) {
                return Err(invalid(
                    format!("abilities[{i}].token"),
                    "must match ^cap/[a-z][a-z0-9_.-]*@[1-9][0-9]*$",
                ));
            }
            let path = format!("abilities[{i}].scope");
            if !(1..=8).contains(&ability.scope.len()) {
                return Err(invalid(&path, "requires 1 to 8 unique keys"));
            }
            let mut seen = std::collections::BTreeSet::new();
            for (j, key) in ability.scope.iter().enumerate() {
                let path = format!("{path}[{j}]");
                check_key(&path, key)?;
                if !seen.insert(key) {
                    return Err(invalid(path, "duplicate scope key"));
                }
            }
        }
    }
    if let Some(destinations) = &manifest.destinations {
        check_keys("destinations", destinations.iter().map(|d| d.key.as_str()))?;
        for (i, destination) in destinations.iter().enumerate() {
            if !matches!(
                destination.kind.as_str(),
                "room" | "agent_inbox" | "notice" | "plexus_action"
            ) {
                return Err(invalid(
                    format!("destinations[{i}].kind"),
                    "must be room, agent_inbox, notice or plexus_action",
                ));
            }
            let path = format!("destinations[{i}].severity_max");
            match (
                destination.kind.as_str(),
                destination.severity_max.as_deref(),
            ) {
                ("notice", Some("info" | "attention")) => {}
                ("notice", _) => {
                    return Err(invalid(
                        path,
                        "required for notice; must be info or attention",
                    ));
                }
                (_, Some(_)) => return Err(invalid(path, "only allowed on notice destinations")),
                (_, None) => {}
            }
        }
    }
    if manifest.abilities.as_ref().is_some_and(|a| !a.is_empty())
        && manifest
            .net
            .as_ref()
            .is_some_and(|net| !net.fetch.is_empty())
    {
        return Err(refused(
            "pack_net_fetch_forbidden",
            "net.fetch: abilities cannot be combined with net.fetch grants",
        ));
    }
    if (manifest.abilities.is_some() || manifest.destinations.is_some())
        && (!manifest.sinks.is_empty() || !manifest.ops.is_empty())
    {
        return Err(refused(
            "pack_literal_grant",
            "sinks/ops: packs cannot use literal sinks or ops grants",
        ));
    }
    Ok(())
}
