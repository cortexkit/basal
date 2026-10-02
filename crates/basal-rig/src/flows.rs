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

/// Every minute, so a flow approved now runs within a minute.
const EVERY_MINUTE: &str = "* * * * *";

fn manifest(id: &str, purpose: &str, agent: &str, facts: bool) -> String {
    let mut m = json!({
        "format": 1,
        "id": id,
        "version": 1,
        "purpose": purpose,
        "trigger": { "schedule": { "cron": EVERY_MINUTE } },
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
