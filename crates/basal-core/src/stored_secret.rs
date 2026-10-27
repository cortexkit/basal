//! Inspect only secret-bearing historical rows; unrelated old manifests must
//! not acquire new lifecycle validation from the secrets feature.
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

struct Presence(bool);
impl<'de> Deserialize<'de> for Presence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Keys;
        impl<'de> Visitor<'de> for Keys {
            type Value = Presence;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("manifest object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Presence, M::Error> {
                let mut found = false;
                while let Some(key) = map.next_key::<String>()? {
                    found |= key == "secrets";
                    map.next_value::<IgnoredAny>()?;
                }
                Ok(Presence(found))
            }
        }
        deserializer.deserialize_map(Keys)
    }
}

fn warn_unreadable(text: &str, flow: &str, version: u32) {
    static WARNED: OnceLock<Mutex<VecDeque<[u8; 32]>>> = OnceLock::new();
    let digest = *blake3::hash(text.as_bytes()).as_bytes();
    let mut warned = WARNED
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if warned.contains(&digest) {
        return;
    }
    if warned.len() == 256 {
        warned.pop_front();
    }
    warned.push_back(digest);
    drop(warned);
    tracing::warn!(target:"consent", flow_id=flow, version, "unreadable historical manifest ignored by secrets inspection");
}

pub(crate) fn has_secrets(text: &str, flow: &str, version: u32) -> bool {
    if !text.contains("secrets") && !text.contains('\\') {
        return false;
    }
    match serde_json::from_str::<Presence>(text) {
        Ok(Presence(false)) => false,
        Ok(Presence(true)) => match crate::manifest::Manifest::parse_stored(text) {
            Ok(manifest) => manifest.secrets.is_some(),
            Err(_) => {
                warn_unreadable(text, flow, version);
                false
            }
        },
        Err(_) => {
            warn_unreadable(text, flow, version);
            false
        }
    }
}

pub(crate) fn global_audience(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .is_ok_and(|value| value["audience"]["kind"] == "global")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review_stored_secret_presence_handles_escaped_keys_null_and_unrelated_rows() {
        let manifest = serde_json::json!({"id":"presence","version":1,"purpose":"presence",
            "trigger":{"schedule":{"interval":"1h"}},"secrets":[]})
        .to_string();
        assert!(has_secrets(&manifest, "presence", 1));
        assert!(has_secrets(
            &manifest.replace("\"secrets\"", "\"secr\\u0065ts\""),
            "presence",
            1
        ));
        assert!(has_secrets(
            &manifest.replace("\"secrets\":[]", "\"secrets\":null"),
            "presence",
            1
        ));
        assert!(!has_secrets("{\"old_schema\":true}", "old", 1));
        assert!(!has_secrets(
            "{\"purpose\":\"secrets are not declared here\"}",
            "old",
            2
        ));
    }
}
