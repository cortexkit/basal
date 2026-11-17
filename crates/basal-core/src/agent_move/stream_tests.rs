use serde_json::json;

use super::*;
use crate::agent_move::tests::{AGENT, migrated, populate};

fn sample() -> Vec<u8> {
    let conn = migrated();
    populate(&conn);
    crate::agent_move::encode(&conn, AGENT, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .unwrap()
        .unwrap()
}

/// Rewrites one record of a stream through `edit`, keeping the framing.
fn edited(bytes: &[u8], index: usize, edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut edit = Some(edit);
    let mut out = Vec::new();
    for (i, body) in frames(bytes).unwrap().into_iter().enumerate() {
        let mut value: Value = serde_json::from_slice(body).unwrap();
        let body = if i == index {
            (edit.take().unwrap())(&mut value);
            canonical(&value)
        } else {
            body.to_vec()
        };
        out.extend((body.len() as u32).to_be_bytes());
        out.extend(body);
    }
    out
}

#[test]
fn the_header_comes_first_and_is_canonical() {
    let bytes = sample();
    let header = header(&bytes).unwrap();
    assert_eq!(header["format"], FORMAT);
    assert_eq!(header["agent"], AGENT);
    assert_eq!(
        header["counts"],
        json!({"flow":3,"generation":1,"history":1,"instance":1,"kv":1,"queued_event":1,"snapshot":1})
    );
    let first = frames(&bytes).unwrap()[0];
    assert_eq!(first, canonical(&serde_json::from_slice(first).unwrap()).as_slice());
    // Encoding the same rows again gives the same bytes.
    assert_eq!(bytes, sample());
}

#[test]
fn an_unknown_format_is_incompatible_before_anything_else_is_read() {
    let bytes = edited(&sample(), 0, |h| h["format"] = json!("basal-flows/2"));
    assert_eq!(
        decode(&bytes),
        Err(DecodeError::Incompatible {
            found: "basal-flows/2".into()
        })
    );
    // Even a stream that is damaged past its header is judged by its header.
    let mut cut_short = bytes.clone();
    cut_short.truncate(cut_short.len() - 3);
    assert!(matches!(decode(&cut_short), Err(DecodeError::Incompatible { .. })));
}

#[test]
fn changed_version_bytes_no_longer_match_their_code_hash() {
    let bytes = edited(&sample(), 1, |flow| {
        flow["versions"][0]["script"] = json!("// another script\n");
    });
    assert!(matches!(decode(&bytes), Err(DecodeError::Damaged(m)) if m.contains("code hash")));
}

#[test]
fn a_member_this_build_does_not_know_is_refused() {
    let bytes = edited(&sample(), 1, |flow| flow["future_column"] = json!(1));
    assert!(matches!(decode(&bytes), Err(DecodeError::Damaged(m)) if m.contains("future_column")));
    let bytes = edited(&sample(), 1, |flow| {
        flow.as_object_mut().unwrap().remove("owner");
    });
    assert!(matches!(decode(&bytes), Err(DecodeError::Damaged(m)) if m.contains("owner")));
}

#[test]
fn a_truncated_stream_or_wrong_counts_are_damaged() {
    let bytes = sample();
    assert!(matches!(decode(&bytes[..bytes.len() - 1]), Err(DecodeError::Damaged(_))));
    let bytes = edited(&sample(), 0, |h| h["counts"]["kv"] = json!(2));
    assert!(matches!(decode(&bytes), Err(DecodeError::Damaged(m)) if m.contains("kv")));
}

#[test]
fn a_stream_past_its_cap_is_refused() {
    let mut writer = Writer::new();
    let big = "x".repeat(1024 * 1024);
    let mut refused = None;
    for _ in 0..70 {
        if let Err(message) = writer.frame(&json!({"kind":"kv","value":big})) {
            refused = Some(message);
            break;
        }
    }
    assert!(refused.unwrap().contains("64 MiB"));
    assert!(writer.bytes.len() as u64 <= STREAM_CAP);
}
