//! Exercise the template through notice admission, body resolution and the worker.
mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use basal_core::{NoHooks, RunState};
use basal_host::mock::MockHost;
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, EventBody, EventDecl, EventOrigin, Host,
    HostOutcome, InstallStatus, MockCatalog, MockConsent, OpDecl, OpKind, TransportError,
};
use basal_module::{caller::Caller, module::Hosts};
use basal_proto::{CallKind, JsonText};
use common::{Fixture, Options, agent, fixture};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const TEMPLATE: &str = "github-repo-events";
const MANIFEST: &str = include_str!("../../../templates/github-repo-events/manifest.json");
const SCRIPT: &str = include_str!("../../../templates/github-repo-events/flow.js");
const GLOBAL_MANIFEST: &str = include_str!("../../../flows/github-events/manifest.json");
const GLOBAL_SCRIPT: &str = include_str!("../../../flows/github-events/flow.js");
const OWNER: &str = "agent_12eb4c0c42cecdb5";
const REPO: &str = "fixture-org/lanterns";
const FLEET_REPO: &str = "cortexkit/basal";
const ISSUE: &str = "github_issue_changed";
const PR: &str = "github_pr_changed";
const RELEASE: &str = "github_release_published";
const ISSUE_SAMPLE: &str =
    include_str!("../../../test-vectors/github-comment-excerpt-v1/issue-comment.json");
const PR_SAMPLE: &str =
    include_str!("../../../test-vectors/github-comment-excerpt-v1/pr-comment.json");

fn sha256(bytes: &str) -> String {
    format!("{:x}", Sha256::digest(bytes.as_bytes()))
}

struct GithubHost {
    mock: MockHost,
    bodies: Mutex<BTreeMap<String, String>>,
    global: bool,
}

impl Host for GithubHost {
    fn supports_core_capability(&self, capability: &str) -> bool {
        capability == basal_host::core_host::FLOW_DIGEST_REPLACE_KEY_V1
    }

    fn classify(&self, kind: &CallKind) -> CallClass {
        self.mock.classify(kind)
    }

    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        let args: Value = serde_json::from_str(request.args.as_str()).unwrap();
        let reply = match &request.kind {
            CallKind::Op { module, op } if module == "plexus" && op == "events_get" => {
                assert_eq!(args["version"], 2);
                let bodies = self.bodies.lock().unwrap();
                let body = &bodies[args["event_key"].as_str().unwrap()];
                json!({"body":body, "digest":sha256(body), "event_key":args["event_key"],
                    "event_name":args["event_name"]})
            }
            CallKind::Op { module, op }
                if self.global && module == "plexus" && op == "github_read" =>
            {
                assert_eq!(args, json!({"action":"github.bindings.list"}));
                json!({"result":{"generation":1,"bindings":[{
                    "repository":FLEET_REPO,"agent_id":OWNER,"app_handle_id":"basal-alfonso"
                }]}})
            }
            CallKind::OpFor { module, op }
                if self.global && module == "plexus" && op == "github_react" =>
            {
                json!({"ok":true})
            }
            CallKind::Op { .. } | CallKind::OpFor { .. } => {
                panic!("unexpected operation: {request:?}")
            }
            _ => return self.mock.dispatch(request),
        };
        Ok(Dispatched::Completed(HostOutcome::fulfilled(
            JsonText::new(reply.to_string()).unwrap(),
        )))
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
    flow_id: String,
}

impl Test {
    fn new(repo: &str, global: bool) -> Self {
        let host = Arc::new(GithubHost {
            mock: MockHost::default(),
            bodies: Mutex::default(),
            global,
        });
        let catalog = MockCatalog::standard();
        catalog.add_agent(OWNER);
        catalog.set_flow_capable("plexus", true);
        let mut ops = vec![("events_get", OpKind::Query)];
        if global {
            ops.extend([
                ("github_read", OpKind::Query),
                ("github_react", OpKind::Mutate),
            ]);
            catalog.set_emits_events("plexus", "github_react", Some(false));
        }
        for (op, kind) in ops {
            catalog.set_op(
                "plexus",
                op,
                OpDecl {
                    kind: Some(kind),
                    cause_echo: false,
                    shell_capable: false,
                },
            );
        }
        for name in [ISSUE, PR, RELEASE] {
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
        let f = fixture(
            TEMPLATE,
            Options {
                hosts: Some(Hosts {
                    host: host.clone(),
                    catalog: Arc::new(catalog),
                    consent: Arc::new(MockConsent::new()),
                    transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
                    hooks: Arc::new(NoHooks),
                }),
                ..Options::with_worker()
            },
        );
        f.clock.set(
            "2026-08-20T12:02:03.456Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_millisecond(),
        );
        let flow_id = if global {
            assert_eq!(repo, FLEET_REPO);
            basal_core::test_support::seed_stored_version(
                &f.module.rt,
                GLOBAL_SCRIPT,
                GLOBAL_MANIFEST,
                false,
            )
            .unwrap();
            "github-events".into()
        } else {
            let registered = f
                .module
                .handle(
                    &Caller::Operator,
                    "package.register",
                    json!({"manifest":MANIFEST,"script":SCRIPT}),
                )
                .unwrap();
            assert_eq!(registered["package"], TEMPLATE);
            let stored = f.module.rt.get_package(TEMPLATE, 1).unwrap();
            assert_eq!(stored["manifest"], MANIFEST);
            assert_eq!(stored["script"], SCRIPT);
            let instance = f.module.handle(&agent(OWNER), "flow.instantiate",
                json!({"template":TEMPLATE,"version":1,"params":{"repo":repo},"expires_in":"365d"})).unwrap();
            assert_eq!(instance["agent_id"], OWNER);
            assert_eq!(instance["params"], json!({"repo":repo}));
            instance["flow_id"].as_str().unwrap().into()
        };
        Self { f, host, flow_id }
    }

    fn admit(&self, name: &str, repo: &str, body: Value) -> usize {
        let mut bodies = self.host.bodies.lock().unwrap();
        let key = body["event_key"]
            .as_str()
            .map_or_else(|| format!("{:064x}", bodies.len() + 1), str::to_owned);
        let bytes = body.to_string();
        let digest = sha256(&bytes);
        assert!(bodies.insert(key.clone(), bytes).is_none());
        drop(bodies);
        self.host.mock.set_clock(self.f.clock.now_ms() as f64);
        self.f
            .module
            .rt
            .admit_events(&[basal_core::events::Notice {
                published_at_ms: self.f.clock.now_ms(),
                subject: format!("ck.account.event.plexus.{name}.v2"),
                event_key: key,
                digest,
                headers: [("kind".into(), name.into()), ("repo".into(), repo.into())].into(),
            }])
            .unwrap()
            .admitted
    }

    fn run(&self, name: &str, repo: &str, body: Value) {
        assert_eq!(self.admit(name, repo, body), 1);
        let runs = self.f.module.rt.startable().unwrap();
        assert_eq!(runs.len(), 1, "{runs:?}");
        let run_id = &runs[0].0;
        self.f.module.engine.run_until_idle(100).unwrap();
        let run = self.f.module.rt.run(run_id).unwrap();
        assert_eq!(run.flow_id, self.flow_id);
        assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
        self.f.clock.advance(60_000);
    }

    fn writes(&self) -> Vec<Value> {
        self.host
            .mock
            .effects()
            .iter()
            .filter(|effect| effect.op == "sink.digest")
            .map(|effect| serde_json::from_str(&effect.args).unwrap())
            .collect()
    }
}

fn sample(fixture: &str) -> Value {
    serde_json::from_str(fixture).unwrap()
}

fn opened(fixture: &str) -> Value {
    let mut body = sample(fixture);
    body["action"] = json!("opened");
    for field in [
        "comment_author",
        "comment_excerpt",
        "comment_url",
        "event_key",
    ] {
        body[field] = Value::Null;
    }
    body
}

fn release() -> Value {
    json!({"repo": REPO, "kind": "release", "action": "published", "tag_name": "v1",
        "title": "Lanterns v1", "url": "https://github.com/fixture-org/lanterns/releases/tag/v1"})
}

#[test]
fn registers_and_instantiates_with_only_an_event_body_read_grant() {
    let t = Test::new(REPO, false);
    assert!(t.flow_id.starts_with("github-repo-events_"));
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    assert_eq!(
        manifest["ops"],
        json!([{"module":"plexus","op":"events_get"}])
    );
    assert_eq!(
        manifest["sinks"],
        json!([{"agent":"$self","digest_max":"wake"}])
    );
    assert_eq!(
        manifest["params"],
        json!({"repo":{"type":"string","max_bytes":200}})
    );
    assert_eq!(
        manifest["trigger"]["events"],
        json!([ISSUE, PR, RELEASE].map(|name| json!({
            "module":"plexus","name":name,"version":2,"match":{"repo":"$params.repo"},
            "max_age":"6h"
        })))
    );
}

#[test]
fn opened_issues_and_prs_wake_and_releases_are_announced_quietly() {
    let t = Test::new("Fixture-Org/Lanterns", false);
    t.run(ISSUE, REPO, opened(ISSUE_SAMPLE));
    t.run(PR, REPO, opened(PR_SAMPLE));
    t.run(RELEASE, REPO, release());
    let writes = t.writes();
    assert_eq!(writes.len(), 3);
    for (write, (kind, id, action, text)) in writes.iter().zip([
        (
            "issue",
            "3",
            "wake",
            "Issue #3 opened observed at 12:02Z. \"The lantern flickers\"",
        ),
        (
            "pull_request",
            "7",
            "wake",
            "PR #7 opened observed at 12:03Z. \"Fix the lantern timer\"",
        ),
        (
            "release",
            "v1",
            "piggyback",
            "Release #v1 update observed at 12:04Z. \"Lanterns v1\"",
        ),
    ]) {
        assert_eq!(write["agent"], OWNER);
        assert_eq!(write["action"], action);
        assert_eq!(write["replace_key"], format!("github:{REPO}:{kind}:{id}"));
        assert_eq!(write["item"]["body"], format!("- {REPO} {text}"));
    }
}

#[test]
fn plexus_comment_samples_wake_with_the_first_line_and_link() {
    let t = Test::new(REPO, false);
    t.run(ISSUE, REPO, sample(ISSUE_SAMPLE));
    t.run(PR, REPO, sample(PR_SAMPLE));
    let writes = t.writes();
    assert_eq!(writes.len(), 2);
    for (write, (line, url)) in writes.iter().zip([
        (
            "New comment by elliot at 12:02Z: \"I can reproduce this.\"",
            "https://github.com/fixture-org/lanterns/issues/3#issuecomment-1",
        ),
        (
            "New comment by mira at 12:03Z: \"Thanks for the fix!\"",
            "https://github.com/fixture-org/lanterns/pull/7#issuecomment-1",
        ),
    ]) {
        assert_eq!(write["action"], "wake");
        assert_eq!(
            write["item"]["body"].as_str().unwrap().lines().nth(1),
            Some(line)
        );
        assert_eq!(write["item"]["links"][1], json!({"kind":"url","url":url}));
    }
}

#[test]
fn another_repos_notice_is_filtered_before_admission() {
    let t = Test::new("fixture-org/other", false);
    assert_eq!(t.admit(ISSUE, REPO, sample(ISSUE_SAMPLE)), 0);
    assert!(t.f.module.rt.startable().unwrap().is_empty());
    assert!(t.writes().is_empty());
}

#[test]
fn a_matching_header_does_not_override_the_verified_bodys_repo() {
    let t = Test::new("fixture-org/other", false);
    t.run(ISSUE, "fixture-org/other", sample(ISSUE_SAMPLE));
    assert!(t.writes().is_empty());
}

#[test]
fn bot_comments_and_pushes_are_quiet() {
    let t = Test::new(REPO, false);
    let mut push = opened(PR_SAMPLE);
    push["action"] = json!("updated");
    t.run(PR, REPO, push);
    let mut bot = sample(PR_SAMPLE);
    bot["comment_author"] = json!("builder[BOT]");
    bot["actor_logins"] = json!(["builder[BOT]"]);
    t.run(PR, REPO, bot);
    let writes = t.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0]["action"], "piggyback");
    assert_eq!(writes[1]["action"], "piggyback");
    assert_eq!(writes[0]["replace_key"], writes[1]["replace_key"]);
    assert_eq!(
        writes[1]["item"]["body"],
        "- fixture-org/lanterns PR #7 updated at 12:02Z and 12:03Z: \"Fix the lantern timer\"\nNew comment by builder[BOT] at 12:03Z: \"Thanks for the fix!\""
    );
}

#[test]
fn marker_only_excerpt_hides_the_marker_but_keeps_author_and_time() {
    let t = Test::new(REPO, false);
    let mut comment = sample(ISSUE_SAMPLE);
    comment["comment_excerpt"] = json!(" <!-- hidden marker --> ");
    t.run(ISSUE, REPO, comment);
    let writes = t.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["action"], "wake");
    assert_eq!(
        writes[0]["item"]["body"],
        "- fixture-org/lanterns Issue #3 new comment observed at 12:02Z. \"The lantern flickers\"\nNew comment by elliot at 12:02Z."
    );
}

#[test]
fn portable_events_match_the_shipped_global_flows_digest_and_wake_decision() {
    let template = Test::new(FLEET_REPO, false);
    let global = Test::new(FLEET_REPO, true);
    let mut push = opened(PR_SAMPLE);
    push["action"] = json!("updated");
    let mut bot = sample(PR_SAMPLE);
    bot["event_key"] = Value::Null;
    bot["comment_author"] = json!("builder[bot]");
    bot["actor_logins"] = json!(["builder[bot]"]);
    let mut marker = sample(ISSUE_SAMPLE);
    marker["event_key"] = Value::Null;
    marker["comment_excerpt"] = json!("<!-- hidden marker cut short");
    let mut echo = sample(PR_SAMPLE);
    echo["event_key"] = Value::Null;
    echo["action"] = json!("suppressed_own_echo");
    let mut closed_without_actors = opened(PR_SAMPLE);
    closed_without_actors["action"] = json!("closed");
    closed_without_actors["state"] = json!("closed");
    closed_without_actors["actor_logins"] = json!([]);

    // The template doesn't look up the repository's own App login or have the
    // global flow's operator/project suppression lists. Compare cases that
    // don't depend on those fleet-specific identities.
    for (name, mut body) in [
        (ISSUE, opened(ISSUE_SAMPLE)),
        (PR, opened(PR_SAMPLE)),
        (ISSUE, sample(ISSUE_SAMPLE)),
        (PR, sample(PR_SAMPLE)),
        (PR, push),
        (PR, bot),
        (ISSUE, marker),
        (RELEASE, release()),
        (PR, echo),
        (PR, closed_without_actors),
    ] {
        body["repo"] = json!(FLEET_REPO);
        template.run(name, FLEET_REPO, body.clone());
        global.run(name, FLEET_REPO, body);
    }
    let expected = global.writes();
    let actual = template.writes();
    assert_eq!(expected.len(), 8);
    assert_eq!(actual.len(), expected.len());
    for (index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
        for field in ["agent", "item", "action", "replace_key"] {
            assert_eq!(actual[field], expected[field], "digest {index}: {field}");
        }
    }
}
