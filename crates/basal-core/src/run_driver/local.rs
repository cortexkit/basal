use std::collections::BTreeMap;

use basal_proto::{CallKind, Primitive};
use serde_json::{Value, json};

use super::*;

fn refused(code: &str, message: &str) -> Prepared {
    Prepared::Refused {
        class: StoredClass::Local,
        value: crate::kv::rejection(code, message).value,
        notify: false,
    }
}

pub(super) fn prepare(
    store: &Store,
    live: &LiveRun,
    state: &State,
    call: &HostCall,
    limits: crate::kv::KvLimits,
) -> Result<Prepared> {
    let args: Value =
        serde_json::from_str(call.args.as_str()).map_err(|e| CoreError::Invalid(e.to_string()))?;
    let Some(key) = args
        .get("key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
    else {
        return Ok(refused(
            "invalid_arguments",
            "kv calls need a nonempty string key",
        ));
    };
    if key.len() > limits.max_key_bytes {
        return Ok(refused("kv_limit", "kv key is too large"));
    }
    let mut values = store.read(|conn| {
        let mut stmt = conn.prepare("SELECT key,value FROM kv WHERE flow_id=?1")?;
        Ok(stmt
            .query_map([&live.flow_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
    })?;
    for (key, value) in &state.overlay {
        if let Some(value) = value {
            values.insert(key.clone(), value.clone());
        } else {
            values.remove(key);
        }
    }
    let (value, effect) = match call.kind {
        CallKind::Primitive(Primitive::KvGet) => (
            JsonText::new(values.get(key).cloned().unwrap_or_else(|| "null".into()))
                .map_err(|e| CoreError::Corrupt(e.to_string()))?,
            None,
        ),
        CallKind::Primitive(Primitive::KvSet) => {
            let value = args
                .get("value")
                .cloned()
                .unwrap_or(Value::Null)
                .to_string();
            if value.len() > limits.max_value_bytes {
                return Ok(refused("kv_limit", "kv value is too large"));
            }
            values.insert(key.into(), value.clone());
            let bytes = values.iter().try_fold(0usize, |n, (k, v)| {
                n.checked_add(k.len()).and_then(|n| n.checked_add(v.len()))
            });
            if bytes.is_none_or(|n| n > limits.max_flow_bytes) {
                return Ok(refused("kv_limit", "flow kv is too large"));
            }
            (
                JsonText::new("true").expect("boolean JSON"),
                Some(KvEffect::Set {
                    key: key.into(),
                    value: JsonText::new(value).map_err(|e| CoreError::Invalid(e.to_string()))?,
                }),
            )
        }
        CallKind::Primitive(Primitive::KvDelete) => (
            JsonText::new(json!(values.contains_key(key)).to_string()).expect("boolean JSON"),
            Some(KvEffect::Delete { key: key.into() }),
        ),
        _ => {
            return Err(CoreError::Invalid(
                "KV authorization for a non-KV call".into(),
            ));
        }
    };
    Ok(Prepared::Local { value, effect })
}
