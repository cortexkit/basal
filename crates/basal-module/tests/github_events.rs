//! Execute the shipped GitHub shadow with synthetic issue/PR event bodies.
//! The worker resolves each notice and verifies its body digest before routing.
mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use basal_core::{Manifest, NoHooks, RunState};
use basal_host::mock::MockHost;
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, EventBody, EventDecl, EventOrigin, Host,
    HostOutcome, InstallStatus, MockCatalog, MockConsent, OpDecl, OpKind, TransportError,
};
use basal_module::{caller::Caller, module::Hosts};
use basal_proto::{CallKind, JsonText, Primitive};
use common::{Fixture, Options};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MANIFEST: &str = include_str!("../../../flows/github-events/manifest.json");
const SCRIPT: &str = include_str!("../../../flows/github-events/flow.js");
const ALF: &str = "agent_fff43bb5f4bea20b";
const BASAL: &str = "agent_12eb4c0c42cecdb5";
const EVENTS: [&str; 3] = [
    "github_issue_changed",
    "github_pr_changed",
    "github_release_published",
];

#[derive(Default)]
struct GithubHost {
    mock: MockHost,
    bodies: Mutex<BTreeMap<String, Value>>,
    writes: Mutex<Vec<Value>>,
    items: Mutex<BTreeMap<String, Value>>,
}

fn text(value: Value) -> JsonText {
    JsonText::new(value.to_string()).unwrap()
}

impl GithubHost {
    fn answer(&self, request: &CallRequest) -> Option<HostOutcome> {
        let args: Value = serde_json::from_str(request.args.as_str()).unwrap();
        match &request.kind {
            CallKind::Primitive(Primitive::SinkDigest) => {
                assert_eq!(args["agent"], ALF, "shadow must never target an owner");
                assert_eq!(args["action"], "silent", "shadow must never wake");
                let item = &args["item"];
                assert!(item["title"].as_str().unwrap().chars().count() <= 200);
                assert!(item["body"].as_str().unwrap().len() <= 4096);
                assert!(serde_json::to_vec(&item["data"]).unwrap().len() <= 8192);
                assert!(serde_json::to_vec(item).unwrap().len() <= 16384);
                self.items.lock().unwrap().insert(
                    args["replace_key"].as_str().unwrap().to_owned(),
                    item.clone(),
                );
                self.writes.lock().unwrap().push(args);
                Some(HostOutcome::fulfilled(text(json!({"ok":true}))))
            }
            CallKind::Op { module, op } if module == "plexus" && op == "events_get" => {
                let body = self.bodies.lock().unwrap()[args["event_key"].as_str().unwrap()].to_string();
                Some(HostOutcome::fulfilled(text(json!({
                    "body":body, "digest":format!("{:x}", Sha256::digest(body.as_bytes())),
                    "event_key":args["event_key"], "event_name":args["event_name"]
                }))))
            }
            CallKind::Op { .. } => panic!("shadow made a non-events_get op: {request:?}"),
            _ => None,
        }
    }
}

impl Host for GithubHost {
    fn supports_core_capability(&self, capability: &str) -> bool {
        capability == basal_host::core_host::FLOW_DIGEST_REPLACE_KEY_V1
    }
    fn classify(&self, kind: &CallKind) -> CallClass {
        self.mock.classify(kind)
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        if let Some(outcome) = self.answer(request) {
            return Ok(Dispatched::Completed(outcome));
        }
        self.mock.dispatch(request)
    }
    fn dispatch_classified(
        &self,
        request: &CallRequest,
        class: CallClass,
    ) -> Result<Dispatched, TransportError> {
        if let Some(outcome) = self.answer(request) {
            return Ok(Dispatched::Completed(outcome));
        }
        self.mock.dispatch_classified(request, class)
    }
    fn dispatch_committed(&self, request: &CallRequest) {
        self.mock.dispatch_committed(request);
    }
    fn now_ms(&self) -> f64 {
        self.mock.now_ms()
    }
    fn random(&self) -> f64 {
        self.mock.random()
    }
    fn attach(&self, sink: Arc<dyn CompletionSink>) {
        self.mock.attach(sink);
    }
    fn install_status(&self, flow: &str, version: u32) -> Result<InstallStatus, TransportError> {
        self.mock.install_status(flow, version)
    }
}

struct Test {
    f: Fixture,
    host: Arc<GithubHost>,
}

impl Test {
    fn new() -> Self {
        let host = Arc::new(GithubHost::default());
        let catalog = MockCatalog::standard();
        catalog.set_op("plexus", "events_get", OpDecl {
            kind: Some(OpKind::Query), cause_echo: false, shell_capable: false,
        });
        for name in EVENTS {
            catalog.set_event("plexus", name, 2, EventDecl {
                headers: ["kind", "repo"].map(str::to_owned).into(),
                origin: EventOrigin::External,
                body: EventBody::Resolved { resolve_op: "events_get".into() },
            });
        }
        let consent = MockConsent::new();
        let f = common::fixture("github-events", Options {
            hosts: Some(Hosts {
                host: host.clone(), catalog: Arc::new(catalog), consent: Arc::new(consent.clone()),
                transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
                hooks: Arc::new(NoHooks),
            }),
            ..Options::with_worker()
        });
        let reply = common::install(&f, &Caller::Operator, SCRIPT, &serde_json::from_str(MANIFEST).unwrap());
        assert!(consent.decide(reply["card_id"].as_str().unwrap(), basal_host::CardDecision::Approve, "operator"));
        Self { f, host }
    }

    fn event(&self, body: Value) -> Value {
        let name = match body["kind"].as_str() {
            Some("pull_request") => EVENTS[1],
            Some("release") => EVENTS[2],
            _ => EVENTS[0],
        };
        let repo = body["repo"].as_str().unwrap().to_owned();
        self.notice(body, name, &repo)
    }

    fn notice(&self, mut body: Value, name: &str, header_repo: &str) -> Value {
        self.host.mock.set_clock(self.f.clock.now_ms() as f64);
        let key = format!("{:064x}", self.host.bodies.lock().unwrap().len() + 1);
        body["event_key"] = json!(key);
        let bytes = body.to_string();
        self.host.bodies.lock().unwrap().insert(key.clone(), body);
        let report = self.f.module.rt.admit_events(&[basal_core::events::Notice {
            published_at_ms: self.f.clock.now_ms(),
            subject: format!("ck.account.event.plexus.{name}.v2"), event_key: key,
            digest: format!("{:x}", Sha256::digest(bytes.as_bytes())),
            headers: [("kind".into(), name.into()), ("repo".into(), header_repo.into())].into(),
        }]).unwrap();
        assert_eq!(report.admitted, 1);
        let startable = self.f.module.rt.startable().unwrap();
        assert_eq!(startable.len(), 1);
        let run = startable[0].0.clone();
        self.f.module.engine.run_until_idle(1000).unwrap();
        let done = self.f.module.rt.run(&run).unwrap();
        assert_eq!(done.state, RunState::Succeeded, "{done:#?}");
        done.result.map(|value| serde_json::from_str(&value).unwrap()).unwrap_or(Value::Null)
    }

    fn writes(&self) -> Vec<Value> {
        self.host.writes.lock().unwrap().clone()
    }

    fn memory(&self) -> Value {
        let stored: String = self.f.module.rt.store().read(|c| {
            Ok(c.query_row("SELECT value FROM kv WHERE flow_id='github-events' AND key='github-events'", [], |r| r.get(0))?)
        }).unwrap();
        serde_json::from_str(&stored).unwrap()
    }
}

fn item(kind: &str, action: &str) -> Value {
    let mut body = json!({
        "event_key":null, "kind":kind, "repo":"cortexkit/basal", "action":action,
        "number":42, "title":"Synthetic subject", "state":"open",
        "url":"https://github.com/cortexkit/basal/issues/42", "author":"someone",
        "updated_at":null, "actor_logins":["someone"], "actor_unknown":null,
        "comment_url":null, "comment_author":null
    });
    if kind == "pull_request" {
        body.as_object_mut().unwrap().extend(json!({
            "draft":false, "merged":false, "head_ref":"topic", "base_ref":"main"
        }).as_object().unwrap().clone());
    }
    body
}

#[test]
fn manifest_has_only_three_v2_repo_events_global_sink_and_events_get() {
    Manifest::parse(MANIFEST).unwrap();
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    assert_eq!(manifest["version"], 1);
    assert_eq!(manifest["audience"], json!({"kind":"global"}));
    assert_eq!(manifest["sinks"], json!([{"agent":"$audience", "digest_max":"wake"}]));
    assert_eq!(manifest["ops"], json!([{"module":"plexus", "op":"events_get"}]));
    assert_eq!(manifest["trigger"]["events"], json!(EVENTS.map(|name|
        json!({"module":"plexus", "name":name, "version":2, "match":{"repo":"cortexkit/*"}}))));
    assert!(manifest["purpose"].as_str().unwrap().contains("silently to ALF only"));
}

#[test]
fn owner_map_matches_router_and_bot_bindings_are_populated() {
    let owners = |script: &str| script.split_once("const OWNERS = {").unwrap().1.split_once("\n};").unwrap().0.to_owned();
    assert_eq!(owners(SCRIPT), owners(include_str!("../../../flows/ci-failure-router/flow.js")));
    let bots = SCRIPT.split_once("const BOT_SLUGS = {").unwrap().1.split_once("\n};").unwrap().0;
    assert_eq!(bots.lines().filter(|line| line.contains(": '")).count(), 26);
    assert!(bots.contains("agent_16013c86: 'ckdesk-alfonso'"));
    assert!(!bots.contains("agent_ef98bc82ef005a63"));
    assert!(!bots.contains("agent_fb7fb88282f843b5"));
}

#[test]
fn opened_issue_would_wake_owner_but_sends_only_silently_to_alf() {
    let t = Test::new();
    let result = t.event(item("issue", "opened"));
    let writes = t.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["item"]["data"]["would_owner"], BASAL);
    assert_eq!(writes[0]["item"]["data"]["would_action"], "wake");
    assert_eq!(writes[0]["item"]["data"]["suppressed_reason"], Value::Null);
    assert_eq!(result["events"][0]["event_key"], format!("{:064x}", 1));
    assert_eq!(result["events"][0]["would_owner"], BASAL);
    assert_eq!(result["delivered"], 1);
    assert_eq!(writes[0]["replace_key"], "github:cortexkit/basal:issue:42");
    assert!(writes[0]["item"]["body"].as_str().unwrap().contains("cortexkit/basal Issue #42 opened observed at"));
    assert_eq!(t.memory()["items"].as_object().unwrap().len(), 1);
}

#[test]
fn updates_replace_one_item_and_accumulate_times_with_newest_summary() {
    let t = Test::new();
    t.f.clock.set("2026-07-01T10:00:00Z".parse::<jiff::Timestamp>().unwrap().as_millisecond());
    t.event(item("issue", "opened"));
    t.f.clock.advance(60_000);
    let mut update = item("issue", "updated");
    update["title"] = json!("Newest synthetic title");
    let result = t.event(update);
    assert_eq!(result["events"][0]["would_action"], "piggyback");
    assert_eq!(t.host.items.lock().unwrap().len(), 1);
    let writes = t.writes();
    assert_eq!(writes[0]["replace_key"], writes[1]["replace_key"]);
    assert_eq!(writes[1]["item"]["data"]["observation_count"], 2);
    assert_eq!(writes[1]["item"]["body"], "- cortexkit/basal Issue #42 updated at 12:00 and 12:01: \"Newest synthetic title\"");
}

#[test]
fn own_bot_authored_pr_with_only_automated_activity_is_suppressed() {
    let t = Test::new();
    let mut body = item("pull_request", "synchronize");
    body["author"] = json!("BASAL-Alfonso[bot]");
    body["actor_logins"] = json!(["dependabot[bot]"]);
    let result = t.event(body);
    assert!(t.writes().is_empty());
    assert_eq!(result["events"][0]["suppressed_reason"], "own_bot_automated");
}

#[test]
fn own_bot_subject_with_human_comment_is_delivered() {
    let t = Test::new();
    let mut body = item("pull_request", "updated");
    body["author"] = json!("basal-alfonso[bot]");
    body["actor_logins"] = json!(["ualtinok"]);
    body["comment_author"] = json!("ualtinok");
    body["comment_url"] = json!("https://github.com/cortexkit/basal/pull/42#issuecomment-1");
    assert_eq!(t.event(body)["delivered"], 1);
    assert_eq!(t.writes()[0]["item"]["data"]["would_action"], "piggyback");
}

#[test]
fn sole_operator_and_all_suppressed_actors_are_dropped_case_insensitively() {
    let t = Test::new();
    for actors in [json!(["UALTINOK"]), json!(["Alfonso-AFT", "mc-ALFONSO", "basal-alfonso[bot]"])] {
        let mut body = item("issue", "updated");
        body["actor_logins"] = actors;
        let result = t.event(body);
        assert_eq!(result["events"][0]["suppressed_reason"], "suppressed_actor_set");
    }
    assert!(t.writes().is_empty());
}

#[test]
fn operator_plus_another_human_is_delivered() {
    let t = Test::new();
    let mut body = item("issue", "updated");
    body["actor_logins"] = json!(["ualtinok", "another-human"]);
    assert_eq!(t.event(body)["delivered"], 1);
    assert_eq!(t.writes().len(), 1);
}

#[test]
fn project_actor_set_is_scoped_and_combined_with_operator_and_own_bot() {
    let t = Test::new();
    let mut body = item("issue", "updated");
    body["repo"] = json!("cortexkit/aft");
    body["actor_logins"] = json!(["CUBIC-dev-ai", "GREPTILE-apps", "ualtinok", "aft-alfonso[bot]"]);
    assert_eq!(t.event(body.clone())["events"][0]["suppressed_reason"], "suppressed_actor_set");
    body["actor_logins"] = json!(["cubic-dev-ai", "another-human"]);
    assert_eq!(t.event(body.clone())["delivered"], 1);
    body["repo"] = json!("cortexkit/basal");
    body["actor_logins"] = json!(["cubic-dev-ai"]);
    assert_eq!(t.event(body)["delivered"], 1);
}

#[test]
fn closed_or_merged_pr_with_reliably_empty_actors_is_dropped() {
    let t = Test::new();
    for merged in [false, true] {
        let mut body = item("pull_request", "closed");
        body["state"] = json!("closed");
        body["merged"] = json!(merged);
        body["actor_logins"] = json!([]);
        assert_eq!(t.event(body)["events"][0]["suppressed_reason"], "closed_pr_empty_actors");
    }
    assert!(t.writes().is_empty());
}

#[test]
fn empty_and_unknown_actor_sets_never_match_the_suppression_set() {
    let t = Test::new();
    for actors in [json!([]), Value::Null] {
        let mut body = item("issue", "updated");
        body["actor_unknown"] = if actors.is_null() { json!({"reason":"enrichment_unavailable"}) } else { Value::Null };
        body["actor_logins"] = actors;
        assert_eq!(t.event(body)["delivered"], 1);
    }
    let mut body = item("pull_request", "closed");
    body["state"] = json!("closed");
    body["actor_logins"] = Value::Null;
    body["actor_unknown"] = json!({"reason":"enrichment_unavailable"});
    assert_eq!(t.event(body)["delivered"], 1);
}

#[test]
fn reopened_pr_forces_open_before_empty_actor_check() {
    let t = Test::new();
    let mut body = item("pull_request", "reopened");
    body["state"] = json!("closed");
    body["actor_logins"] = json!([]);
    assert_eq!(t.event(body)["delivered"], 1);
    assert_eq!(t.writes()[0]["item"]["data"]["state"], "open");
    assert!(t.writes()[0]["item"]["body"].as_str().unwrap().contains("reopened observed at"));
}

#[test]
fn unmapped_repo_is_counted_without_a_delivery() {
    let t = Test::new();
    let mut body = item("issue", "opened");
    body["repo"] = json!("cortexkit/unmapped");
    let result = t.event(body);
    assert_eq!(result["unmapped"], 1);
    assert_eq!(result["events"][0]["would_owner"], Value::Null);
    assert_eq!(result["events"][0]["suppressed_reason"], "unmapped_repo");
    assert_eq!(t.memory()["unmapped"], 1);
    assert!(t.writes().is_empty());
}
