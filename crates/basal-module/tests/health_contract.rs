//! `flow.health` as prefrontal-core reads it. Core polls the op on its own
//! route to decide whether a flow's claim on a source still holds, and
//! treats any reply it cannot decode as unhealthy. So the reply is decoded
//! here exactly as core decodes it: into a struct with only the contract's
//! fields, every one required (a field that may be null must still be
//! present), with the contract's types. Fields core does not know are
//! ignored, as core ignores them.

mod common;

use basal_core::Actor;
use basal_host::mock::Fault;
use basal_module::caller::Caller;
use common::{
    Fixture, Options, T0, admit, agent, events_manifest, fixture, install, install_approved,
};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

/// An RFC 3339 time in UTC, as core parses it.
#[derive(Debug)]
struct Utc(jiff::Timestamp);

impl<'de> Deserialize<'de> for Utc {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        if !text.ends_with('Z') {
            return Err(serde::de::Error::custom(format!("{text} is not UTC")));
        }
        text.parse()
            .map(Utc)
            .map_err(|e| serde::de::Error::custom(format!("{text}: {e}")))
    }
}

#[derive(Debug, Deserialize)]
struct Reply {
    as_of: Utc,
    flows: Vec<Entry>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum State {
    Enabled,
    Disabled,
    /// No version approved yet: the card is pending or was declined.
    Unapproved,
    Shadow,
}

#[derive(Debug, Deserialize)]
struct LastRun {
    outcome: String,
    at: Utc,
}

#[derive(Debug, Deserialize)]
struct Entry {
    flow_id: String,
    state: State,
    /// Null when the flow never finished a run, but never absent.
    #[serde(deserialize_with = "present")]
    last_run: Option<LastRun>,
    oldest_overdue_age_ms: u64,
    consecutive_failures: u64,
    needs_reconcile: bool,
    overflowed: bool,
    auto_disabled: bool,
}

/// A nullable field that must be present: with a custom deserializer serde
/// reports a missing field instead of defaulting it to `None`.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<LastRun>, D::Error> {
    Option::<LastRun>::deserialize(d)
}

fn health(f: &Fixture, caller: &Caller, params: Value) -> Value {
    f.module
        .handle(caller, "flow.health", params)
        .expect("flow.health")
}

fn decode(raw: &Value) -> Reply {
    serde_json::from_value(raw.clone())
        .unwrap_or_else(|e| panic!("core cannot decode: {e}\n{raw:#}"))
}

fn entry<'a>(reply: &'a Reply, flow: &str) -> &'a Entry {
    reply
        .flows
        .iter()
        .find(|e| e.flow_id == flow)
        .unwrap_or_else(|| panic!("{flow} is listed"))
}

#[test]
fn flow_health_decodes_as_core_decodes_it() {
    let f = fixture("health-contract", Options::with_worker());
    let synapse = agent("SYNAPSE");
    let ok = install_approved(&f, &synapse, "return 1;", &events_manifest("flow-ok"));
    let failing = install_approved(
        &f,
        &synapse,
        "throw new Error('broken');",
        &events_manifest("flow-failing"),
    );
    let mut post_manifest = events_manifest("flow-post");
    post_manifest["ops"] = json!([{ "module": "mock", "op": "post" }]);
    let post = install_approved(
        &f,
        &synapse,
        "return await ops.call('mock', 'post', { n: 1 });",
        &post_manifest,
    );
    let auto = install_approved(&f, &synapse, "return 1;", &events_manifest("flow-auto"));
    install_approved(&f, &synapse, "return 1;", &events_manifest("flow-quiet"));
    let waiting = install_approved(&f, &synapse, "return 1;", &events_manifest("flow-waiting"));
    // Installed, its card still open: nothing approved.
    install(&f, &synapse, "return 1;", &events_manifest("flow-pending"));

    admit(&f, &ok, "ok-1");
    admit(&f, &failing, "f-1");
    admit(&f, &failing, "f-2");
    // An unkeyed mutation whose reply is lost: its run waits for the
    // operator.
    f.mock.inject(
        "mock",
        "post",
        &[Fault {
            proven_unsent: false,
            effect_applied: true,
        }],
    );
    admit(&f, &post, "p-1");
    f.module.engine.run_until_idle(50).expect("idle");
    f.module
        .rt
        .disable_flow(&auto, &Actor::Runtime, "saturated")
        .expect("auto-disable");
    // Admitted and not yet started: overdue work.
    admit(&f, &waiting, "w-1");
    f.clock.set(T0 + 5_000);

    for params in [Value::Null, json!({})] {
        let raw = health(&f, &Caller::Core, params);
        assert_eq!(
            raw["worker_confinement"],
            json!({
                "os": std::env::consts::OS,
                "landlock": if cfg!(target_os = "linux") { json!("required") } else { Value::Null },
                "sigsys_deaths": 0,
            })
        );
        let reply = decode(&raw);
        assert_eq!(reply.as_of.0.as_millisecond(), T0 + 5_000);
        assert_eq!(reply.flows.len(), 7);

        let e = entry(&reply, "flow-ok");
        assert_eq!(e.state, State::Enabled);
        let last = e.last_run.as_ref().expect("a finished run");
        assert_eq!(last.outcome, "succeeded");
        assert!(last.at.0.as_millisecond() > 0);
        assert_eq!(e.oldest_overdue_age_ms, 0, "nothing overdue reads 0");
        assert_eq!(e.consecutive_failures, 0);
        assert!(!e.needs_reconcile && !e.overflowed && !e.auto_disabled);

        let e = entry(&reply, "flow-failing");
        assert_eq!(e.consecutive_failures, 2);
        assert_eq!(
            e.last_run.as_ref().map(|r| r.outcome.as_str()),
            Some("failed")
        );

        let e = entry(&reply, "flow-post");
        assert!(e.needs_reconcile);
        assert!(
            e.last_run.is_none(),
            "a run in needs_reconcile has not finished"
        );

        let e = entry(&reply, "flow-auto");
        assert_eq!(e.state, State::Disabled);
        assert!(e.auto_disabled);

        let e = entry(&reply, "flow-quiet");
        assert!(e.last_run.is_none());
        assert_eq!(e.oldest_overdue_age_ms, 0);
        assert_eq!(e.state, State::Enabled);

        let e = entry(&reply, "flow-waiting");
        assert_eq!(e.oldest_overdue_age_ms, 5_000);

        let e = entry(&reply, "flow-pending");
        assert_eq!(e.state, State::Unapproved, "no version is approved");

        // Every contract field is present on every entry, nullable ones
        // included: removing one makes the reply undecodable.
        let mut missing = raw.clone();
        if let Some(first) = missing["flows"].get_mut(0).and_then(Value::as_object_mut) {
            first.remove("last_run");
        }
        assert!(serde_json::from_value::<Reply>(missing).is_err());
    }

    // Asking for some flows reports those, leaving out ones basal does not
    // know.
    let raw = health(
        &f,
        &Caller::Core,
        json!({ "flow_ids": ["flow-ok", "flow-auto", "no-such-flow"] }),
    );
    let reply = decode(&raw);
    let mut ids: Vec<&str> = reply.flows.iter().map(|e| e.flow_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["flow-auto", "flow-ok"]);

    // The operator's call with no parameters keeps working and carries the
    // operator's extra figures beside the contract.
    let raw = health(&f, &Caller::Operator, Value::Null);
    decode(&raw);
    assert!(raw["runs"]["oldest_pending_ms"].is_number());
    assert!(raw["metrics"]["workers"].is_object());
    let auto_entry = raw["flows"]
        .as_array()
        .and_then(|a| a.iter().find(|e| e["flow_id"] == "flow-auto"))
        .expect("listed");
    assert_eq!(auto_entry["disabled"]["by"], "auto");
    // Unknown parameters are refused rather than ignored.
    let refused = f.module.handle(
        &Caller::Core,
        "flow.health",
        json!({ "flows": ["flow-ok"] }),
    );
    assert!(matches!(refused, Err(e) if e.code == "invalid_params"));
}

#[test]
fn health_and_list_show_grant_loss_and_retirement_evidence() {
    let f = fixture("grant-health", Options::default());
    let owner = agent("SYNAPSE");
    let flow = install_approved(&f, &owner, "return 1;", &events_manifest("lost-grant"));
    f.module
        .rt
        .disable_flow(&flow, &Actor::Core, "grant_lost")
        .unwrap();
    f.module.rt.store().write(|tx| {
        tx.execute("INSERT INTO flow_grant_losses(flow_id,provider,grant_key,grant_label,echoable,run_id,version,state,next_poll_at,lost_at) VALUES (?1,'plexus','g1.ref','github: create_issue',1,'run-evidence',1,'stopped',0,0)",[&flow])?;
        Ok(())
    }).unwrap();
    for method in ["flow.health", "flow.list"] {
        let raw = f
            .module
            .handle(&Caller::Operator, method, json!({}))
            .unwrap();
        let entry = raw["flows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["flow_id"] == flow)
            .unwrap();
        assert_eq!(entry["disabled"]["reason"], "grant_lost");
        assert_eq!(entry["grant_losses"][0]["grant"], "g1.ref");
        assert_eq!(
            entry["grant_losses"][0]["grant_label"],
            "github: create_issue"
        );
        assert_eq!(entry["grant_losses"][0]["state"], "stopped");
    }
    let retirement = json!({"agent_id":"SYNAPSE","agent_reference":"SYNAPSE","provider":"prefrontal-core","action":"sink.status","at_ms":T0});
    f.module.rt.store().write(|tx| {
        tx.execute("UPDATE flows SET disabled_reason='agent_retired',agent_retirement=?2 WHERE flow_id=?1",rusqlite::params![flow,retirement.to_string()])?;
        Ok(())
    }).unwrap();
    for method in ["flow.health", "flow.list"] {
        let raw = f
            .module
            .handle(&Caller::Operator, method, json!({}))
            .unwrap();
        let entry = raw["flows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["flow_id"] == flow)
            .unwrap();
        assert_eq!(entry["disabled"]["reason"], "agent_retired");
        assert_eq!(entry["agent_retirement"], retirement);
    }
    let refused = f
        .module
        .handle(&Caller::Operator, "flow.enable", json!({"flow_id":flow}))
        .unwrap_err();
    assert_eq!(refused.code, "agent_retired");
    assert_eq!(refused.detail, Some(retirement));
}
