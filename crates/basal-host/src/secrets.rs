//! Vault references and public target constraints, never secret material.
use base64::Engine;
use serde::{Deserialize, Serialize};

mod echo;
pub(crate) use echo::EchoGuard;

pub(crate) fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut offset = 0;
    while offset < input.len() {
        if input[offset] == b'%' {
            let hi = input
                .get(offset + 1)
                .and_then(|b| (*b as char).to_digit(16));
            let lo = input
                .get(offset + 2)
                .and_then(|b| (*b as char).to_digit(16));
            if let Some((hi, lo)) = hi.zip(lo) {
                output.push((hi * 16 + lo) as u8);
                offset += 3;
                continue;
            }
        }
        output.push(if input[offset] == b'+' {
            b' '
        } else {
            input[offset]
        });
        offset += 1;
    }
    output
}

/// Best-effort clearing of owned reply values, not transport or allocator copies.
pub(crate) fn clear_reply(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            let mut bytes = std::mem::take(text).into_bytes();
            bytes.fill(0);
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(clear_reply),
        serde_json::Value::Object(values) => values.values_mut().for_each(clear_reply),
        _ => {}
    }
}

/// Exact raw, base64 and percent-encoded substitution. HTTP responses use the
/// checked variant with a response-wide budget and withhold exhausted fields.
pub fn redact(input: &[u8], key: &[u8], name: &str) -> Vec<u8> {
    let mut remaining = REDACTION_WORK;
    redact_with_budget(input, key, name, &mut remaining)
        .unwrap_or_else(|_| format!("{{{{ck-secret:{name}}}}}").into_bytes())
}

pub(crate) const REDACTION_WORK: usize = 8 * 1024 * 1024;
#[derive(Debug)]
pub(crate) struct RedactionLimit;

fn charge(remaining: &mut usize, work: usize) -> Result<(), RedactionLimit> {
    if work > *remaining {
        *remaining = 0;
        return Err(RedactionLimit);
    }
    *remaining -= work;
    Ok(())
}

fn prefix_table(pattern: &[u8]) -> Vec<usize> {
    let mut prefix = vec![0; pattern.len()];
    for i in 1..pattern.len() {
        let mut j = prefix[i - 1];
        while j > 0 && pattern[i] != pattern[j] {
            j = prefix[j - 1];
        }
        if pattern[i] == pattern[j] {
            j += 1;
        }
        prefix[i] = j;
    }
    prefix
}

fn exact_matches(input: &[u8], key: &[u8], prefix: &[usize], mut found: impl FnMut(usize)) {
    let mut j = 0;
    for (i, &b) in input.iter().enumerate() {
        while j > 0 && b != key[j] {
            j = prefix[j - 1];
        }
        if b == key[j] {
            j += 1;
        }
        if j == key.len() {
            found(i + 1 - j);
            j = prefix[j - 1];
        }
    }
}

pub(crate) fn redact_with_budget(
    input: &[u8],
    key: &[u8],
    name: &str,
    remaining: &mut usize,
) -> Result<Vec<u8>, RedactionLimit> {
    if key.is_empty() || input.len() < key.len() {
        return Ok(input.to_vec());
    }
    charge(
        remaining,
        input.len().saturating_add(key.len().saturating_mul(6)),
    )?;
    let placeholder = format!("{{{{ck-secret:{name}}}}}");
    let mut patterns = vec![key.to_vec()];
    for engine in [
        &base64::engine::general_purpose::STANDARD,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE,
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ] {
        patterns.push(engine.encode(key).into_bytes());
    }
    patterns.sort_by_key(|p| std::cmp::Reverse(p.len()));
    patterns.dedup();
    let mut literals = vec![0; input.len()];
    for pattern in &patterns {
        charge(
            remaining,
            input.len().saturating_mul(2).saturating_add(pattern.len()),
        )?;
        exact_matches(input, pattern, &prefix_table(pattern), |start| {
            literals[start] = literals[start].max(pattern.len())
        });
    }
    let key_prefix = prefix_table(key);
    let mut percents = input
        .iter()
        .enumerate()
        .filter_map(|(i, b)| (*b == b'%').then_some(i))
        .peekable();
    let mut output = Vec::new();
    let mut offset = 0;
    while offset < input.len() {
        charge(remaining, 1)?;
        let tail = &input[offset..];
        let literal = (literals[offset] > 0).then_some(literals[offset]);
        let alphabet = |b: u8| b.is_ascii_alphanumeric() || b"+/-_".contains(&b);
        let mut encoded = None;
        if offset == 0 || !alphabet(input[offset - 1]) {
            let mut end = tail.iter().take_while(|b| alphabet(**b)).count();
            for _ in 0..2 {
                if tail.get(end) == Some(&b'=') {
                    end += 1;
                }
            }
            if end >= 4 {
                for engine in [
                    &base64::engine::general_purpose::STANDARD,
                    &base64::engine::general_purpose::STANDARD_NO_PAD,
                    &base64::engine::general_purpose::URL_SAFE,
                    &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                ] {
                    charge(remaining, end.saturating_mul(2))?;
                    if let Ok(mut decoded) = engine.decode(&tail[..end]) {
                        charge(remaining, decoded.len().saturating_mul(2))?;
                        let mut contains_key = false;
                        exact_matches(&decoded, key, &key_prefix, |_| contains_key = true);
                        decoded.fill(0);
                        if contains_key {
                            encoded = Some(end);
                            break;
                        }
                    }
                }
            }
        }
        while percents.peek().is_some_and(|p| *p < offset) {
            percents.next();
        }
        let mut positions = Vec::new();
        if tail.len() >= key.len() && percents.peek().is_some_and(|p| *p < offset + key.len()) {
            positions.push(0);
            for &b in key {
                let mut next = Vec::new();
                for position in positions {
                    charge(remaining, 1)?;
                    if tail.get(position) == Some(&b) {
                        next.push(position + 1);
                    }
                    if tail.get(position) == Some(&b'%') {
                        let hi = tail
                            .get(position + 1)
                            .and_then(|b| (*b as char).to_digit(16));
                        let lo = tail
                            .get(position + 2)
                            .and_then(|b| (*b as char).to_digit(16));
                        if hi
                            .zip(lo)
                            .is_some_and(|(hi, lo)| hi * 16 + lo == u32::from(b))
                        {
                            next.push(position + 3);
                        }
                    }
                }
                if next.len() > 32768 {
                    return Err(RedactionLimit);
                }
                charge(
                    remaining,
                    next.len()
                        .saturating_mul((usize::BITS - next.len().leading_zeros()) as usize),
                )?;
                next.sort_unstable();
                next.dedup();
                positions = next;
                if positions.is_empty() {
                    break;
                }
            }
        }
        if let Some(length) = encoded.or(literal).or(positions.into_iter().max()) {
            charge(remaining, placeholder.len())?;
            output.extend_from_slice(placeholder.as_bytes());
            offset += length;
        } else {
            output.push(input[offset]);
            offset += 1;
        }
    }
    for pattern in &mut patterns {
        pattern.fill(0);
    }
    Ok(output)
}

pub(crate) fn scope_refusal(code: &str) -> crate::transport::WireError {
    crate::transport::WireError::Refused {
        code: code.into(),
        message: "the daemon did not attest this installed flow identity".into(),
    }
}

pub(crate) fn not_ready() -> crate::transport::WireError {
    crate::transport::WireError::Typed(crate::flow_refusal::FlowRefusal::new(
        crate::flow_refusal::RefusalReason::NoFlowScope,
        "claustrum",
        "secret.use",
    ))
}

pub(crate) fn use_attested(
    host: &dyn crate::Host,
    transport: &dyn crate::transport::Transport,
    scope: &crate::flow_scope::FlowScope,
    request: &crate::CallRequest,
    code_hash: &str,
    mut params: serde_json::Value,
) -> Result<serde_json::Value, crate::transport::WireError> {
    let description = host
        .scope_describe(&scope.owner, &scope.scope_ref)
        .map_err(|_| not_ready())?;
    let stamp = attested_stamp(scope, request, code_hash, description)?;
    params["stamp"] =
        serde_json::to_value(stamp).map_err(|_| scope_refusal("secret_scope_mismatch"))?;
    transport.management("claustrum", "secret.use", params)
}

pub(crate) fn attested_stamp(
    scope: &crate::flow_scope::FlowScope,
    request: &crate::CallRequest,
    code_hash: &str,
    description: crate::ScopeDescription,
) -> Result<crate::ScopeStamp, crate::transport::WireError> {
    if description.status != crate::ScopeStatus::Live
        || description.scope_epoch != Some(scope.epoch)
    {
        return Err(not_ready());
    }
    let stamp = description.scope.ok_or_else(not_ready)?;
    if stamp.scope_epoch != scope.epoch
        || !description.owner_synced
        || !description.owner_configured
    {
        return Err(not_ready());
    }
    let attributes = &stamp.attributes;
    if attributes.flow_id.as_ref().is_none_or(|s| s.is_empty())
        || attributes.flow_version.is_none()
        || attributes
            .flow_code_hash
            .as_ref()
            .is_none_or(|s| s.is_empty())
        || attributes
            .flow_install_id
            .as_ref()
            .is_none_or(|s| s.is_empty())
    {
        return Err(scope_refusal("secret_scope_missing"));
    }
    if stamp.owner != scope.owner
        || stamp.scope_ref != scope.scope_ref
        || !stamp.owner_authorized
        || attributes.flow_id.as_deref() != Some(request.flow_id.as_str())
        || attributes.flow_version != request.flow_version.map(u64::from)
        || attributes.flow_code_hash.as_deref() != Some(code_hash)
    {
        return Err(scope_refusal("secret_scope_mismatch"));
    }
    Ok(stamp)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub name: String,
    pub target: Target,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "decode_methods"
    )]
    pub methods: Option<Vec<String>>,
}

fn decode_methods<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    Option::<Vec<String>>::deserialize(deserializer).map(|value| Some(value.unwrap_or_default()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Target {
    Api(ApiTarget),
    Login(LoginTarget),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiTarget {
    pub scheme: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginTarget {
    pub origin: String,
}

pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn valid_method(method: &str) -> bool {
    (1..=16).contains(&method.len()) && method.bytes().all(|b| b.is_ascii_uppercase())
}

/// Reject ambiguous paths instead of letting the server and vault interpret
/// escapes, repeated separators or dot segments differently.
pub fn valid_path(path: &str) -> bool {
    if !path.starts_with('/') || path.len() > 1024 || path.contains("//") {
        return false;
    }
    for segment in path.split('/') {
        if matches!(segment.split(';').next(), Some("." | "..")) {
            return false;
        }
    }
    let mut bytes = path.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let Some(hi) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                return false;
            };
            let Some(lo) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                return false;
            };
            let decoded = (hi * 16 + lo) as u8;
            if !(0x21..0x7f).contains(&decoded) || b"/.%\\;".contains(&decoded) {
                return false;
            }
        } else if !b.is_ascii_alphanumeric() && !b"-._~!$&'()*+,;=:@/".contains(&b) {
            return false;
        }
    }
    true
}

impl Secret {
    pub fn valid(&self) -> bool {
        if !valid_name(&self.name) {
            return false;
        }
        match &self.target {
            Target::Api(target) => {
                target.scheme == "https"
                    && target.port.unwrap_or(443) == 443
                    && target.host == target.host.to_ascii_lowercase()
                    && crate::builtins::net::host_problem(&target.host).is_none()
                    && valid_path(&target.path)
                    && self.methods.as_ref().is_some_and(|methods| {
                        (1..=16).contains(&methods.len())
                            && methods.iter().all(|m| valid_method(m))
                            && methods
                                .iter()
                                .collect::<std::collections::BTreeSet<_>>()
                                .len()
                                == methods.len()
                    })
            }
            Target::Login(target) => {
                let Some(authority) = target.origin.strip_prefix("https://") else {
                    return false;
                };
                let host = authority.strip_suffix(":443").unwrap_or(authority);
                self.methods.is_none()
                    && host == host.to_ascii_lowercase()
                    && crate::builtins::net::host_problem(host).is_none()
            }
        }
    }

    pub fn permits(&self, url: &crate::builtins::net::Url, method: &str) -> bool {
        let Target::Api(target) = &self.target else {
            return false;
        };
        let path = url.target.split('?').next().unwrap_or("");
        let prefix = target.path.as_str();
        target.scheme == "https"
            && url.host == target.host
            && url.port == target.port.unwrap_or(443)
            && valid_path(path)
            && (path == prefix
                || if prefix.ends_with('/') {
                    path.starts_with(prefix)
                } else {
                    path.strip_prefix(prefix)
                        .is_some_and(|p| p.starts_with('/'))
                })
            && self
                .methods
                .as_ref()
                .is_some_and(|m| m.iter().any(|m| m == method))
    }
}

#[cfg(test)]
#[path = "secrets_tests.rs"]
mod tests;
