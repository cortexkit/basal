//! The install card's fields, built from the version as installed, the
//! catalog, the dry run and the flow's token window. The card shows everything
//! the manifest grants and the code itself, because approval binds to the
//! code hash: what the operator approves is exactly what runs.

use basal_core::manifest::Manifest;
use basal_core::{Installed, Warning};
use basal_host::Catalog;
use serde_json::{Value, json};

use basal_proto::util::hex;

pub fn code_hash_hex(hash: &[u8; 32]) -> String {
    hex(hash)
}

pub(crate) fn warning(w: &Warning) -> Value {
    match w {
        Warning::ProtectedRoot {
            field,
            path,
            protections,
        } => json!({
            "kind": "protected_root",
            "field": field,
            "path": path,
            "protections": protections.iter().map(|p| json!({
                "os": p.os.as_str(),
                "folder": p.folder,
                "effect": p.effect(),
            })).collect::<Vec<_>>(),
            // One sentence per OS, one per line.
            "text": protections.iter().map(|p| p.text()).collect::<Vec<_>>().join("\n"),
        }),
        Warning::AgentlessAudienceOp {
            module,
            op,
            audience,
        } => json!({
            "kind": "agentless_audience_op",
            "module": module,
            "op": op,
            "audience": audience,
            "text": format!("runs without an agent identity ({audience} audience), so writes through an agent's bot account will be refused; use for: with a delegate grant to act for an agent"),
        }),
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
        Warning::LoopExemptNoEvents { module, op } => json!({
            "kind": "loop_exempt_no_events",
            "module": module,
            "op": op,
            "text": "the module declares that no call to this tool makes it emit an event, so the loop install rule does not apply; loop protection for this op rests on that declaration",
        }),
        Warning::PrivateTextWithNetFetch { host } => json!({
            "kind": "private_text_with_net_fetch",
            "host": host,
            "text": "the flow reads private text and can fetch from this host, which can carry that text out",
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
                let mut fields = json!({ "module": e.module, "name": e.name, "version": e.version, "origin": origin });
                if let Some(max_age) = &e.max_age {
                    fields["max_age"] = json!(max_age);
                }
                fields
            }).collect::<Vec<_>>(),
        }),
        (None, None) => Value::Null,
    };
    let ops: Vec<Value> = m
        .ops
        .iter()
        .map(|o| {
            let decl = input.catalog.op(&o.module, &o.op);
            // Install reads a tool's emits_events only for an op the loop
            // rule would otherwise refuse, and records the result as a
            // warning. So this is `false` for an op that passed the rule by
            // that declaration, and null (not read) for every other op.
            let emits_events = input
                .installed
                .warnings
                .iter()
                .any(|w| {
                    matches!(w, Warning::LoopExemptNoEvents { module, op }
                        if *module == o.module && *op == o.op)
                })
                .then_some(false);
            json!({
                "module": o.module,
                "op": o.op,
                "kind": decl.as_ref().and_then(|d| d.kind).map(|k| k.as_str()),
                "cause_echo": decl.as_ref().map(|d| d.cause_echo),
                "emits_events": emits_events,
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
        "sinks": m.sinks.iter().map(|s| match s {
            basal_core::manifest::SinkGrant::Wait(_) => json!({"kind":"wait"}),
            basal_core::manifest::SinkGrant::Digest(s) => json!({
            "agent": s.agent,
            "digest_max": s.digest_max,
            "break_through_requested": s.break_through,
        })}).collect::<Vec<_>>(),
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
        // The built-in lines, as approved: roots as written (`~` is the
        // user's home) and each host with the methods it is approved for.
        "fs": m.fs.as_ref().map(|f| json!({ "read": f.read, "write": f.write })),
        "git": m.git.as_ref().map(|g| json!({ "read": g.read })),
        "net": m.net.as_ref().map(|n| json!({
            "fetch": n.fetch.iter().map(|f| json!({
                "host": f.host,
                "methods": f.effective_methods(),
            })).collect::<Vec<_>>(),
        })),
        "placement": m.placement,
        "warnings": input.installed.warnings.iter().map(warning).collect::<Vec<_>>(),
        "code_hash": code_hash_hex(&input.installed.code_hash),
        // The exact bytes the hash covers; the card shows them collapsed.
        "code": { "script": input.script, "manifest": input.manifest_text },
        // The raw summary stays for audits; the plain-language text is what
        // core and cingulate show the operator, when core accepts it.
        "dry_run_text": serde_json::to_value(basal_core::dry_run_text::render(m, &input.dry_run))
            .unwrap_or(Value::Null),
        "dry_run_summary": input.dry_run,
    })
}

#[cfg(test)]
mod warning_tests {
    use super::*;

    #[test]
    fn protected_root_warning_keeps_its_manifest_source_and_one_line_per_os() {
        use basal_core::{ProtectingOs, Protection};

        assert_eq!(
            warning(&Warning::ProtectedRoot {
                field: "fs.read".into(),
                path: "~/Documents/project".into(),
                protections: vec![
                    Protection {
                        os: ProtectingOs::Macos,
                        folder: "~/Documents".into(),
                    },
                    Protection {
                        os: ProtectingOs::Windows,
                        folder: "~/Documents".into(),
                    },
                ],
            }),
            json!({
                "kind": "protected_root",
                "field": "fs.read",
                "path": "~/Documents/project",
                "protections": [
                    {
                        "os": "macos",
                        "folder": "~/Documents",
                        "effect": "the first access asks the operator for permission",
                    },
                    {
                        "os": "windows",
                        "folder": "~/Documents",
                        "effect": "writes are blocked while Controlled Folder Access is on",
                    },
                ],
                "text": "On macOS, the first access to ~/Documents asks the operator for permission.\n\
                         On Windows, writes under ~/Documents are blocked while Controlled Folder Access is on.",
            }),
        );
    }
}
