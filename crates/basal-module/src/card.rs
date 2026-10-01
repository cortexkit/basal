//! The install card's fields (`docs/design.md` section 6), built from the version as
//! installed, the catalog, the dry run and the flow's token window.

use basal_core::manifest::Manifest;
use basal_core::{Installed, Warning};
use basal_host::Catalog;
use serde_json::{Value, json};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn code_hash_hex(hash: &[u8; 32]) -> String {
    hex(hash)
}

fn warning(w: &Warning) -> Value {
    match w {
        Warning::PrivateTextWithOutboundOp { module, op } => json!({
            "kind": "private_text_with_outbound_op",
            "module": module,
            "op": op,
            "text": "the flow reads private text and holds an op that can send data out",
        }),
        Warning::LoopOverride { module, op } => json!({
            "kind": "loop_override",
            "module": module,
            "op": op,
            "text": "the operator overrode the loop install rule; loop protection for this flow is rate limiting only",
        }),
    }
}

/// Everything the card shows.
pub struct CardInput<'a> {
    pub manifest: &'a Manifest,
    pub script: &'a str,
    pub manifest_text: &'a str,
    pub author: &'a str,
    pub installed: &'a Installed,
    pub catalog: &'a dyn Catalog,
    /// The flow's current token window, split by field, when it has one.
    pub token_window: Option<Value>,
    pub dry_run: Value,
}

pub fn fields(input: &CardInput<'_>) -> Value {
    let m = input.manifest;
    let trigger = match (&m.trigger.schedule, &m.trigger.events) {
        (Some(schedule), _) => json!({ "schedule": schedule }),
        (None, Some(events)) => json!({
            "events": events.iter().map(|e| {
                let origin = input
                    .catalog
                    .event(&e.module, &e.name, e.version)
                    .map(|d| match d.origin {
                        basal_host::EventOrigin::External => "external",
                        basal_host::EventOrigin::Internal => "internal",
                    });
                json!({ "module": e.module, "name": e.name, "version": e.version, "origin": origin })
            }).collect::<Vec<_>>(),
        }),
        (None, None) => Value::Null,
    };
    let ops: Vec<Value> = m
        .ops
        .iter()
        .map(|o| {
            let decl = input.catalog.op(&o.module, &o.op);
            json!({
                "module": o.module,
                "op": o.op,
                "kind": decl.as_ref().and_then(|d| d.kind).map(|k| k.as_str()),
                "cause_echo": decl.as_ref().map(|d| d.cause_echo),
            })
        })
        .collect();
    json!({
        "kind": "flow_install",
        "flow_id": m.id,
        "version": m.version,
        "purpose": m.purpose,
        "author": input.author,
        "trigger": trigger,
        "sinks": m.sinks.iter().map(|s| json!({
            "agent": s.agent,
            "digest_max": s.digest_max,
            "break_through_requested": s.break_through,
        })).collect::<Vec<_>>(),
        "status_targets": m.status,
        "claims": m.claims.iter().map(|c| json!({ "agent": c.agent, "source_kind": c.source_kind })).collect::<Vec<_>>(),
        "ops": ops,
        "facts": m.facts.as_ref().map(|f| json!({ "targets": f.targets, "private_text": f.text })),
        "token_cap": m.llm.as_ref().map(|l| json!({
            "tokens": l.token_cap.tokens,
            "window": l.token_cap.window,
            "max_output": l.max_output,
        })),
        "token_window": input.token_window,
        "placement": m.placement,
        "warnings": input.installed.warnings.iter().map(warning).collect::<Vec<_>>(),
        "code_hash": code_hash_hex(&input.installed.code_hash),
        // The exact bytes the hash covers; the card shows them collapsed.
        "code": { "script": input.script, "manifest": input.manifest_text },
        "dry_run_summary": input.dry_run,
    })
}
