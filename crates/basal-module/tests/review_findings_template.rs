//! Exercise the published review-feedback template through registration,
//! instantiation, verified Plexus event bodies, and capture-only dry runs.
mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
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

const MANIFEST: &str = include_str!("../../../templates/review-findings/manifest.json");
const SCRIPT: &str = include_str!("../../../templates/review-findings/flow.js");
const OWNER: &str = "agent_47120287c700722b";
const REPO: &str = "example/project";
const COMMENT: &str = "github_pr_review_comment_created";
const REVIEW: &str = "github_pr_review_submitted";

fn text(value: Value) -> JsonText {
    JsonText::new(value.to_string()).unwrap()
}

fn bindings(handle: &str) -> HostOutcome {
    HostOutcome::fulfilled(text(json!({"result":{
        "generation": 7,
        "bindings": [
            {"repository":"other/project","agent_id":OWNER,"app_handle_id":"other-bot"},
            {"repository":REPO,"agent_id":OWNER,"app_handle_id":handle}
        ]
    }})))
}

#[derive(Default)]
struct ReviewHost {
    mock: MockHost,
    bodies: Mutex<BTreeMap<String, Value>>,
    bindings: Mutex<Option<HostOutcome>>,
    reads: AtomicUsize,
}

impl ReviewHost {
    fn answer(&self, request: &CallRequest) -> Option<HostOutcome> {
        let args: Value = serde_json::from_str(request.args.as_str()).unwrap();
        match &request.kind {
            CallKind::Op { module, op } if module == "plexus" && op == "events_get" => {
                let body =
                    self.bodies.lock().unwrap()[args["event_key"].as_str().unwrap()].to_string();
                Some(HostOutcome::fulfilled(text(json!({
                    "body":body, "digest":format!("{:x}", Sha256::digest(body.as_bytes())),
                    "event_key":args["event_key"], "event_name":args["event_name"]
                }))))
            }
            CallKind::Op { module, op } if module == "plexus" && op == "github_read" => {
                assert_eq!(args, json!({"action":"github.bindings.list"}));
                self.reads.fetch_add(1, Ordering::Relaxed);
                Some(
                    self.bindings
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| bindings("project-helper")),
                )
            }
            CallKind::Op { .. } | CallKind::OpFor { .. } => {
                panic!("unexpected review-template operation: {request:?}")
            }
            _ => None,
        }
    }
}

impl Host for ReviewHost {
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
    host: Arc<ReviewHost>,
}

impl Test {
    fn new() -> Self {
        let host = Arc::new(ReviewHost::default());
        let catalog = MockCatalog::standard();
        catalog.add_named_agent(OWNER, "Owner");
        for op in ["events_get", "github_read"] {
            // These queries cannot write to GitHub and trigger another review event.
            catalog.set_op(
                "plexus",
                op,
                OpDecl {
                    kind: Some(OpKind::Query),
                    cause_echo: false,
                    shell_capable: false,
                },
            );
        }
        for name in [COMMENT, REVIEW] {
            catalog.set_event(
                "plexus",
                name,
                1,
                EventDecl {
                    headers: vec!["repo".into()],
                    origin: EventOrigin::External,
                    body: EventBody::Resolved {
                        resolve_op: "events_get".into(),
                    },
                },
            );
        }
        let f = fixture(
            "review-findings-template",
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
        basal_core::test_support::seed_stored_version(&f.module.rt, SCRIPT, MANIFEST, true)
            .unwrap();
        let registered = f.module.rt.get_package("review-findings", 1).unwrap();
        assert_eq!(registered["package"], "review-findings");
        let stored = f.module.rt.get_package("review-findings", 1).unwrap();
        assert_eq!(stored["manifest"], MANIFEST);
        assert_eq!(stored["script"], SCRIPT);
        Self { f, host }
    }

    fn create(&self, params: Value) -> Result<Value, basal_module::ops::OpError> {
        self.f.module.handle(
            &agent(OWNER),
            "flow.instantiate",
            json!({"template":"review-findings","version":1,"params":params,"expires_in":"365d"}),
        )
    }

    fn instance(&self) -> Value {
        self.create(json!({"repo":REPO})).unwrap()
    }

    fn notice(&self, instance: &Value, name: &str, repo: &str, body: Value, age_ms: i64) {
        self.host.mock.set_clock(self.f.clock.now_ms() as f64);
        let key = format!("{:064x}", self.host.bodies.lock().unwrap().len() + 1);
        let bytes = body.to_string();
        self.host.bodies.lock().unwrap().insert(key.clone(), body);
        let report = self
            .f
            .module
            .rt
            .admit_events(&[basal_core::events::Notice {
                published_at_ms: self.f.clock.now_ms() - age_ms,
                subject: format!("ck.account.event.plexus.{name}.v1"),
                event_key: key,
                digest: format!("{:x}", Sha256::digest(bytes.as_bytes())),
                headers: [("repo".into(), repo.into())].into(),
            }])
            .unwrap();
        let expected =
            usize::from(repo == instance["params"]["repo"] && age_ms <= 6 * 60 * 60 * 1000);
        assert_eq!(report.admitted, expected);
        let startable = self.f.module.rt.startable().unwrap();
        assert_eq!(startable.len(), expected, "{startable:?}");
        for (run, _) in startable {
            assert_eq!(
                self.f.module.rt.run(&run).unwrap().flow_id,
                instance["flow_id"]
            );
            self.f.module.engine.run_until_idle(100).unwrap();
            let done = self.f.module.rt.run(&run).unwrap();
            assert_eq!(done.state, RunState::Succeeded, "{done:#?}");
        }
    }

    fn event(&self, instance: &Value, body: Value) {
        let name = if body["kind"] == "pull_request_review" {
            REVIEW
        } else {
            COMMENT
        };
        self.notice(instance, name, REPO, body, 0);
    }

    fn items(&self) -> Vec<Value> {
        self.host
            .mock
            .effects()
            .iter()
            .filter(|effect| effect.op == "sink.digest")
            .map(|effect| serde_json::from_str(&effect.args).unwrap())
            .collect()
    }

    fn one_item(&self) -> Value {
        let items = self.items();
        assert_eq!(items.len(), 1, "{items:#?}");
        assert_eq!(items[0]["agent"], OWNER);
        assert_eq!(items[0]["action"], "wake");
        items[0]["item"].clone()
    }
}

fn comment(login: &str) -> Value {
    json!({
        "kind":"pull_request_review_comment", "repo":REPO, "pr":42,
        "author_login":login, "author_is_bot":true,
        "path":"src/lib.rs", "line":17, "original_line":12,
        "comment_id":73, "head_sha":"0123456789abcdef",
        "body_excerpt":"<p>Fix the race</p>\n\n\nMore detail", "body_truncated":false,
        "comment_url":"https://github.com/example/project/pull/42#discussion_r73"
    })
}

fn review(login: &str, bot: bool, state: &str) -> Value {
    json!({
        "kind":"pull_request_review", "repo":REPO, "pr":42,
        "reviewer_login":login, "reviewer_is_bot":bot, "state":state,
        "review_id":99, "head_sha":"0123456789abcdef",
        "body_excerpt":"<p>Please fix this</p>", "body_truncated":false,
        "review_url":"https://github.com/example/project/pull/42#pullrequestreview-99"
    })
}

#[test]
fn manifest_has_one_bounded_parameter_and_self_only_event_grants() {
    let m: Value = serde_json::from_str(MANIFEST).unwrap();
    assert_eq!(
        m["params"],
        json!({"repo":{"type":"string","max_bytes":200}})
    );
    assert_eq!(
        m["template"],
        json!({"max_per_agent":100,"max_lifetime":"365d"})
    );
    assert_eq!(m["sinks"], json!([{"agent":"$self","digest_max":"wake"}]));
    assert_eq!(
        m["ops"],
        json!([
            {"module":"plexus","op":"events_get"},
            {"module":"plexus","op":"github_read"}
        ])
    );
    assert_eq!(
        m["trigger"],
        json!({"events":[
            {"module":"plexus","name":COMMENT,"version":1,"match":{"repo":"$params.repo"},"max_age":"6h"},
            {"module":"plexus","name":REVIEW,"version":1,"match":{"repo":"$params.repo"},"max_age":"6h"}
        ]})
    );
}

#[test]
fn inline_bot_comment_wakes_with_pr_file_line_finding_and_link() {
    let t = Test::new();
    let instance = t.instance();
    t.event(&instance, comment("greptile[bot]"));
    let item = t.one_item();
    assert_eq!(item["title"], "PR #42 review comment: src/lib.rs:17");
    assert_eq!(
        item["body"],
        "greptile[bot] on head 0123456789ab:\n\nFix the race\n\nMore detail"
    );
    assert_eq!(
        item["data"],
        json!({
            "pr":42,"path":"src/lib.rs","line":17,"comment_id":73,"head_sha":"0123456789abcdef"
        })
    );
    assert_eq!(
        item["links"],
        json!([{
            "kind":"url","url":"https://github.com/example/project/pull/42#discussion_r73"
        }])
    );
    assert!(
        t.f.module.rt.live_instances().unwrap()["instances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["flow_id"] == instance["flow_id"])
    );
}

#[test]
fn outdated_comment_uses_original_line_and_marks_it_outdated() {
    let t = Test::new();
    let instance = t.instance();
    let mut body = comment("greptile[bot]");
    body["line"] = Value::Null;
    t.event(&instance, body);
    assert_eq!(
        t.one_item()["title"],
        "PR #42 review comment: src/lib.rs:12 (outdated)"
    );
}

#[test]
fn repository_own_bot_from_bindings_is_skipped() {
    let t = Test::new();
    let instance = t.instance();
    t.event(&instance, comment("PROJECT-HELPER[bot]"));
    t.event(
        &instance,
        review("project-helper[bot]", true, "changes_requested"),
    );
    assert!(
        t.items().is_empty(),
        "repository bot must not wake its own head"
    );
    t.event(&instance, comment("other-bot[bot]"));
    assert!(
        t.one_item()["body"]
            .as_str()
            .unwrap()
            .starts_with("other-bot[bot]")
    );
    assert_eq!(t.host.reads.load(Ordering::Relaxed), 1);
}

#[test]
fn bot_summary_reviews_are_skipped_unless_requesting_changes() {
    let t = Test::new();
    let instance = t.instance();
    for state in ["commented", "approved", "dismissed"] {
        t.event(&instance, review("greptile[bot]", true, state));
    }
    assert!(t.items().is_empty());
    t.event(
        &instance,
        review("greptile[bot]", true, "changes_requested"),
    );
    let item = t.one_item();
    assert_eq!(
        item["title"],
        "PR #42 review changes_requested by greptile[bot]"
    );
    assert_eq!(item["body"], "Head 0123456789ab:\n\nPlease fix this");
    assert_eq!(item["data"]["review_id"], 99);
    assert_eq!(
        item["links"][0]["url"],
        "https://github.com/example/project/pull/42#pullrequestreview-99"
    );
}

#[test]
fn person_review_wakes_even_without_a_review_body() {
    let t = Test::new();
    let instance = t.instance();
    let mut body = review("reviewer", false, "approved");
    body["body_excerpt"] = json!("<p></p>");
    body["body_truncated"] = json!(true);
    t.event(&instance, body);
    let item = t.one_item();
    assert_eq!(item["title"], "PR #42 review approved by reviewer");
    assert_eq!(
        item["body"],
        "Head 0123456789ab:\n\n(no review body)\n\n(truncated)"
    );
}

#[test]
fn replies_keep_person_marker_truncation_marker_and_general_location() {
    let t = Test::new();
    let instance = t.instance();
    let mut body = comment("reviewer");
    body["author_is_bot"] = json!(false);
    body["path"] = Value::Null;
    body["in_reply_to_id"] = json!(72);
    body["body_truncated"] = json!(true);
    body["comment_url"] = Value::Null;
    t.event(&instance, body);
    let item = t.one_item();
    assert_eq!(item["title"], "PR #42 review comment (reply): general");
    assert_eq!(
        item["body"],
        "reviewer (person) on head 0123456789ab:\n\nFix the race\n\nMore detail\n\n(truncated)"
    );
    assert_eq!(item["links"], json!([]));
}

#[test]
fn unreadable_or_missing_bindings_deliver_and_skip_only_alfonso_bots() {
    for outcome in [
        HostOutcome::fulfilled(text(json!({"error":{"code":"store_failure"}}))),
        HostOutcome::rejected(text(json!({"code":"transport_failed"}))),
        HostOutcome::fulfilled(text(json!({"result":{"generation":7}}))),
        HostOutcome::fulfilled(text(json!({"result":{"generation":7,"bindings":[]}}))),
    ] {
        let t = Test::new();
        *t.host.bindings.lock().unwrap() = Some(outcome);
        let instance = t.instance();
        t.event(&instance, comment("project-alfonso[bot]"));
        t.event(
            &instance,
            review("PROJECT-ALFONSO[bot]", true, "changes_requested"),
        );
        assert!(t.items().is_empty());
        t.event(&instance, comment("project-helper[bot]"));
        t.event(&instance, review("reviewer", false, "commented"));
        let items = t.items();
        assert_eq!(items.len(), 2, "{items:#?}");
        assert_eq!(items[0]["action"], "wake");
        assert!(
            items[0]["item"]["body"]
                .as_str()
                .unwrap()
                .starts_with("project-helper[bot]")
        );
        assert_eq!(
            items[1]["item"]["title"],
            "PR #42 review commented by reviewer"
        );
        assert_eq!(t.host.reads.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn bindings_cache_refreshes_at_ten_minutes_and_failed_refresh_falls_back() {
    let t = Test::new();
    let instance = t.instance();
    t.event(&instance, comment("project-helper[bot]"));
    *t.host.bindings.lock().unwrap() = Some(bindings("replacement"));
    t.f.clock.advance(599_999);
    t.event(&instance, comment("project-helper[bot]"));
    assert_eq!(t.host.reads.load(Ordering::Relaxed), 1);
    t.f.clock.advance(1);
    t.event(&instance, comment("replacement[bot]"));
    assert_eq!(t.host.reads.load(Ordering::Relaxed), 2);
    assert!(t.items().is_empty());

    *t.host.bindings.lock().unwrap() = Some(HostOutcome::fulfilled(text(
        json!({"error":{"code":"unavailable"}}),
    )));
    t.f.clock.advance(600_000);
    t.event(&instance, comment("replacement[bot]"));
    assert_eq!(
        t.one_item()["title"],
        "PR #42 review comment: src/lib.rs:17"
    );
    t.event(&instance, comment("project-alfonso[bot]"));
    assert_eq!(t.items().len(), 1);
    assert_eq!(t.host.reads.load(Ordering::Relaxed), 3);
}

#[test]
fn another_repository_or_mismatched_verified_body_is_ignored() {
    let t = Test::new();
    let instance = t.instance();
    let mut body = comment("greptile[bot]");
    body["repo"] = json!("other/project");
    t.notice(&instance, COMMENT, "other/project", body.clone(), 0);
    t.notice(&instance, COMMENT, REPO, body, 0);
    assert!(t.items().is_empty());
    assert_eq!(t.host.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn six_hour_old_backlog_is_not_admitted() {
    let t = Test::new();
    let instance = t.instance();
    t.notice(
        &instance,
        COMMENT,
        REPO,
        comment("greptile[bot]"),
        6 * 60 * 60 * 1000 + 1,
    );
    t.notice(
        &instance,
        REVIEW,
        REPO,
        review("reviewer", false, "commented"),
        6 * 60 * 60 * 1000 + 1,
    );
    assert!(t.items().is_empty());
    assert_eq!(t.host.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn instantiation_validates_required_repo_type_and_bounds() {
    let t = Test::new();
    for (params, code) in [
        (json!({}), "param_missing"),
        (json!({"repo":42}), "param_out_of_bounds"),
        (json!({"repo":"x".repeat(201)}), "param_out_of_bounds"),
        (json!({"repo":"example/\nproject"}), "param_out_of_bounds"),
        (json!({"repo":""}), "param_out_of_bounds"),
        (json!({"repo":REPO,"bot":"helper"}), "param_unknown"),
    ] {
        assert_eq!(t.create(params.clone()).unwrap_err().code, code, "{params}");
    }
    assert_eq!(t.instance()["params"], json!({"repo":REPO}));
}

#[test]
fn script_ignores_non_owner_name_repo_patterns() {
    let t = Test::new();
    let instance = t.create(json!({"repo":"example/*"})).unwrap();
    let mut body = comment("greptile[bot]");
    body["repo"] = json!("example/*");
    t.notice(&instance, COMMENT, "example/*", body, 0);
    assert!(t.items().is_empty());
    assert_eq!(t.host.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn normal_validation_and_capture_dry_run_accept_the_template() {
    let t = Test::new();
    let mut intake: Value = serde_json::from_str(&common::authored_manifest(MANIFEST)).unwrap();
    intake["id"] = json!("review-findings-intake");
    let registered =
        t.f.module
            .handle(
                &Caller::Operator,
                "package.register",
                json!({"manifest":intake.to_string(),"script":SCRIPT}),
            )
            .unwrap();
    assert_eq!(registered["package"], "review-findings-intake");
    let sends = t.host.mock.total_sends();
    let reply =
        t.f.module
            .handle(
                &agent(OWNER),
                "flow.dry_run",
                json!({
                    "flow_id":"review-findings","params":{"repo":REPO},
                    "trigger":{"event":{"module":"plexus","name":COMMENT,"version":1,
                        "headers":{"repo":REPO},"body":comment("greptile[bot]")}}
                }),
            )
            .unwrap();
    assert_eq!(reply["summary"]["mode"], "capture");
    assert_eq!(
        reply["summary"]["runs"][0]["state"], "succeeded",
        "{reply:#}"
    );
    assert!(
        reply
            .to_string()
            .contains("PR #42 review comment: src/lib.rs:17"),
        "{reply:#}"
    );
    assert_eq!(t.host.mock.total_sends(), sends);
    assert!(t.items().is_empty());
    println!(
        "review-findings code_hash={}",
        basal_core::ids::hex(&basal_core::ids::code_hash(SCRIPT, MANIFEST))
    );
}
