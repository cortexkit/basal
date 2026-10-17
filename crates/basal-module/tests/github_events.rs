//! Execute the shipped GitHub events flow with synthetic issue/PR event bodies.
//! The worker resolves each notice and verifies its body digest before routing,
//! then delivers to the repository's owner and reacts on newly opened items.
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
const BASAL: &str = "agent_12eb4c0c42cecdb5";
const PLEX: &str = "agent_309edd5e94fbb2fe";
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
    /// Raw journaled envelopes of every targeted `github_react` call.
    reactions: Mutex<Vec<String>>,
    /// What `github_react` answers; a fulfilled `{ok:true}` when unset.
    react_outcome: Mutex<Option<HostOutcome>>,
}

fn text(value: Value) -> JsonText {
    JsonText::new(value.to_string()).unwrap()
}

impl GithubHost {
    fn answer(&self, request: &CallRequest) -> Option<HostOutcome> {
        let args: Value = serde_json::from_str(request.args.as_str()).unwrap();
        match &request.kind {
            CallKind::Primitive(Primitive::SinkDigest) => {
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
                let body =
                    self.bodies.lock().unwrap()[args["event_key"].as_str().unwrap()].to_string();
                Some(HostOutcome::fulfilled(text(json!({
                    "body":body, "digest":format!("{:x}", Sha256::digest(body.as_bytes())),
                    "event_key":args["event_key"], "event_name":args["event_name"]
                }))))
            }
            CallKind::OpFor { module, op } if module == "plexus" && op == "github_react" => {
                self.reactions
                    .lock()
                    .unwrap()
                    .push(request.args.as_str().to_owned());
                Some(
                    self.react_outcome
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| HostOutcome::fulfilled(text(json!({"ok":true})))),
                )
            }
            CallKind::Op { .. } | CallKind::OpFor { .. } => {
                panic!("flow made an unexpected op: {request:?}")
            }
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
    /// The install card's fields, read before the operator approved it.
    card: Value,
}

impl Test {
    fn new() -> Self {
        let host = Arc::new(GithubHost::default());
        let catalog = MockCatalog::standard();
        catalog.set_op(
            "plexus",
            "events_get",
            OpDecl {
                kind: Some(OpKind::Query),
                cause_echo: false,
                shell_capable: false,
            },
        );
        // A reaction is a mutation on the module whose events trigger this
        // flow. It does not echo causes; install accepts it because Plexus
        // declares in its tool catalog that a reaction never emits an event.
        catalog.set_op(
            "plexus",
            "github_react",
            OpDecl {
                kind: Some(OpKind::Mutate),
                cause_echo: false,
                shell_capable: false,
            },
        );
        catalog.set_emits_events("plexus", "github_react", Some(false));
        // Targeted calls resolve the owner through the agent registry.
        for owner in owners(SCRIPT)
            .split('\'')
            .filter(|part| part.starts_with("agent_"))
        {
            catalog.add_agent(owner);
        }
        for name in EVENTS {
            catalog.set_event(
                "plexus",
                name,
                2,
                EventDecl {
                    headers: ["kind", "repo"].map(str::to_owned).into(),
                    origin: EventOrigin::External,
                    body: EventBody::Resolved {
                        resolve_op: "events_get".into(),
                    },
                },
            );
        }
        let consent = MockConsent::new();
        let f = common::fixture(
            "github-events",
            Options {
                hosts: Some(Hosts {
                    host: host.clone(),
                    catalog: Arc::new(catalog),
                    consent: Arc::new(consent.clone()),
                    transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
                    hooks: Arc::new(NoHooks),
                }),
                ..Options::with_worker()
            },
        );
        let reply = common::install(
            &f,
            &Caller::Operator,
            SCRIPT,
            &serde_json::from_str(MANIFEST).unwrap(),
        );
        let card_id = reply["card_id"].as_str().unwrap();
        let card = consent.card(card_id).unwrap().fields;
        assert!(consent.decide(card_id, basal_host::CardDecision::Approve, "operator"));
        Self { f, host, card }
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

    fn notice(&self, body: Value, name: &str, header_repo: &str) -> Value {
        self.notice_with_clock(body, name, header_repo, self.f.clock.now_ms())
    }

    fn notice_with_clock(&self, mut body: Value, name: &str, header_repo: &str, now: i64) -> Value {
        self.host.mock.set_clock(now as f64);
        let key = format!("{:064x}", self.host.bodies.lock().unwrap().len() + 1);
        body["event_key"] = json!(key);
        let bytes = body.to_string();
        self.host.bodies.lock().unwrap().insert(key.clone(), body);
        let report = self
            .f
            .module
            .rt
            .admit_events(&[basal_core::events::Notice {
                published_at_ms: self.f.clock.now_ms(),
                subject: format!("ck.account.event.plexus.{name}.v2"),
                event_key: key,
                digest: format!("{:x}", Sha256::digest(bytes.as_bytes())),
                headers: [
                    ("kind".into(), name.into()),
                    ("repo".into(), header_repo.into()),
                ]
                .into(),
            }])
            .unwrap();
        assert_eq!(report.admitted, 1);
        let startable = self.f.module.rt.startable().unwrap();
        assert_eq!(startable.len(), 1);
        let run = startable[0].0.clone();
        self.f.module.engine.run_until_idle(1000).unwrap();
        let done = self.f.module.rt.run(&run).unwrap();
        assert_eq!(done.state, RunState::Succeeded, "{done:#?}");
        done.result
            .map(|value| serde_json::from_str(&value).unwrap())
            .unwrap_or(Value::Null)
    }

    fn writes(&self) -> Vec<Value> {
        self.host.writes.lock().unwrap().clone()
    }

    fn reactions(&self) -> Vec<Value> {
        self.host
            .reactions
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_str(raw).unwrap())
            .collect()
    }

    fn memory(&self) -> Value {
        let stored: String = self
            .f
            .module
            .rt
            .store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT value FROM kv WHERE flow_id='github-events' AND key='github-events'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
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
        body.as_object_mut().unwrap().extend(
            json!({
                "draft":false, "merged":false, "head_ref":"topic", "base_ref":"main"
            })
            .as_object()
            .unwrap()
            .clone(),
        );
    }
    body
}

fn owners(script: &str) -> String {
    script
        .split_once("const OWNERS = {")
        .unwrap()
        .1
        .split_once("\n};")
        .unwrap()
        .0
        .to_owned()
}

/// The exact `github_react` arguments, as basal journals them inside the
/// targeted-call envelope, for an opened item numbered 42 in cortexkit/basal.
fn pinned_reaction(kind: &str, event: usize) -> String {
    format!(
        r#""args":{{"kind":"{kind}","reaction":"eyes","repository":"cortexkit/basal","request_nonce":"github-events:{event:064x}","target":{{"number":42}}}}"#
    )
}

#[test]
fn manifest_declares_v2_repo_events_global_sink_and_only_a_delegated_react_grant() {
    Manifest::parse(MANIFEST).unwrap();
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    assert_eq!(manifest["version"], 3);
    // Nothing beyond the trigger, the audience sink and the two ops: no
    // facts, board, status, claims or model grant.
    assert_eq!(
        manifest.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "audience", "deadline", "format", "id", "ops", "purpose", "sinks", "trigger", "version"
        ]
    );
    assert_eq!(manifest["audience"], json!({"kind":"global"}));
    assert_eq!(
        manifest["sinks"],
        json!([{"agent":"$audience", "digest_max":"wake"}])
    );
    // Only github_react may act as the target agent; the body resolver reads
    // on the flow's own scope.
    assert_eq!(
        manifest["ops"],
        json!([
            {"module":"plexus", "op":"events_get"},
            {"module":"plexus", "op":"github_react", "for_target":"delegate"}
        ])
    );
    assert_eq!(
        manifest["trigger"]["events"],
        json!(EVENTS.map(|name|
        json!({"module":"plexus", "name":name, "version":2, "match":{"repo":"cortexkit/*"}})))
    );
    assert!(
        manifest["purpose"]
            .as_str()
            .unwrap()
            .contains("owning agent")
    );
}

#[test]
fn install_admits_github_react_by_plexus_declaring_it_emits_no_events() {
    // `Test::new` installs without the loop override; the card shows which
    // op passed the loop rule by declaration rather than by cause echo.
    let test = Test::new();
    let react = test.card["ops"]
        .as_array()
        .unwrap()
        .iter()
        .find(|op| op["op"] == "github_react")
        .unwrap();
    assert_eq!(react["cause_echo"], false);
    assert_eq!(react["emits_events"], false);
    let events_get = test.card["ops"]
        .as_array()
        .unwrap()
        .iter()
        .find(|op| op["op"] == "events_get")
        .unwrap();
    assert_eq!(events_get["emits_events"], Value::Null);
    let loop_warnings: Vec<&Value> = test.card["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["kind"].as_str().is_some_and(|k| k.starts_with("loop_")))
        .collect();
    assert_eq!(loop_warnings.len(), 1, "{loop_warnings:?}");
    assert_eq!(loop_warnings[0]["kind"], "loop_exempt_no_events");
    assert_eq!(loop_warnings[0]["op"], "github_react");
}

#[test]
fn owner_map_matches_router_and_bot_bindings_are_populated() {
    assert_eq!(
        owners(SCRIPT),
        owners(include_str!("../../../flows/ci-failure-router/flow.js"))
    );
    let bots = SCRIPT
        .split_once("const BOT_SLUGS = {")
        .unwrap()
        .1
        .split_once("\n};")
        .unwrap()
        .0;
    assert_eq!(bots.lines().filter(|line| line.contains(": '")).count(), 26);
    assert!(bots.contains("agent_16013c86: 'ckdesk-alfonso'"));
    assert!(!bots.contains("agent_ef98bc82ef005a63"));
    assert!(!bots.contains("agent_fb7fb88282f843b5"));
}

#[test]
fn opened_issue_wakes_its_owner_and_reacts_once_with_the_pinned_request() {
    let t = Test::new();
    let result = t.event(item("issue", "opened"));
    let writes = t.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["agent"], BASAL);
    assert_eq!(writes[0]["action"], "wake");
    assert_eq!(writes[0]["replace_key"], "github:cortexkit/basal:issue:42");
    // Routing comparison fields stay in the run result, not the delivered item.
    let data = writes[0]["item"]["data"].as_object().unwrap();
    for field in ["would_owner", "would_action", "suppressed_reason", "rule"] {
        assert!(!data.contains_key(field), "{field}");
    }
    assert_eq!(data["repo"], "cortexkit/basal");
    assert_eq!(data["number"], 42);
    assert_eq!(result["events"][0]["event_key"], format!("{:064x}", 1));
    assert_eq!(result["events"][0]["would_owner"], BASAL);
    assert_eq!(result["events"][0]["would_action"], "wake");
    assert_eq!(result["events"][0]["rule"], "deliver");
    assert_eq!(result["delivered"], 1);
    assert!(
        writes[0]["item"]["body"]
            .as_str()
            .unwrap()
            .contains("cortexkit/basal Issue #42 opened observed at")
    );
    assert_eq!(t.memory()["items"].as_object().unwrap().len(), 1);
    // One reaction, as the owner, with byte-stable arguments and nonce.
    let raw = t.host.reactions.lock().unwrap().clone();
    assert_eq!(raw.len(), 1);
    assert!(raw[0].contains(&pinned_reaction("issue", 1)), "{}", raw[0]);
    let reaction = &t.reactions()[0];
    assert_eq!(reaction["agent"], BASAL);
    assert_eq!(
        reaction["args"],
        json!({"repository":"cortexkit/basal", "kind":"issue", "target":{"number":42},
            "reaction":"eyes", "request_nonce":format!("github-events:{:064x}", 1)})
    );
    assert_eq!(
        result["reaction"],
        json!({"owner":BASAL, "request_nonce":format!("github-events:{:064x}", 1), "code":null})
    );
    // A second opened notice for the same item (a new event key) is still
    // delivered but never reacts again.
    let again = t.event(item("issue", "opened"));
    assert_eq!(again["delivered"], 1);
    assert_eq!(again["reaction"], Value::Null);
    assert_eq!(t.host.reactions.lock().unwrap().len(), 1);
}

#[test]
fn updates_and_releases_never_react() {
    let t = Test::new();
    for action in ["updated", "created", "closed", "reopened"] {
        let result = t.event(item("issue", action));
        assert_eq!(result["delivered"], 1, "{action}");
        assert_eq!(result["reaction"], Value::Null, "{action}");
    }
    let result = t.event(item("pull_request", "synchronize"));
    assert_eq!(result["delivered"], 1);
    let release = json!({"repo":"cortexkit/basal", "kind":"release", "action":"published",
        "tag_name":"v1", "title":"Synthetic release",
        "url":"https://github.com/cortexkit/basal/releases/tag/v1", "updated_at":null,
        "author":"someone", "actor_logins":["someone"], "actor_unknown":null});
    let result = t.event(release);
    assert_eq!(result["delivered"], 1);
    assert_eq!(result["reaction"], Value::Null);
    assert_eq!(t.writes().len(), 6);
    assert!(
        t.writes()
            .iter()
            .all(|write| write["action"] == "piggyback")
    );
    assert!(t.host.reactions.lock().unwrap().is_empty());
}

#[test]
fn a_reaction_refusal_still_delivers_the_digest_and_records_its_code() {
    for (code, outcome) in [
        (
            "vendor_rejected",
            HostOutcome::rejected(text(
                json!({"code":"vendor_rejected", "message":"rate limited"}),
            )),
        ),
        (
            "agent_grant_absent",
            HostOutcome::rejected(text(
                json!({"code":"agent_grant_absent", "message":"no grant"}),
            )),
        ),
        // A refusal Plexus reports inside a fulfilled tool result.
        (
            "policy_blocked",
            HostOutcome::fulfilled(text(json!({"error":{"code":"policy_blocked"}}))),
        ),
    ] {
        let t = Test::new();
        *t.host.react_outcome.lock().unwrap() = Some(outcome);
        let result = t.event(item("pull_request", "opened"));
        assert_eq!(result["delivered"], 1, "{code}");
        assert_eq!(result["reaction"]["code"], code);
        assert_eq!(result["reaction"]["owner"], BASAL);
        let writes = t.writes();
        assert_eq!(writes.len(), 1, "{code}");
        assert_eq!(writes[0]["agent"], BASAL);
        assert_eq!(writes[0]["action"], "wake");
        assert_eq!(t.reactions()[0]["args"]["kind"], "pr");
        assert_eq!(t.memory()["items"].as_object().unwrap().len(), 1);
    }
}

#[test]
fn updates_replace_one_item_and_accumulate_times_with_newest_summary() {
    let t = Test::new();
    t.f.clock.set(
        "2026-07-01T10:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond(),
    );
    t.event(item("issue", "opened"));
    t.f.clock.advance(60_000);
    let mut update = item("issue", "updated");
    update["title"] = json!("Newest synthetic title");
    let result = t.event(update);
    assert_eq!(result["events"][0]["would_action"], "piggyback");
    assert_eq!(t.host.items.lock().unwrap().len(), 1);
    let writes = t.writes();
    assert_eq!(writes[0]["replace_key"], writes[1]["replace_key"]);
    assert_eq!(writes[1]["agent"], BASAL);
    assert_eq!(writes[1]["action"], "piggyback");
    assert_eq!(writes[1]["item"]["data"]["observation_count"], 2);
    assert_eq!(
        writes[1]["item"]["body"],
        "- cortexkit/basal Issue #42 updated at 12:00 and 12:01: \"Newest synthetic title\""
    );
    // Only the opened notice reacted.
    assert_eq!(t.host.reactions.lock().unwrap().len(), 1);
}

#[test]
fn own_bot_authored_pr_with_only_automated_activity_is_suppressed() {
    let t = Test::new();
    let mut body = item("pull_request", "synchronize");
    body["author"] = json!("BASAL-Alfonso[bot]");
    body["actor_logins"] = json!(["dependabot[bot]"]);
    let result = t.event(body);
    assert!(t.writes().is_empty());
    assert_eq!(
        result["events"][0]["suppressed_reason"],
        "own_bot_automated"
    );
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
    assert_eq!(t.writes()[0]["action"], "piggyback");
}

#[test]
fn sole_operator_and_all_suppressed_actors_are_dropped_case_insensitively() {
    let t = Test::new();
    for actors in [
        json!(["UALTINOK"]),
        json!(["Alfonso-AFT", "mc-ALFONSO", "basal-alfonso[bot]"]),
    ] {
        let mut body = item("issue", "updated");
        body["actor_logins"] = actors;
        let result = t.event(body);
        assert_eq!(
            result["events"][0]["suppressed_reason"],
            "suppressed_actor_set"
        );
    }
    assert!(t.writes().is_empty());
}

#[test]
fn an_agents_own_action_and_the_automation_it_triggers_never_wake() {
    // MC commented on its own PR, which set off a label and a design-gate
    // rerun: github-actions only acted because MC did.
    let t = Test::new();
    for (repo, actors) in [
        (
            "cortexkit/magic-context",
            json!(["github-actions[bot]", "magic-alfonso[bot]"]),
        ),
        ("cortexkit/basal", json!(["GITHUB-ACTIONS", "ualtinok"])),
        (
            "cortexkit/basal",
            json!(["basal-alfonso", "github-actions[bot]"]),
        ),
    ] {
        let mut body = item("issue", "updated");
        body["repo"] = json!(repo);
        body["actor_logins"] = actors.clone();
        assert_eq!(
            t.event(body)["events"][0]["suppressed_reason"],
            "suppressed_actor_set",
            "{actors}"
        );
    }
    // github-actions alone, a human, or an unlisted review bot still wakes.
    for actors in [
        json!(["github-actions[bot]"]),
        json!(["github-actions[bot]", "ualtinok", "another-human"]),
        json!(["ualtinok", "coderabbitai[bot]"]),
    ] {
        let mut body = item("issue", "updated");
        body["actor_logins"] = actors.clone();
        assert_eq!(t.event(body)["delivered"], 1, "{actors}");
    }
}

#[test]
fn a_workflow_run_is_decided_by_who_started_it() {
    let t = Test::new();
    // Plexus carries the starter flat, as GitHub's user object.
    let run = |starter: Value, actors: Value| {
        let mut body = item("issue", "updated");
        body["triggering_actor"] = json!({"login": starter});
        body["actor"] = json!({"login": "someone-else"});
        body["actor_logins"] = actors;
        body
    };
    // Started by a suppressed login, and only suppressed or github-actions acted
    // (an empty set counts): an echo.
    for actors in [
        json!([]),
        json!(["github-actions[bot]"]),
        json!(["ualtinok"]),
    ] {
        let result = t.event(run(json!("ualtinok"), actors.clone()));
        assert_eq!(
            result["events"][0]["suppressed_reason"], "suppressed_actor_set",
            "{actors}"
        );
    }
    // Started by anyone else: wakes even when only the agent acted.
    let result = t.event(run(json!("another-human"), json!(["ualtinok"])));
    assert_eq!(result["delivered"], 1);
    // With no triggering_actor, the run's actor decides; a nested workflow_run,
    // when present, is the run itself.
    let mut body = item("issue", "updated");
    body["actor"] = "another-human".into();
    body["workflow_run"] = json!({"actor": "ualtinok"});
    body["actor_logins"] = json!(["github-actions[bot]"]);
    assert_eq!(
        t.event(body)["events"][0]["suppressed_reason"],
        "suppressed_actor_set"
    );
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
    body["actor_logins"] = json!([
        "CUBIC-dev-ai",
        "GREPTILE-apps",
        "ualtinok",
        "aft-alfonso[bot]"
    ]);
    assert_eq!(
        t.event(body.clone())["events"][0]["suppressed_reason"],
        "suppressed_actor_set"
    );
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
        assert_eq!(
            t.event(body)["events"][0]["suppressed_reason"],
            "closed_pr_empty_actors"
        );
    }
    assert!(t.writes().is_empty());
}

#[test]
fn empty_and_unknown_actor_sets_never_match_the_suppression_set() {
    let t = Test::new();
    for actors in [json!([]), Value::Null] {
        let mut body = item("issue", "updated");
        body["actor_unknown"] = if actors.is_null() {
            json!({"reason":"enrichment_unavailable"})
        } else {
            Value::Null
        };
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
    assert!(
        t.writes()[0]["item"]["body"]
            .as_str()
            .unwrap()
            .contains("reopened observed at")
    );
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

#[test]
fn bot_bindings_use_each_owners_login_and_missing_slugs_disable_only_bot_rule() {
    let t = Test::new();
    for (repo, bot) in [
        ("cortexkit/alfonso-desktop", "ckdesk-alfonso[bot]"),
        ("cortexkit/aft", "aft-alfonso[bot]"),
    ] {
        let mut body = item("pull_request", "updated");
        body["repo"] = json!(repo);
        body["author"] = json!(bot);
        assert_eq!(
            t.event(body)["events"][0]["suppressed_reason"],
            "own_bot_automated"
        );
    }
    for (repo, owner) in [
        ("cortexkit/motor", "agent_ef98bc82ef005a63"),
        ("cortexkit/cortexkit-cloud", "agent_fb7fb88282f843b5"),
    ] {
        let mut body = item("pull_request", "updated");
        body["repo"] = json!(repo);
        body["author"] = json!("unbound-alfonso[bot]");
        body["actor_logins"] = json!(["unbound-alfonso[bot]"]);
        assert_eq!(t.event(body.clone())["events"][0]["would_owner"], owner);
        body["actor_logins"] = json!(["ualtinok"]);
        assert_eq!(
            t.event(body)["events"][0]["suppressed_reason"],
            "suppressed_actor_set"
        );
    }
    assert_eq!(t.writes().len(), 2);
}

#[test]
fn human_filer_is_not_a_commenter_and_bot_comments_do_not_bypass_authorship() {
    let t = Test::new();
    let mut body = item("pull_request", "updated");
    body["actor_logins"] = json!(["basal-alfonso[bot]"]);
    assert_eq!(
        t.event(body.clone())["events"][0]["suppressed_reason"],
        "suppressed_actor_set"
    );
    body["author"] = json!("basal-alfonso[bot]");
    body["actor_logins"] = json!(["another-human"]);
    assert_eq!(
        t.event(body.clone())["events"][0]["suppressed_reason"],
        "own_bot_automated"
    );
    body["comment_author"] = json!("someone[BoT]");
    body["comment_url"] = json!("https://github.com/cortexkit/basal/pull/42#issuecomment-2");
    assert_eq!(
        t.event(body)["events"][0]["suppressed_reason"],
        "own_bot_automated"
    );
    assert!(t.writes().is_empty());
}

#[test]
fn observation_times_cap_at_six_and_count_overflow_without_old_titles() {
    let t = Test::new();
    t.f.clock.set(
        "2026-07-01T10:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond(),
    );
    for i in 0..9 {
        let mut body = item("issue", "updated");
        body["title"] = json!(format!("Synthetic observation {i}"));
        t.event(body);
        t.f.clock.advance(60_000);
    }
    assert_eq!(t.host.items.lock().unwrap().len(), 1);
    let last = t.writes().pop().unwrap();
    assert_eq!(
        last["item"]["body"],
        "- cortexkit/basal Issue #42 updated at 12:00, 12:01, 12:02, 12:03, 12:04, 12:05 and 3 more: \"Synthetic observation 8\""
    );
    assert_eq!(last["item"]["data"]["overflow"], 3);
    assert_eq!(
        last["item"]["data"]["observation_times"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
}

#[test]
fn modeled_debounce_rolls_caps_and_restarts_after_due_in_kv_and_data() {
    let t = Test::new();
    let first = t.f.clock.now_ms();
    for (minutes, due_minutes) in [(0, 30), (20, 50), (40, 60), (59, 60), (61, 91)] {
        t.f.clock.set(first + minutes * 60_000);
        let result = t.event(item("issue", "updated"));
        let data = t.writes().pop().unwrap()["item"]["data"].clone();
        assert_eq!(data["due_ms"], first + due_minutes * 60_000);
        assert_eq!(data["latest_ms"], first + minutes * 60_000);
        assert_eq!(
            data["first_seen_ms"],
            if minutes == 61 {
                first + 61 * 60_000
            } else {
                first
            }
        );
        for key in ["first_seen_ms", "latest_ms", "due_ms"] {
            assert_eq!(
                data[key],
                t.memory()["items"]["github:cortexkit/basal:issue:42"][key]
            );
            assert_eq!(result["events"][0][key], data[key]);
        }
    }
    assert_eq!(
        t.writes().len(),
        5,
        "debounce is modeled, not a delayed shadow write"
    );
}

#[test]
fn opened_pr_bypasses_debounce_and_first_update_starts_its_own_window() {
    let t = Test::new();
    let first = t.f.clock.now_ms();
    let opened = t.event(item("pull_request", "opened"));
    assert_eq!(opened["events"][0]["would_action"], "wake");
    assert_eq!(opened["events"][0]["due_ms"], first);
    t.f.clock.advance(5 * 60_000);
    let update = t.event(item("pull_request", "updated"));
    assert_eq!(update["events"][0]["first_seen_ms"], first + 5 * 60_000);
    assert_eq!(update["events"][0]["due_ms"], first + 35 * 60_000);
    let raw = t.host.reactions.lock().unwrap().clone();
    assert_eq!(raw.len(), 1);
    assert!(raw[0].contains(&pinned_reaction("pr", 1)), "{}", raw[0]);
}

#[test]
fn rendering_sanitizes_vendor_text_and_allowlists_action_words() {
    let t = Test::new();
    let mut body = item("issue", "<wake>\r\n- forged action");
    body["title"] = json!("<tag> \"quoted\" 'apostrophe'\r\n- forged title");
    t.event(body);
    let rendered = t.writes()[0]["item"]["body"].as_str().unwrap().to_owned();
    assert!(rendered.contains("Issue #42 update observed at"));
    assert!(rendered.ends_with("\"‹tag> 'quoted' 'apostrophe'  - forged title\""));
    assert!(!rendered.contains(['<', '\r', '\n']));
    assert_eq!(rendered.matches('"').count(), 2);
    for (action, word) in [
        ("created", "new comment"),
        ("edited", "edit"),
        ("closed", "closed"),
        ("reopened", "reopened"),
        ("constructor", "update"),
    ] {
        let mut body = item("issue", action);
        body["number"] = json!(100 + t.writes().len());
        t.event(body);
        assert!(
            t.writes().last().unwrap()["item"]["body"]
                .as_str()
                .unwrap()
                .contains(&format!(" {word} observed at"))
        );
    }
}

#[test]
fn long_multibyte_titles_and_urls_fit_all_core_digest_limits() {
    let t = Test::new();
    let mut body = item("issue", "opened");
    body["title"] = json!("🦀é\"\n<".repeat(4_000));
    body["url"] = json!(format!(
        "https://github.com/cortexkit/basal/issues/42?{}",
        "x".repeat(20_000)
    ));
    assert_eq!(t.event(body)["delivered"], 1);
    let item = &t.writes()[0]["item"];
    assert!(item["body"].as_str().unwrap().ends_with("...\""));
    assert!(item["body"].as_str().unwrap().len() > 4000);
    assert!(
        item["data"]["truncated"]["observed.summary"]["original_bytes"]
            .as_u64()
            .unwrap()
            > 8192
    );
    assert!(item["links"].as_array().unwrap().is_empty());
    assert!(serde_json::to_vec(&t.memory()).unwrap().len() <= 48000);
}

#[test]
fn distinct_repo_kind_and_number_each_get_a_distinct_replacement_key() {
    let t = Test::new();
    t.event(item("issue", "updated"));
    t.event(item("pull_request", "updated"));
    let mut body = item("issue", "updated");
    body["number"] = json!(43);
    t.event(body.clone());
    body["repo"] = json!("cortexkit/plexus");
    t.event(body);
    assert_eq!(t.host.items.lock().unwrap().len(), 4);
    assert_eq!(t.memory()["items"].as_object().unwrap().len(), 4);
    // Each repository's item goes to that repository's owner.
    let agents: Vec<_> = t.writes().iter().map(|w| w["agent"].clone()).collect();
    assert_eq!(agents, [BASAL, BASAL, BASAL, PLEX]);
}

#[test]
fn identity_is_required_but_kind_and_action_have_no_whitelist() {
    let t = Test::new();
    for field in ["kind", "number"] {
        let mut body = item("issue", "opened");
        body[field] = Value::Null;
        assert_eq!(
            t.notice(body, EVENTS[0], "cortexkit/basal")["events"][0]["suppressed_reason"],
            "missing_identity"
        );
    }
    let mut body = item("discussion", "unknown-action");
    body["number"] = json!(0);
    assert_eq!(t.event(body)["delivered"], 1);
    assert_eq!(t.writes()[0]["action"], "piggyback");
    let mut body = item("issue", "suppressed_own_echo");
    assert_eq!(
        t.event(body.clone())["events"][0]["suppressed_reason"],
        "own_echo"
    );
    body["repo"] = Value::Null;
    t.notice(body, EVENTS[0], "cortexkit/basal");
    assert_eq!(t.writes().len(), 1);
}

#[test]
fn stale_drops_are_visible_and_six_hour_boundary_and_unknown_times_deliver() {
    let t = Test::new();
    t.f.clock.set(
        "2026-10-16T12:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond(),
    );
    for field in ["completed_at", "updated_at"] {
        let mut body = item("issue", "updated");
        body[field] = json!("2026-10-16T05:59:59Z");
        let result = t.event(body);
        assert_eq!(result["events"][0]["suppressed_reason"], "stale");
        assert_eq!(result["events"][0]["would_owner"], BASAL);
        assert_eq!(result["delivered"], 0);
    }
    let mut body = item("issue", "updated");
    body["updated_at"] = json!("2026-10-16T06:00:00Z");
    assert_eq!(t.event(body)["delivered"], 1);
    assert_eq!(t.event(item("issue", "updated"))["delivered"], 1);
}

#[test]
fn verified_repo_is_rechecked_independently_of_notice_headers() {
    let t = Test::new();
    let mut body = item("issue", "opened");
    body["repo"] = json!("elsewhere/not-cortexkit");
    t.notice(body, EVENTS[0], "cortexkit/basal");
    assert!(t.writes().is_empty());
}

#[test]
fn release_assumes_a_tag_identity_and_is_a_piggyback_update() {
    let t = Test::new();
    for tag_field in ["tag_name", "tag"] {
        let mut body = json!({"repo":"cortexkit/basal", "kind":"release", "action":"published",
            "title":"Synthetic release", "url":"https://github.com/cortexkit/basal/releases/tag/v1",
            "updated_at":null, "author":"someone", "actor_logins":["someone"], "actor_unknown":null});
        body[tag_field] = json!("v1");
        let result = t.event(body);
        assert_eq!(result["events"][0]["number"], "v1");
        assert_eq!(result["events"][0]["kind"], "release");
        assert_eq!(result["events"][0]["would_action"], "piggyback");
    }
    assert_eq!(t.host.items.lock().unwrap().len(), 1);
    assert_eq!(
        t.writes()[0]["replace_key"],
        "github:cortexkit/basal:release:v1"
    );
}

#[test]
fn madrid_clock_covers_2026_dst_transitions_and_normal_summer_and_winter() {
    let t = Test::new();
    for (timestamp, expected) in [
        ("2026-03-29T00:59:00Z", "01:59"),
        ("2026-03-29T01:00:00Z", "03:00"),
        ("2026-10-25T00:59:00Z", "02:59"),
        ("2026-10-25T01:00:00Z", "02:00"),
        ("2026-07-01T10:15:00Z", "12:15"),
        ("2026-12-01T10:15:00Z", "11:15"),
    ] {
        let now = timestamp
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond();
        let mut body = item("issue", "opened");
        body["number"] = json!(100 + t.writes().len());
        t.notice_with_clock(body, EVENTS[0], "cortexkit/basal", now);
        assert!(
            t.writes().last().unwrap()["item"]["body"]
                .as_str()
                .unwrap()
                .contains(&format!("observed at {expected}.")),
            "{timestamp}"
        );
    }
}
