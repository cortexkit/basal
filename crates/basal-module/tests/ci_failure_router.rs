//! Execute the shipped CI failure router with real workers and stored deduplication state.
//! Synthetic Plexus notices resolve to digest-verified bodies before owner routing.
mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

const MANIFEST: &str = include_str!("../../../flows/ci-failure-router/manifest.json");
const SCRIPT: &str = include_str!("../../../flows/ci-failure-router/flow.js");
const BASAL: &str = "agent_12eb4c0c42cecdb5";
const PLEX: &str = "agent_309edd5e94fbb2fe";

#[derive(Default)]
struct CiHost {
    mock: MockHost,
    bodies: Mutex<BTreeMap<String, Value>>,
    writes: Mutex<Vec<Value>>,
    resolved: AtomicUsize,
    refuse: AtomicBool,
    reads: Mutex<Vec<Value>>,
    supersession: Mutex<Option<HostOutcome>>,
}

fn text(value: Value) -> JsonText {
    JsonText::new(value.to_string()).unwrap()
}

fn supersession_reply(repository: &str, run_id: u64, superseded: bool) -> HostOutcome {
    let newer = superseded.then(|| {
        json!({
            "id":run_id + 1, "run_number":8, "status":"queued", "conclusion":null,
            "created_at":"2027-01-01T12:01:00Z",
            "url":format!("https://github.com/{repository}/actions/runs/{}", run_id + 1)
        })
    });
    HostOutcome::fulfilled(text(json!({
        "result":{
            "repository":repository, "superseded":superseded,
            "run":{"id":run_id, "run_number":7, "status":"completed", "conclusion":"failure",
                "created_at":"2027-01-01T12:00:00Z"},
            "newer":newer
        }
    })))
}

impl CiHost {
    fn answer(&self, request: &CallRequest) -> Option<HostOutcome> {
        let args: Value = serde_json::from_str(request.args.as_str()).unwrap();
        if request.kind
            == (CallKind::Op {
                module: "plexus".into(),
                op: "github_read".into(),
            })
        {
            assert_eq!(args["action"], "github.actions.run.superseded");
            assert_eq!(args.as_object().unwrap().len(), 3);
            let repository = args["repository"].as_str().unwrap().to_owned();
            assert!(repository.starts_with("cortexkit/"));
            assert_eq!(repository, repository.to_ascii_lowercase());
            assert_eq!(args["args"].as_object().unwrap().len(), 1);
            let run_id = args["args"]["run_id"].as_u64().unwrap();
            assert!(run_id > 0);
            self.reads.lock().unwrap().push(args);
            return Some(
                self.supersession
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| supersession_reply(&repository, run_id, false)),
            );
        }
        if request.kind == CallKind::Primitive(Primitive::SinkDigest) {
            let item = &args["item"];
            assert!(item["title"].as_str().unwrap().chars().count() <= 80);
            assert!(item["body"].as_str().unwrap().len() <= 4096);
            assert!(serde_json::to_vec(&item["data"]).unwrap().len() <= 8192);
            assert!(serde_json::to_vec(item).unwrap().len() <= 16384);
            assert!(item["links"].as_array().unwrap().len() <= 8);
            self.writes.lock().unwrap().push(args);
            if self.refuse.swap(false, Ordering::Relaxed) {
                return Some(HostOutcome::rejected(text(json!({
                    "code":"sink_target_not_in_audience", "message":"unknown agent"
                }))));
            }
            return Some(HostOutcome::fulfilled(text(json!({"ok":true}))));
        }
        if request.kind
            == (CallKind::Op {
                module: "plexus".into(),
                op: "events_get".into(),
            })
        {
            let body = self.bodies.lock().unwrap()[args["event_key"].as_str().unwrap()].to_string();
            self.resolved.fetch_add(1, Ordering::Relaxed);
            return Some(HostOutcome::fulfilled(text(json!({
                "body":body, "digest":format!("{:x}", Sha256::digest(body.as_bytes())),
                "event_key":args["event_key"], "event_name":args["event_name"]
            }))));
        }
        None
    }
}

impl Host for CiHost {
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
    host: Arc<CiHost>,
}
impl Test {
    fn new() -> Self {
        let host = Arc::new(CiHost::default());
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
        catalog.set_op(
            "plexus",
            "github_read",
            OpDecl {
                kind: Some(OpKind::Query),
                cause_echo: false,
                shell_capable: false,
            },
        );
        for name in ["github_ci_job_failed", "github_ci_run_completed"] {
            catalog.set_event(
                "plexus",
                name,
                2,
                EventDecl {
                    headers: ["kind", "repo", "project", "pr", "branch", "conclusion"]
                        .map(str::to_owned)
                        .into(),
                    origin: EventOrigin::External,
                    body: EventBody::Resolved {
                        resolve_op: "events_get".into(),
                    },
                },
            );
        }
        let consent = MockConsent::new();
        let f = common::fixture(
            "ci-failure-router",
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
        assert!(consent.decide(
            reply["card_id"].as_str().unwrap(),
            basal_host::CardDecision::Approve,
            "operator"
        ));
        Self { f, host }
    }
    fn notice(&self, mut body: Value, repo: &str, branch: &str, admitted: bool) -> Option<String> {
        let name = if body["kind"] == "ci_job" {
            "github_ci_job_failed"
        } else {
            "github_ci_run_completed"
        };
        let key = format!("{:064x}", self.host.bodies.lock().unwrap().len() + 1);
        body["event_key"] = json!(key);
        let bytes = body.to_string();
        let conclusion = body["conclusion"].as_str().unwrap_or("").to_owned();
        self.host.bodies.lock().unwrap().insert(key.clone(), body);
        let resolved = self.host.resolved.load(Ordering::Relaxed);
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
                    ("repo".into(), repo.into()),
                    ("branch".into(), branch.into()),
                    ("conclusion".into(), conclusion),
                ]
                .into(),
            }])
            .unwrap();
        let startable = self.f.module.rt.startable().unwrap();
        if !admitted {
            assert_eq!((report.filtered_out, report.admitted), (1, 0));
            assert!(startable.is_empty());
            assert_eq!(self.host.resolved.load(Ordering::Relaxed), resolved);
            return None;
        }
        assert_eq!(report.admitted, 1);
        assert_eq!(startable.len(), 1);
        let run = startable[0].0.clone();
        self.f.module.engine.run_until_idle(1000).unwrap();
        let done = self.f.module.rt.run(&run).unwrap();
        assert_eq!(done.state, RunState::Succeeded, "{done:#?}");
        Some(run)
    }
    fn ci(&self, body: Value) -> String {
        let repo = body["repo"].as_str().unwrap().to_owned();
        let branch = body["head_branch"].as_str().unwrap_or("").to_owned();
        self.notice(body, &repo, &branch, true).unwrap()
    }
    fn writes(&self) -> Vec<Value> {
        self.host.writes.lock().unwrap().clone()
    }
    fn reads(&self) -> Vec<Value> {
        self.host.reads.lock().unwrap().clone()
    }
    fn supersession(&self, outcome: HostOutcome) {
        *self.host.supersession.lock().unwrap() = Some(outcome);
    }
    fn memory(&self) -> Value {
        let stored: String = self
            .f
            .module
            .rt
            .store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT value FROM kv WHERE flow_id='ci-failure-router' AND key='ci-router'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        serde_json::from_str(&stored).unwrap()
    }
}

fn job(repo: &str, id: u64) -> Value {
    let mut body: Value =
        serde_json::from_str(include_str!("data/template-ci-job-v2.json")).unwrap();
    body["repo"] = json!(repo);
    body["id"] = json!(id);
    body["name"] = json!(format!("linux shard {id}"));
    body["url"] = json!(format!(
        "https://github.com/{repo}/actions/runs/42/job/{id}"
    ));
    body
}
fn run(repo: &str, conclusion: &str) -> Value {
    let mut body: Value =
        serde_json::from_str(include_str!("data/template-ci-run-v2.json")).unwrap();
    body["repo"] = json!(repo);
    body["conclusion"] = json!(conclusion);
    body["url"] = json!(format!("https://github.com/{repo}/actions/runs/42"));
    body
}

fn completed_at(t: &Test, age_ms: i64) -> String {
    jiff::Timestamp::from_millisecond(t.host.now_ms() as i64 - age_ms)
        .unwrap()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[test]
fn published_manifest_is_global_with_only_ci_v2_repo_filters_and_minimal_grants() {
    Manifest::parse(MANIFEST).unwrap();
    let value: Value = serde_json::from_str(MANIFEST).unwrap();
    assert_eq!(value["audience"], json!({"kind":"global"}));
    assert_eq!(value["version"], 13);
    assert!(
        value["purpose"]
            .as_str()
            .unwrap()
            .contains("cortexkit/cerebellum")
    );
    assert!(
        value["purpose"]
            .as_str()
            .unwrap()
            .contains("CI off by owner decision")
    );
    assert_eq!(
        value["sinks"],
        json!([{"agent":"$audience", "digest_max":"wake"}])
    );
    assert_eq!(
        value["ops"],
        json!([{"module":"plexus", "op":"events_get"}, {"module":"plexus", "op":"github_read"}])
    );
    assert_eq!(
        value["trigger"]["events"],
        json!([
            {"module":"plexus", "name":"github_ci_job_failed", "version":2, "match":{"repo":"cortexkit/*"}, "max_age":"6h"},
            {"module":"plexus", "name":"github_ci_run_completed", "version":2, "match":{"repo":"cortexkit/*"}, "max_age":"6h"}
        ])
    );
    assert_eq!(value["deadline"], "2m");
    assert!(value.get("params").is_none());
    assert!(value.get("template").is_none());
}

#[test]
fn two_repos_route_to_distinct_stable_owners() {
    let t = Test::new();
    t.ci(job("CortexKit/Basal", 101));
    t.ci(job("cortexkit/plexus", 101));
    let writes = t.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0]["agent"], BASAL);
    assert_eq!(writes[1]["agent"], PLEX);
    assert_eq!(writes[0]["replace_key"], "ci:cortexkit/basal:42:1");
    assert_eq!(writes[1]["replace_key"], "ci:cortexkit/plexus:42:1");
    assert_eq!(t.memory()["runs"].as_object().unwrap().len(), 2);
}

#[test]
fn unmapped_repos_write_nothing_without_a_fallback() {
    let t = Test::new();
    for repo in [
        "cortexkit/opencode-interceptor",
        "cortexkit/orw",
        "cortexkit/tree-sitter-scss",
        "cortexkit/new-repo",
    ] {
        t.ci(job(repo, 101));
        t.ci(run(repo, "startup_failure"));
    }
    assert!(t.writes().is_empty());
    assert_eq!(t.memory()["unmapped"], 8);
    assert!(t.memory()["runs"].as_object().unwrap().is_empty());
}

#[test]
fn ignored_workflow_run_is_dropped() {
    let t = Test::new();
    let mut body = run("cortexkit/aft", "failure");
    body["name"] = json!("Design gate");
    t.ci(body);
    assert!(t.writes().is_empty());
    assert!(t.memory()["runs"].as_object().unwrap().is_empty());
}

#[test]
fn magic_context_design_gate_is_ignored_off_master_and_train_branches() {
    // A contributor PR blocked by the design gate is expected; the same gate
    // failing on master or a train branch is real breakage and still wakes.
    for (branch, wakes) in [
        ("fix/status-threshold-model-fallback", false),
        ("master", true),
        ("train/2026-10", true),
    ] {
        let t = Test::new();
        let mut failed = job("cortexkit/magic-context", 101);
        failed["workflow_name"] = json!("design-gate");
        failed["name"] = json!("design-gate / design-gate");
        failed["head_branch"] = json!(branch);
        t.ci(failed);
        let mut finished = run("cortexkit/magic-context", "failure");
        finished["name"] = json!("design-gate");
        finished["head_branch"] = json!(branch);
        t.ci(finished);
        let wakes_seen = t.writes().iter().filter(|w| w["action"] == "wake").count();
        assert_eq!(wakes_seen > 0, wakes, "{branch}");
        if !wakes {
            assert!(t.writes().is_empty(), "{branch}");
        }
    }
}

#[test]
fn magic_context_design_gate_job_in_another_workflow_is_ignored_off_master() {
    // The reusable gate also reports as a job inside other workflows.
    let t = Test::new();
    // bench/* is a magic-context wake branch where the design gate is still
    // ignored, so the other job on it wakes while the gate job sends nothing.
    let mut failed = job("cortexkit/magic-context", 101);
    failed["workflow_name"] = json!("CI");
    failed["name"] = json!("design-gate");
    failed["head_branch"] = json!("bench/fix");
    t.ci(failed);
    assert!(t.writes().is_empty());
    let mut other = job("cortexkit/magic-context", 102);
    other["workflow_name"] = json!("CI");
    other["head_branch"] = json!("bench/fix");
    t.ci(other);
    assert_eq!(t.writes()[0]["action"], "wake");
}

#[test]
fn ignored_workflow_job_with_workflow_name_is_dropped() {
    let t = Test::new();
    let mut body = job("cortexkit/aft", 101);
    body["workflow_name"] = json!("Design gate");
    t.ci(body);
    assert!(t.writes().is_empty());
    assert!(t.memory()["runs"].as_object().unwrap().is_empty());
}

#[test]
fn job_without_workflow_name_from_ignored_repo_still_wakes() {
    let t = Test::new();
    let body = job("cortexkit/aft", 101);
    assert!(body.get("workflow_name").is_none());
    t.ci(body);
    assert_eq!(t.writes().len(), 1);
    assert_eq!(t.writes()[0]["action"], "wake");
    assert_eq!(t.writes()[0]["agent"], "agent_ab64ea0873bbee0c");
}

#[test]
fn quiet_repo_updates_silently_while_other_repos_still_wake() {
    let t = Test::new();
    t.ci(job("cortexkit/cerebellum", 101));
    let writes = t.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["action"], "silent");
    assert_eq!(writes[0]["agent"], "agent_b1a9c546c33b84c9");
    assert_eq!(writes[0]["replace_key"], "ci:cortexkit/cerebellum:42:1");
    assert!(
        writes[0]["item"]["body"]
            .as_str()
            .unwrap()
            .contains("Failed jobs: linux shard 101")
    );
    assert_eq!(
        t.memory()["runs"]["ci:cortexkit/cerebellum:42:1"]["jobs"]
            .as_object()
            .unwrap()
            .len(),
        1
    );

    t.ci(job("cortexkit/basal", 102));
    let writes = t.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[1]["action"], "wake");
    assert_eq!(writes[1]["agent"], BASAL);
}

#[test]
fn stale_job_completion_sends_nothing_and_creates_no_run_entry() {
    let t = Test::new();
    let mut body = job("cortexkit/basal", 101);
    body["completed_at"] = json!(completed_at(&t, 7 * common::HOUR));
    t.ci(body);

    assert!(t.writes().is_empty());
    assert!(t.memory()["runs"].as_object().unwrap().is_empty());
}

#[test]
fn recent_job_completion_wakes() {
    let t = Test::new();
    let mut body = job("cortexkit/basal", 101);
    body["completed_at"] = json!(completed_at(&t, 30 * 60 * 1000));
    t.ci(body);

    let writes = t.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["action"], "wake");
}

#[test]
fn job_without_completion_timestamp_still_wakes() {
    let t = Test::new();
    let mut body = job("cortexkit/basal", 101);
    body.as_object_mut().unwrap().remove("completed_at");
    t.ci(body);

    let writes = t.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["action"], "wake");
}

#[test]
fn stale_run_completion_sends_nothing() {
    let t = Test::new();
    let mut body = run("cortexkit/basal", "failure");
    body["completed_at"] = json!(completed_at(&t, 7 * common::HOUR));
    t.ci(body);

    assert!(t.writes().is_empty());
}

#[test]
fn cancelled_run_without_failed_jobs_sends_nothing() {
    let t = Test::new();
    t.ci(run("cortexkit/basal", "cancelled"));
    assert!(t.writes().is_empty());
    assert!(t.memory()["runs"].as_object().unwrap().is_empty());
}

#[test]
fn cancelled_run_after_a_failure_updates_silently() {
    let t = Test::new();
    t.ci(job("cortexkit/basal", 101));
    t.ci(run("cortexkit/basal", "cancelled"));
    let writes = t.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0]["action"], "wake");
    assert_eq!(writes[1]["action"], "silent");
    assert_eq!(writes[1]["replace_key"], "ci:cortexkit/basal:42:1");
}

#[test]
fn first_failed_job_wakes_and_later_jobs_are_silent() {
    let t = Test::new();
    for id in 101..=107 {
        t.ci(job("cortexkit/basal", id));
        let writes = t.writes();
        assert_eq!(writes.len(), (id - 100) as usize);
        assert_eq!(
            writes.last().unwrap()["action"],
            if id == 101 { "wake" } else { "silent" }
        );
        assert_eq!(
            writes.last().unwrap()["replace_key"],
            "ci:cortexkit/basal:42:1"
        );
        assert!(
            writes.last().unwrap()["item"]["data"]["observed"]["failed_jobs"]
                .as_str()
                .unwrap()
                .contains(&format!("linux shard {id}"))
        );
    }
    let run = &t.memory()["runs"]["ci:cortexkit/basal:42:1"];
    assert_eq!(run["firstWakeJobIds"], json!(["101"]));
    assert_eq!(run["jobs"].as_object().unwrap().len(), 7);
}

#[test]
fn digest_body_never_repeats_title_or_reports_unavailable_test_names() {
    let failed = Test::new();
    failed.ci(job("cortexkit/basal", 101));
    let failed_item = failed.writes()[0]["item"].clone();

    let green = Test::new();
    green.ci(job("cortexkit/basal", 101));
    green.ci(run("cortexkit/basal", "success"));
    let green_writes = green.writes();
    let green_item = green_writes.last().unwrap()["item"].clone();

    let red_without_jobs = Test::new();
    red_without_jobs.ci(run("cortexkit/basal", "failure"));
    let red_item = red_without_jobs.writes()[0]["item"].clone();

    for item in [&failed_item, &green_item, &red_item] {
        let title = item["title"].as_str().unwrap();
        let body = item["body"].as_str().unwrap();
        assert!(!body.contains(title), "{body}");
        assert!(!body.contains("test names unavailable"), "{body}");
    }

    assert_eq!(failed_item["title"], "cortexkit/basal: CI failed on main");
    assert_eq!(
        failed_item["body"],
        "Failed jobs: linux shard 101\nRun 42, attempt 1, jobs arriving\nCommit 0123456789ab"
    );
    assert_eq!(
        green_item["title"],
        "cortexkit/basal: CI green again on main"
    );
    assert_eq!(
        green_item["body"],
        "Previously failed: linux shard 101\nRun 42, attempt 1, finished green (success)\nCommit 0123456789ab"
    );
    assert_eq!(
        red_item["title"],
        "cortexkit/basal: CI run on main ended failure"
    );
    assert_eq!(
        red_item["body"],
        "Failed jobs not reported yet\nRun 42, attempt 1, finished red (failure)\nCommit 0123456789ab"
    );
}

#[test]
fn digest_links_put_the_run_first_and_keep_at_most_three_failed_jobs() {
    let t = Test::new();
    for id in 101..=108 {
        t.ci(job("cortexkit/basal", id));
    }
    t.ci(run("cortexkit/basal", "failure"));

    let writes = t.writes();
    let links = writes.last().unwrap()["item"]["links"].as_array().unwrap();
    assert_eq!(links.len(), 4);
    assert_eq!(
        links[0]["url"],
        "https://github.com/cortexkit/basal/actions/runs/42"
    );
    for (link, id) in links[1..].iter().zip(101..=103) {
        assert_eq!(
            link["url"],
            format!("https://github.com/cortexkit/basal/actions/runs/42/job/{id}")
        );
    }
}

#[test]
fn oversized_failed_job_digest_stays_within_core_item_limits() {
    let t = Test::new();
    let mut event = job("cortexkit/basal", 108);
    let url = format!(
        "https://github.com/cortexkit/basal/actions/runs/42/job/{}",
        "x".repeat(900)
    );
    event["name"] = json!("é🦀".repeat(8000));
    event["url"] = json!(url);
    event["head_branch"] = json!(format!("main-{}", "b".repeat(12000)));
    event["head_sha"] = json!("a".repeat(12000));

    t.ci(event);

    let item = &t.writes()[0]["item"];
    assert!(item["title"].as_str().unwrap().chars().count() <= 80);
    assert!(item["title"].as_str().unwrap().ends_with('…'));
    assert!(item["body"].as_str().unwrap().len() <= 4096);
    assert!(serde_json::to_vec(&item["data"]).unwrap().len() <= 8192);
    assert!(serde_json::to_vec(item).unwrap().len() <= 16 * 1024);
    assert!(
        item["data"]["observed"]["failed_jobs"]
            .as_str()
            .unwrap()
            .len()
            <= 256
    );
    assert_eq!(
        item["data"]["truncated"]["link_url"]["original_bytes"],
        json!(url.len())
    );
    assert_eq!(
        item["data"]["truncated"]["observed.branch"]["original_bytes"],
        json!(12005)
    );
    assert_eq!(
        item["data"]["truncated"]["observed.head_sha"]["original_bytes"],
        json!(12000)
    );
}

#[test]
fn tests_failures_on_land_branches_in_aft_still_wake() {
    let t = Test::new();
    let mut body = job("cortexkit/aft", 101);
    body["name"] = json!("Tests");
    body["head_branch"] = json!("land/123");
    t.ci(body);
    let mut merge_group = job("cortexkit/aft", 102);
    merge_group["run_id"] = json!(43);
    merge_group["name"] = json!("Tests");
    merge_group["head_branch"] = json!("gh-readonly-queue/main/pr-123-1");
    t.ci(merge_group);
    let writes = t.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0]["action"], "wake");
    assert_eq!(writes[0]["agent"], "agent_ab64ea0873bbee0c");
    assert_eq!(writes[1]["action"], "wake");
    assert_eq!(writes[1]["agent"], "agent_ab64ea0873bbee0c");
}

#[test]
fn contributor_branch_failures_record_silently_and_seat_branches_wake() {
    // A contributor's branch, such as a draft held by a design gate, is the
    // contributor's to fix: its failure is recorded without waking the owner.
    let contributor = |id: u64, tweak: &dyn Fn(&mut Value)| {
        let mut body = job("cortexkit/claustrum", id);
        body["run_id"] = json!(id);
        body["head_branch"] = json!("feat/opencode-v2-plugin");
        tweak(&mut body);
        body
    };
    type Case<'a> = (&'a str, &'a dyn Fn(&mut Value), &'a str);
    let cases: [Case; 9] = [
        (
            "contributor push",
            &|b| b["actor"] = json!("some-contributor"),
            "silent",
        ),
        (
            "operator re-ran a contributor's run",
            &|b| {
                b["actor"] = json!("some-contributor");
                b["triggering_actor"] = json!("ualtinok");
            },
            "silent",
        ),
        (
            "fork pushed under an operator-like login",
            &|b| b["head_repo_is_fork"] = json!(true),
            "silent",
        ),
        (
            "unknown fork",
            &|b| b["head_repo_is_fork"] = Value::Null,
            "silent",
        ),
        ("unknown actor", &|b| b["actor"] = Value::Null, "silent"),
        ("operator push", &|_| {}, "wake"),
        (
            "operator push, other case",
            &|b| b["actor"] = json!("UAltinok"),
            "wake",
        ),
        (
            "seat bot push",
            &|b| b["actor"] = json!("ckcred-alfonso[bot]"),
            "wake",
        ),
        (
            "third-party bot",
            &|b| b["actor"] = json!("dependabot[bot]"),
            "silent",
        ),
    ];
    for (id, (name, tweak, action)) in (201..).zip(cases) {
        let t = Test::new();
        t.ci(contributor(id, tweak));
        let writes = t.writes();
        assert_eq!(writes.len(), 1, "{name}");
        assert_eq!(writes[0]["action"], action, "{name}");
    }
}

#[test]
fn a_repository_with_wake_branches_wakes_only_on_those_branches() {
    // magic-context wakes only on master, train/* and bench/*, even for a branch
    // the operator pushed; other repositories keep the general rule.
    for (id, repo, branch, action) in [
        (401, "cortexkit/magic-context", "master", "wake"),
        (402, "cortexkit/magic-context", "train/2026-10", "wake"),
        (403, "cortexkit/magic-context", "bench/levels", "wake"),
        (
            404,
            "cortexkit/magic-context",
            "feat/operator-branch",
            "silent",
        ),
        (
            405,
            "cortexkit/magic-context",
            "gh-readonly-queue/master/pr-1",
            "silent",
        ),
        (406, "cortexkit/aft", "feat/operator-branch", "wake"),
    ] {
        let t = Test::new();
        let mut body = job(repo, id);
        body["run_id"] = json!(id);
        body["head_branch"] = json!(branch);
        body["actor"] = json!("ualtinok");
        body["head_repo_is_fork"] = json!(false);
        t.ci(body);
        assert_eq!(t.writes()[0]["action"], action, "{repo} {branch}");
    }
}

#[test]
fn protected_branches_wake_whoever_started_the_run() {
    for (id, branch) in (301..).zip([
        "main",
        "gh-readonly-queue/main/pr-9-1",
        "train/2026-10",
        "release/0.4",
    ]) {
        let t = Test::new();
        let mut body = job("cortexkit/claustrum", id);
        body["run_id"] = json!(id);
        body["head_branch"] = json!(branch);
        body["actor"] = json!("some-contributor");
        body["head_repo_is_fork"] = json!(true);
        t.ci(body);
        assert_eq!(t.writes()[0]["action"], "wake", "{branch}");
    }
}

#[test]
fn failed_jobs_grow_one_silent_item_on_any_branch() {
    let t = Test::new();
    for id in 101..=103 {
        let mut body = job("cortexkit/basal", id);
        body["head_branch"] = json!("feature/ci");
        if id == 103 {
            body["conclusion"] = json!("timed_out");
        }
        t.ci(body);
        let writes = t.writes();
        assert_eq!(writes.len(), (id - 100) as usize);
        let last = writes.last().unwrap();
        assert_eq!(last["agent"], BASAL);
        assert_eq!(last["action"], if id == 101 { "wake" } else { "silent" });
        assert_eq!(last["replace_key"], "ci:cortexkit/basal:42:1");
        let names = (101..=id)
            .map(|n| format!("linux shard {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            last["item"]["title"],
            "cortexkit/basal: CI failed on feature/ci"
        );
        assert!(
            last["item"]["body"]
                .as_str()
                .unwrap()
                .contains(&format!("Failed jobs: {names}"))
        );
        assert_eq!(
            last["item"]["links"].as_array().unwrap().len(),
            (id - 100) as usize
        );
    }
    assert_eq!(t.memory()["runs"].as_object().unwrap().len(), 1);
}

/// A failed job no runner picked up never started, as when GitHub refuses a
/// private repo's jobs for billing. Plexus sends `runner_name: null` for it.
fn never_started(id: u64) -> Value {
    let mut body = job("cortexkit/basal", id);
    body["runner_name"] = Value::Null;
    body["steps_run"] = json!(0);
    body
}

#[test]
fn jobs_that_never_started_record_silently_and_never_wake_the_run() {
    let t = Test::new();
    t.ci(never_started(101));
    t.ci(never_started(102));
    t.ci(run("cortexkit/basal", "failure"));
    let writes = t.writes();
    assert_eq!(writes.len(), 3);
    assert!(
        writes.iter().all(|write| write["action"] == "silent"),
        "{writes:?}"
    );
    let body = writes[2]["item"]["body"].as_str().unwrap();
    assert!(body.contains("linux shard 101 (not started)"), "{body}");
}

#[test]
fn a_job_that_started_still_wakes_beside_never_started_ones() {
    // A runner but no steps: it failed during runner setup, so it started.
    let mut setup_failure = job("cortexkit/basal", 103);
    setup_failure["runner_name"] = json!("GitHub Actions 7");
    setup_failure["steps_run"] = json!(0);
    // An older body without the field is not known to be unstarted.
    let t = Test::new();
    t.ci(job("cortexkit/basal", 104));
    assert_eq!(t.writes()[0]["action"], "wake");

    let t = Test::new();
    t.ci(never_started(101));
    t.ci(setup_failure);
    let writes = t.writes();
    assert_eq!(writes[0]["action"], "silent");
    assert_eq!(writes[1]["action"], "wake");

    // After a real wake, a later never-started job doesn't wake the
    // run's completion either.
    let t = Test::new();
    t.ci(job("cortexkit/basal", 105));
    t.ci(never_started(106));
    t.ci(run("cortexkit/basal", "failure"));
    let actions: Vec<_> = t.writes().iter().map(|w| w["action"].clone()).collect();
    assert_eq!(actions, [json!("wake"), json!("silent"), json!("silent")]);
}

#[test]
fn run_finished_after_new_jobs_wakes_and_names_only_jobs_since_first_notice() {
    let t = Test::new();
    for id in 101..=107 {
        t.ci(job("cortexkit/basal", id));
    }
    t.ci(run("cortexkit/basal", "failure"));
    t.ci(run("cortexkit/basal", "failure"));

    let writes = t.writes();
    assert_eq!(writes.len(), 9);
    assert_eq!(writes[0]["action"], "wake");
    assert!(writes[1..7].iter().all(|write| write["action"] == "silent"));
    let finished = &writes[7];
    assert_eq!(finished["action"], "wake");
    assert_eq!(finished["replace_key"], "ci:cortexkit/basal:42:1");
    assert_eq!(
        finished["item"]["title"],
        "cortexkit/basal: CI failed on main"
    );
    assert!(
        !finished["item"]["body"]
            .as_str()
            .unwrap()
            .contains("linux shard 101")
    );
    assert!(finished["item"]["body"].as_str().unwrap().contains(
        "Failed since first notice: linux shard 102, linux shard 103, linux shard 104, linux shard 105, linux shard 106, linux shard 107"
    ));
    assert_eq!(
        finished["item"]["data"]["observed"]["failed_jobs_since_first_notice"],
        "linux shard 102, linux shard 103, linux shard 104, linux shard 105, linux shard 106, linux shard 107"
    );
    assert!(
        t.memory()["runs"]["ci:cortexkit/basal:42:1"]["completionWoke"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(writes[8]["action"], "silent");
}

#[test]
fn duplicate_job_id_adds_no_write_even_with_a_new_event_key() {
    let t = Test::new();
    t.ci(job("cortexkit/basal", 101));
    let duplicate = t.ci(job("cortexkit/basal", 101));
    assert_eq!(t.host.resolved.load(Ordering::Relaxed), 2);
    assert_eq!(t.writes().len(), 1);
    assert!(
        !t.f.module
            .rt
            .calls(&duplicate)
            .unwrap()
            .iter()
            .any(|c| c.kind == CallKind::Primitive(Primitive::SinkDigest))
    );
}

#[test]
fn rerun_attempt_gets_a_new_item_with_the_same_job_id() {
    let t = Test::new();
    t.ci(job("cortexkit/basal", 101));
    let mut body = job("cortexkit/basal", 101);
    body["run_attempt"] = json!(2);
    t.ci(body);
    let writes = t.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0]["replace_key"], "ci:cortexkit/basal:42:1");
    assert_eq!(writes[1]["replace_key"], "ci:cortexkit/basal:42:2");
    assert_eq!(writes[0]["action"], "wake");
    assert_eq!(writes[1]["action"], "wake");
}

#[test]
fn superseded_attempt_and_later_events_send_nothing() {
    // On master, so magic-context's wake-branch list lets the first job wake and
    // the supersession read happen.
    let on_master = |repo: &str, id: u64| {
        let mut body = job(repo, id);
        body["head_branch"] = json!("master");
        body
    };
    for repo in ["cortexkit/aft", "cortexkit/magic-context"] {
        let t = Test::new();
        t.supersession(supersession_reply(repo, 42, true));
        t.ci(on_master(repo, 101));
        assert!(t.writes().is_empty());
        assert_eq!(
            t.memory()["runs"][format!("ci:{repo}:42:1")]["superseded"],
            true
        );
        t.supersession(supersession_reply(repo, 42, false));
        t.ci(on_master(repo, 102));
        t.ci(run(repo, "failure"));
        t.ci(run(repo, "success"));
        assert!(t.writes().is_empty());
        assert_eq!(
            t.reads(),
            vec![json!({
                "action":"github.actions.run.superseded", "repository":repo, "args":{"run_id":42}
            })]
        );
    }
}

#[test]
fn nonsuperseded_attempt_wakes_and_reads_only_once_per_attempt() {
    let t = Test::new();
    t.ci(job("CortexKit/Basal", 101));
    t.ci(job("cortexkit/basal", 102));
    t.ci(run("cortexkit/basal", "failure"));
    assert_eq!(
        t.reads(),
        vec![json!({
            "action":"github.actions.run.superseded", "repository":"cortexkit/basal", "args":{"run_id":42}
        })]
    );
    assert_eq!(t.writes()[0]["action"], "wake");
    assert_eq!(t.writes()[1]["action"], "silent");
    assert_eq!(t.writes()[2]["action"], "wake");
    let mut retry = job("cortexkit/basal", 101);
    retry["run_attempt"] = json!(2);
    t.ci(retry);
    assert_eq!(t.reads().len(), 2);
    assert_eq!(t.writes()[3]["action"], "wake");
}

#[test]
fn supersession_refusals_and_unreadable_replies_fail_open_and_are_logged() {
    for code in [
        "github_actions_quota_exhausted",
        "run_not_found",
        "repository_not_visible",
        "unreadable",
    ] {
        let t = Test::new();
        t.supersession(if code == "unreadable" {
            HostOutcome::fulfilled(text(json!({"result":{"superseded":"true"}})))
        } else {
            HostOutcome::fulfilled(text(
                json!({"error":{"code":code, "detail":{"retry_after_ms":60000}}}),
            ))
        });
        let id = t.ci(job("cortexkit/basal", 101));
        let done = t.f.module.rt.run(&id).unwrap();
        let result: Value = serde_json::from_str(done.result.as_ref().unwrap()).unwrap();
        assert!(
            result["refusals"][0]["code"]
                .as_str()
                .unwrap()
                .contains(code)
        );
        assert_eq!(t.writes()[0]["action"], "wake");
        t.ci(job("cortexkit/basal", 102));
        t.ci(run("cortexkit/basal", "failure"));
        assert_eq!(t.writes()[2]["action"], "wake");
        assert_eq!(t.reads().len(), 1);
    }
}

#[test]
fn supersession_answer_is_reused_after_sink_refusal() {
    let t = Test::new();
    t.host.refuse.store(true, Ordering::Relaxed);
    t.ci(job("cortexkit/basal", 101));
    t.ci(job("cortexkit/basal", 102));
    assert_eq!(t.reads().len(), 1);
    assert!(t.writes().iter().all(|write| write["action"] == "wake"));
}

#[test]
fn rejected_supersession_reads_fail_open_with_the_refusal_code() {
    for refusal in [
        json!({"code":"module_grant_absent", "message":"no grant"}),
        json!({"error":{"code":"read_dispatch_failed", "message":"read failed"}}),
    ] {
        let t = Test::new();
        t.supersession(HostOutcome::rejected(text(refusal.clone())));
        let id = t.ci(job("cortexkit/basal", 101));
        let done = t.f.module.rt.run(&id).unwrap();
        let result: Value = serde_json::from_str(done.result.as_ref().unwrap()).unwrap();
        let expected = refusal.get("error").unwrap_or(&refusal);
        assert_eq!(result["refusals"][0]["code"], expected["code"]);
        assert_eq!(t.writes()[0]["action"], "wake");
    }
}

#[test]
fn green_quiet_ignored_and_stale_events_never_read_supersession() {
    let t = Test::new();
    t.ci(run("cortexkit/basal", "success"));
    t.ci(job("cortexkit/cerebellum", 101));
    t.ci(run("cortexkit/cerebellum", "failure"));
    let mut ignored = job("cortexkit/aft", 101);
    ignored["workflow_name"] = json!("Design gate");
    t.ci(ignored);
    let mut ignored = job("cortexkit/aft", 102);
    ignored["name"] = json!("design-gate");
    t.ci(ignored);
    let mut stale = job("cortexkit/basal", 101);
    stale["completed_at"] = json!(completed_at(&t, 7 * common::HOUR));
    t.ci(stale);
    assert!(t.reads().is_empty());
}

#[test]
fn merge_queue_repeats_are_silent_until_main_workflow_goes_green() {
    for (branch, default_branch) in [("main", None), ("master", None), ("trunk", Some("trunk"))] {
        let t = Test::new();
        let mut main = job("cortexkit/aft", 101);
        main["workflow_name"] = json!("CI");
        main["name"] = json!("Tests");
        main["head_branch"] = json!(branch);
        main["default_branch"] = json!(default_branch);
        t.ci(main);
        let mut queued = job("cortexkit/aft", 102);
        queued["run_id"] = json!(43);
        queued["workflow_name"] = json!("CI");
        queued["name"] = json!("Tests");
        queued["head_branch"] = json!("gh-readonly-queue/main/pr-123-1");
        t.ci(queued.clone());
        let mut finished = run("cortexkit/aft", "failure");
        finished["run_id"] = json!(43);
        finished["head_branch"] = queued["head_branch"].clone();
        t.ci(finished);
        assert_eq!(t.writes()[0]["action"], "wake");
        assert_eq!(t.writes()[1]["action"], "silent");
        assert_eq!(t.writes()[2]["action"], "silent");
        assert_eq!(t.reads().len(), 1);

        let mut green = run("cortexkit/aft", "success");
        green["id"] = json!(44);
        green["run_id"] = json!(44);
        green["head_branch"] = json!(branch);
        green["default_branch"] = json!(default_branch);
        t.ci(green);
        assert!(t.memory()["mainFailures"].as_object().unwrap().is_empty());
        queued["run_id"] = json!(45);
        t.ci(queued);
        assert_eq!(t.writes()[3]["action"], "wake");
        assert_eq!(t.reads().len(), 2);
    }
}

#[test]
fn merge_queue_failure_flags_are_scoped_to_repo_workflow_and_job() {
    let t = Test::new();
    let mut main = job("cortexkit/aft", 101);
    main["workflow_name"] = json!("CI");
    t.ci(main);
    for (repo, workflow, id) in [
        ("cortexkit/basal", "CI", 101),
        ("cortexkit/aft", "Release", 101),
        ("cortexkit/aft", "CI", 102),
    ] {
        let mut queued = job(repo, id);
        queued["run_id"] = json!(id + 100);
        queued["workflow_name"] = json!(workflow);
        queued["head_branch"] = json!("gh-readonly-queue/main/pr-123-1");
        t.ci(queued);
    }
    assert_eq!(t.writes().len(), 4);
    assert!(t.writes().iter().all(|write| write["action"] == "wake"));
}

#[test]
fn main_failure_flags_share_the_memory_byte_cap_and_age_pruning() {
    let t = Test::new();
    for id in 1..=60 {
        t.f.clock.advance(3_600_000);
        let mut main = job("cortexkit/aft", id);
        main["run_id"] = json!(id);
        main["workflow_name"] = json!("CI");
        main["name"] = json!(format!("{} {id}", "é🦀".repeat(200)));
        t.ci(main);
    }
    let memory = t.memory();
    assert!(serde_json::to_vec(&memory).unwrap().len() <= 48000);
    let failures = memory["mainFailures"].as_object().unwrap();
    assert!(!failures.is_empty());
    assert!(failures.len() < 60);
    let mut fresh = job("cortexkit/basal", 200);
    fresh["run_id"] = json!(200);
    fresh["completed_at"] = json!("2027-01-16T12:00:00Z");
    t.ci(fresh);
    assert!(t.memory()["mainFailures"].as_object().unwrap().is_empty());
}

#[test]
fn ignored_job_names_and_completions_with_only_ignored_failures_send_nothing() {
    let t = Test::new();
    let mut ignored = job("cortexkit/aft", 101);
    ignored["name"] = json!("design-gate");
    t.ci(ignored.clone());
    t.ci(ignored.clone());
    assert!(t.writes().is_empty());
    t.ci(run("cortexkit/aft", "failure"));
    assert!(t.writes().is_empty());
    assert!(t.reads().is_empty());

    t.ci(job("cortexkit/aft", 102));
    t.ci(ignored);
    t.ci(run("cortexkit/aft", "failure"));
    assert_eq!(t.writes().len(), 2);
    assert_eq!(t.writes()[0]["action"], "wake");
    assert_eq!(t.writes()[1]["action"], "silent");
    assert_eq!(
        t.writes()[1]["item"]["data"]["observed"]["failed_jobs"],
        "linux shard 102"
    );
}

#[test]
fn red_run_without_jobs_creates_one_item_and_only_startup_failure_wakes() {
    // A plain red run with no failed job either has job events still arriving
    // or never ran a job (a fork run closed while awaiting approval), so it is
    // recorded silently; startup_failure can never produce a job, so it wakes.
    for (conclusion, action) in [("startup_failure", "wake"), ("failure", "silent")] {
        let t = Test::new();
        t.ci(run("cortexkit/basal", conclusion));
        let writes = t.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0]["agent"], BASAL);
        assert_eq!(writes[0]["action"], action, "{conclusion}");
        assert_eq!(writes[0]["replace_key"], "ci:cortexkit/basal:42:1");
        assert_eq!(
            writes[0]["item"]["title"],
            format!("cortexkit/basal: CI run on main ended {conclusion}")
        );
        assert_eq!(
            writes[0]["item"]["body"],
            format!(
                "Failed jobs not reported yet\nRun 42, attempt 1, finished red ({conclusion})\nCommit 0123456789ab"
            )
        );
    }
}

#[test]
fn a_job_arriving_after_a_silent_red_run_still_wakes_once() {
    let t = Test::new();
    t.ci(run("cortexkit/basal", "failure"));
    t.ci(job("cortexkit/basal", 101));
    t.ci(job("cortexkit/basal", 102));
    let actions: Vec<_> = t.writes().iter().map(|w| w["action"].clone()).collect();
    assert_eq!(actions, [json!("silent"), json!("wake"), json!("silent")]);
}

#[test]
fn completed_red_updates_the_existing_item_then_green_updates_silently() {
    let t = Test::new();
    t.ci(job("cortexkit/basal", 101));
    t.ci(run("cortexkit/basal", "failure"));
    t.ci(run("cortexkit/basal", "success"));
    let writes = t.writes();
    assert_eq!(writes.len(), 3);
    assert!(
        writes
            .iter()
            .all(|w| w["replace_key"] == "ci:cortexkit/basal:42:1")
    );
    // The run finishing red adds no new failed job, so it must not wake again.
    assert_eq!(writes[1]["action"], "silent");
    assert_eq!(
        writes[1]["item"]["title"],
        "cortexkit/basal: CI failed on main"
    );
    assert!(
        writes[1]["item"]["body"]
            .as_str()
            .unwrap()
            .contains("finished red (failure)")
    );
    assert_eq!(writes[2]["action"], "silent");
    assert_eq!(
        writes[2]["item"]["title"],
        "cortexkit/basal: CI green again on main"
    );
}

#[test]
fn unknown_conclusion_and_branch_are_never_green() {
    let t = Test::new();
    let mut body = job("cortexkit/basal", 101);
    body["head_branch"] = Value::Null;
    body["conclusion"] = Value::Null;
    t.ci(body);
    let mut body = run("cortexkit/basal", "failure");
    body["head_branch"] = Value::Null;
    body["conclusion"] = Value::Null;
    t.ci(body);
    let writes = t.writes();
    assert_eq!(writes.len(), 2);
    for (index, write) in writes.into_iter().enumerate() {
        assert_eq!(write["action"], if index == 0 { "wake" } else { "silent" });
        let title = write["item"]["title"].as_str().unwrap();
        assert!(!title.contains("green"), "{title}");
        assert!(!write["item"]["body"].as_str().unwrap().contains("green"));
        assert_eq!(write["item"]["data"]["observed"]["conclusion"], "unknown");
        assert_eq!(write["item"]["data"]["observed"]["branch"], "unknown");
    }
}

#[test]
fn unreported_green_and_unrelated_attempt_green_write_nothing() {
    let t = Test::new();
    t.ci(run("cortexkit/basal", "success"));
    assert!(t.writes().is_empty());
    t.ci(job("cortexkit/basal", 101));
    let mut body = run("cortexkit/basal", "success");
    body["run_attempt"] = json!(2);
    t.ci(body);
    assert_eq!(t.writes().len(), 1);
}

#[test]
fn unknown_agent_refusal_is_logged_and_later_runs_continue() {
    let t = Test::new();
    t.host.refuse.store(true, Ordering::Relaxed);
    let id = t.ci(job("cortexkit/basal", 101));
    let done = t.f.module.rt.run(&id).unwrap();
    let result: Value = serde_json::from_str(done.result.as_ref().unwrap()).unwrap();
    assert_eq!(
        result["refusals"],
        json!([{"repo":"cortexkit/basal", "owner":BASAL, "code":"sink_target_not_in_audience"}])
    );
    assert_eq!(
        t.memory()["runs"]["ci:cortexkit/basal:42:1"]["firstWakeJobIds"],
        Value::Null
    );
    t.ci(job("cortexkit/basal", 102));
    assert_eq!(t.writes()[1]["action"], "wake");
    assert_eq!(
        t.memory()["runs"]["ci:cortexkit/basal:42:1"]["firstWakeJobIds"],
        json!(["101", "102"])
    );
    t.ci(job("cortexkit/plexus", 102));
    assert_eq!(t.writes().len(), 3);
    assert_eq!(t.writes()[2]["agent"], PLEX);
}

#[test]
fn non_cortexkit_headers_never_start_runs_and_verified_body_is_rechecked() {
    let t = Test::new();
    for body in [
        job("cortexkit/basal", 101),
        run("cortexkit/basal", "failure"),
    ] {
        for repo in ["other/basal", "", "cortexkit-other/basal"] {
            t.notice(body.clone(), repo, "main", false);
        }
    }
    let mut body = job("other/basal", 101);
    body["head_branch"] = json!("verified-branch");
    t.notice(body, "cortexkit/basal", "main", true);
    assert!(t.writes().is_empty());
    let mut body = job("cortexkit/plexus", 102);
    body["head_branch"] = json!("verified-branch");
    t.notice(body, "cortexkit/basal", "main", true);
    let writes = t.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["agent"], PLEX);
    assert!(
        writes[0]["item"]["title"]
            .as_str()
            .unwrap()
            .contains("on verified-branch")
    );
}

#[test]
fn fleet_memory_is_byte_bounded_and_age_pruned_across_many_repos() {
    let t = Test::new();
    let repos = [
        "basal",
        "aft",
        "plexus",
        "broca",
        "prefrontal",
        "insula",
        "engram",
        "motor",
        "synapse",
        "astrocyte",
        "cerebellum",
        "wernicke",
    ];
    for id in 1..=120 {
        // Separate admissions so the runtime's hourly run limit does not mask KV eviction.
        t.f.clock.advance(3_600_000);
        let mut body = job(
            &format!("cortexkit/{}", repos[(id as usize - 1) % repos.len()]),
            id,
        );
        body["run_id"] = json!(id);
        body["completed_at"] = json!(format!("2027-01-01T12:{:02}:{:02}Z", id / 60, id % 60));
        body["name"] = json!("é🦀".repeat(50));
        body["head_branch"] = json!("b".repeat(256));
        t.ci(body);
    }
    let memory = t.memory();
    assert!(serde_json::to_vec(&memory).unwrap().len() <= 48000);
    let runs = memory["runs"].as_object().unwrap();
    assert!(!runs.is_empty());
    assert!(runs.len() < 120, "must actually reach the shared byte cap");
    assert!(runs.keys().all(|key| key.starts_with("ci:cortexkit/")));
    assert!(
        runs.keys()
            .any(|key| key.starts_with("ci:cortexkit/basal:"))
    );
    assert!(
        runs.keys()
            .any(|key| key.starts_with("ci:cortexkit/plexus:"))
    );
    for id in 121..=130 {
        t.ci(job("cortexkit/unmapped", id));
    }
    assert_eq!(t.memory()["unmapped"], 10);
    assert!(serde_json::to_vec(&t.memory()).unwrap().len() <= 48000);
    let mut fresh = job("cortexkit/basal", 200);
    fresh["run_id"] = json!(200);
    fresh["completed_at"] = json!("2027-01-16T12:00:00Z");
    t.ci(fresh);
    assert_eq!(t.memory()["runs"].as_object().unwrap().len(), 1);
    let count = t.writes().len();
    t.ci(job("cortexkit/plexus", 201));
    assert_eq!(t.writes().len(), count);
}

#[test]
fn synthetic_trigger_dry_run_succeeds_without_dispatch() {
    let t = Test::new();
    let sends = t.host.mock.total_sends();
    let reply = t
        .f
        .module
        .handle(
            &Caller::Operator,
            "flow.dry_run",
            json!({
                "flow_id":"ci-failure-router",
                "trigger":{"event":{
                    "module":"plexus", "name":"github_ci_job_failed", "version":2,
                    "event_key":"synthetic", "headers":{"repo":"cortexkit/basal", "branch":"main"},
                    "body":job("cortexkit/basal", 101)
                }}
            }),
        )
        .unwrap();
    assert_eq!(reply["summary"]["mode"], "capture");
    let runs = reply["summary"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["state"], "succeeded", "{reply:#}");
    assert_eq!(t.host.mock.total_sends(), sends);
    assert_eq!(t.host.resolved.load(Ordering::Relaxed), 0);
    assert!(t.writes().is_empty());
    let captured = runs[0]["calls"].as_array().unwrap();
    assert!(
        captured
            .iter()
            .any(|call| call["kind"] == "sink.digest" && call["args"]["agent"] == BASAL),
        "{reply:#}"
    );
}
