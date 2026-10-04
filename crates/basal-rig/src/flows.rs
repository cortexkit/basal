//! The flows the contract suite installs. Each is a manifest (sent as these
//! exact bytes, which core renders the card from and both sides hash) and a
//! script whose return value records what every call answered, so the suite
//! reads the replies from the run's result in basal's store.

use serde_json::{Value, json};

/// A flow version to install: its id, the exact manifest bytes, the script.
#[derive(Debug, Clone)]
pub struct Flow {
    pub id: String,
    pub version: i64,
    pub manifest: String,
    pub script: String,
}

impl Flow {
    pub fn manifest_value(&self) -> Value {
        serde_json::from_str(&self.manifest).expect("the suite writes valid manifests")
    }
}

/// The cron pattern every suite flow is scheduled on: every minute, so a
/// flow approved now runs within a minute.
const EVERY_MINUTE: &str = "* * * * *";

/// Once a year, at midnight UTC on 1 January: a flow scheduled on it is
/// approved and enabled like any other but does not run while the suite does,
/// unless the suite happens to run across New Year.
const NEW_YEAR: &str = "0 0 1 1 *";

fn manifest(id: &str, purpose: &str, agent: &str, facts: bool) -> String {
    manifest_on(id, purpose, agent, facts, EVERY_MINUTE)
}

fn manifest_on(id: &str, purpose: &str, agent: &str, facts: bool, cron: &str) -> String {
    let mut m = json!({
        "format": 1,
        "id": id,
        "version": 1,
        "purpose": purpose,
        "trigger": { "schedule": { "cron": cron } },
        "sinks": [ { "agent": agent, "digest_max": "piggyback" } ],
        "status": [ agent ],
    });
    if facts {
        m["facts"] = json!({ "targets": [agent], "text": false });
    }
    serde_json::to_string(&m).expect("a JSON value always serialises")
}

/// A JS string literal for `text`.
fn js(text: &str) -> String {
    serde_json::to_string(text).expect("a string always serialises")
}

/// Catches a refused call and records what the script saw.
const CAUGHT: &str =
    "(e) => ({ refused: { name: e && e.name, message: e && e.message, data: e && e.data } })";

/// The sinks and facts flow: reads the granted facts, tries private text the
/// grant does not cover and a target it does not name, then writes one digest
/// item and one status line for `agent`.
pub fn sinks(id: &str, agent: &str, ungranted_target: &str) -> Flow {
    let script = format!(
        "const caught = {CAUGHT};
const out = {{}};
out.facts = await facts({a}, {{ fields: ['identity', 'residence', 'activity'], include: [], max_age_ms: null }});
out.text = await facts({a}, {{ fields: ['activity'], include: ['text'], max_age_ms: null }}).catch(caught);
out.other = await facts({o}, {{ fields: ['identity'], include: [], max_age_ms: null }}).catch(caught);
out.digest = await sink.digest({a}, {{ title: 'basal rig contract', body: 'sinks case', data: {{ flow: {f} }}, links: [] }}, 'piggyback');
out.status = await sink.status({a}, 'basal rig contract: sinks case');
return out;",
        a = js(agent),
        o = js(ungranted_target),
        f = js(id),
    );
    Flow {
        id: id.into(),
        version: 1,
        manifest: manifest(
            id,
            "Basal rig contract: read granted facts, write one digest item and one status line.",
            agent,
            true,
        ),
        script,
    }
}

/// A flow that writes one digest item and one status line and returns both
/// replies (or the refusals).
pub fn writer(id: &str, agent: &str, purpose: &str) -> Flow {
    let script = format!(
        "const caught = {CAUGHT};
const out = {{}};
out.digest = await sink.digest({a}, {{ title: 'basal rig contract', body: {p}, data: {{ flow: {f} }}, links: [] }}, 'piggyback').catch(caught);
out.status = await sink.status({a}, 'basal rig contract').catch(caught);
return out;",
        a = js(agent),
        p = js(purpose),
        f = js(id),
    );
    Flow {
        id: id.into(),
        version: 1,
        manifest: manifest(id, purpose, agent, false),
        script,
    }
}

/// A writer flow scheduled once a year, for the cases that only manage a
/// flow (dry run, disable, enable, list) and must not see it run.
pub fn quiet(id: &str, agent: &str, purpose: &str) -> Flow {
    Flow {
        manifest: manifest_on(id, purpose, agent, false, NEW_YEAR),
        ..writer(id, agent, purpose)
    }
}

/// The crash flow: one digest item, the run's only remote call, so it is
/// journaled at position 0 (the journal counts positions from 0).
pub fn crash(id: &str, agent: &str) -> Flow {
    let script = format!(
        "const digest = await sink.digest({a}, {{ title: 'basal rig contract', body: 'crash case', data: {{ flow: {f} }}, links: [] }}, 'piggyback');
return {{ digest }};",
        a = js(agent),
        f = js(id),
    );
    Flow {
        id: id.into(),
        version: 1,
        manifest: manifest(
            id,
            "Basal rig contract: one digest item, written across a crash of ck-basal.",
            agent,
            false,
        ),
        script,
    }
}

/// Model cases are agent-owned at installation, just like the scoped writer.
/// Each daily cap bounds every scheduled run, not just the first run the suite
/// observes. The three sending flows total 3,072 tokens; the refused one adds
/// 16, for a finite 3,088-token allowance even if cleanup is interrupted.
pub fn model(id: &str, agent: &str, classify: bool, capped: bool) -> Flow {
    let mut m: Value = serde_json::from_str(&manifest(
        id,
        "Basal rig contract: a tiny, agent-owned model call through Broca.",
        agent,
        false,
    ))
    .expect("valid manifest");
    m["llm"] = json!({
        // Routing refuses all-zero requirements without a required capability.
        "iq": 1, "eq": 0,
        "token_cap": {"tokens": if capped {16} else {1024}, "window": "1d"},
        "max_output": if capped {16} else {64},
    });
    let request = if classify {
        "classify('A friendly hello.', ['positive', 'negative'])"
    } else {
        "llm({prompt:'Reply with the word hello.', max_output:16})"
    };
    Flow {
        id: id.into(),
        version: 1,
        manifest: m.to_string(),
        script: format!("return await {request}.catch({CAUGHT});"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_manifests_bound_the_suite_and_keep_prompts_tiny() {
        let mut total = 0;
        for (id, classify, capped) in [
            ("first", false, false),
            ("classify", true, false),
            ("crash", false, false),
            ("cap", false, true),
        ] {
            let f = model(id, "RigAgent", classify, capped);
            let m = f.manifest_value();
            let tokens = m["llm"]["token_cap"]["tokens"].as_u64().unwrap();
            total += tokens;
            assert_eq!(m["llm"]["token_cap"]["window"], "1d");
            assert_eq!(m["llm"]["iq"], 1);
            assert!((16..=64).contains(&m["llm"]["max_output"].as_u64().unwrap()));
            assert!(f.script.len() < 256);
            assert!(!f.script.contains("tools"));
            assert_eq!(f.script.matches("await").count(), 1);
            if capped {
                assert_eq!(tokens, 16);
            }
        }
        assert_eq!(total, 3088);
    }
}
