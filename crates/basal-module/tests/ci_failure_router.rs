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
}

fn text(value: Value) -> JsonText {
    JsonText::new(value.to_string()).unwrap()
}

impl CiHost {
    fn answer(&self, request: &CallRequest) -> Option<HostOutcome> {
        let args: Value = serde_json::from_str(request.args.as_str()).unwrap();
        if request.kind == CallKind::Primitive(Primitive::SinkDigest) {
            let item = &args["item"];
            assert!(item["title"].as_str().unwrap().chars().count() < 200);
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
    assert_eq!(value["version"], 3);
    assert_eq!(
        value["sinks"],
        json!([{"agent":"$audience", "digest_max":"wake"}])
    );
    assert_eq!(
        value["ops"],
        json!([{"module":"plexus", "op":"events_get"}])
    );
    assert_eq!(
        value["trigger"]["events"],
        json!([
            {"module":"plexus", "name":"github_ci_job_failed", "version":2, "match":{"repo":"cortexkit/*"}},
            {"module":"plexus", "name":"github_ci_run_completed", "version":2, "match":{"repo":"cortexkit/*"}}
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
fn second_distinct_failed_job_of_a_run_wakes() {
    let t = Test::new();
    t.ci(job("cortexkit/basal", 101));
    t.ci(job("cortexkit/basal", 102));
    let writes = t.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0]["action"], "wake");
    assert_eq!(writes[1]["action"], "wake");
    assert_eq!(writes[0]["replace_key"], writes[1]["replace_key"]);
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
fn three_failed_jobs_update_one_item_and_wake_on_any_branch() {
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
        assert_eq!(last["action"], "wake");
        assert_eq!(last["replace_key"], "ci:cortexkit/basal:42:1");
        let names = (101..=id)
            .map(|n| format!("linux shard {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            last["item"]["title"],
            format!("cortexkit/basal run 42 (attempt 1) on feature/ci: failed jobs {names}")
        );
        assert!(
            last["item"]["body"]
                .as_str()
                .unwrap()
                .contains("test names unavailable")
        );
        assert_eq!(
            last["item"]["links"].as_array().unwrap().len(),
            (id - 100) as usize
        );
    }
    assert_eq!(t.memory()["runs"].as_object().unwrap().len(), 1);
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
fn red_run_without_jobs_including_startup_failure_creates_exactly_one_item() {
    for conclusion in ["startup_failure", "failure"] {
        let t = Test::new();
        t.ci(run("cortexkit/basal", conclusion));
        let writes = t.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0]["agent"], BASAL);
        assert_eq!(writes[0]["action"], "wake");
        assert_eq!(writes[0]["replace_key"], "ci:cortexkit/basal:42:1");
        assert!(
            writes[0]["item"]["title"]
                .as_str()
                .unwrap()
                .contains(&format!(
                    "run failed with no failed job reported ({conclusion})"
                ))
        );
        assert!(
            writes[0]["item"]["body"]
                .as_str()
                .unwrap()
                .contains("test names unavailable")
        );
    }
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
    assert_eq!(writes[1]["action"], "silent");
    assert!(
        writes[1]["item"]["title"]
            .as_str()
            .unwrap()
            .contains("run finished red (failure)")
    );
    assert_eq!(writes[2]["action"], "silent");
    assert!(
        writes[2]["item"]["title"]
            .as_str()
            .unwrap()
            .ends_with("now green")
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
        assert!(
            !write["item"]["title"]
                .as_str()
                .unwrap()
                .contains("now green")
        );
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
    t.ci(job("cortexkit/plexus", 102));
    assert_eq!(t.writes().len(), 2);
    assert_eq!(t.writes()[1]["agent"], PLEX);
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
