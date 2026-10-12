//! These tests exercise published template bytes through package registration,
//! real workers, canonical parameter grants, event-body resolution, and
//! capture-only dry runs.
mod common;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use basal_core::{Admission, NoHooks, RunState};
use basal_host::mock::MockHost;
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, EventBody, EventDecl, EventOrigin, Host,
    HostOutcome, InstallStatus, MockCatalog, MockConsent, OpDecl, OpKind, TransportError,
};
use basal_module::{caller::Caller, module::Hosts};
use basal_proto::{CallKind, JsonText, Primitive};
use common::{Fixture, Options, agent, fixture};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const OWNER: &str = "agent_47120287c700722b";
const TEMPLATES: [(&str, &str, &str); 4] = [
    (
        "watch-file",
        include_str!("../../../templates/watch-file/manifest.json"),
        include_str!("../../../templates/watch-file/flow.js"),
    ),
    (
        "watch-git",
        include_str!("../../../templates/watch-git/manifest.json"),
        include_str!("../../../templates/watch-git/flow.js"),
    ),
    (
        "watch-github",
        include_str!("../../../templates/watch-github/manifest.json"),
        include_str!("../../../templates/watch-github/flow.js"),
    ),
    (
        "remind-at",
        include_str!("../../../templates/remind-at/manifest.json"),
        include_str!("../../../templates/remind-at/flow.js"),
    ),
];
const PR: &str = include_str!("data/template-pr-v1.json");
const ISSUE: &str = include_str!("data/template-issue-v1.json");
const RELEASE: &str = include_str!("data/template-release-v1.json");

fn text(value: Value) -> JsonText {
    JsonText::new(value.to_string()).unwrap()
}

fn limits(item: &Value) {
    assert!(item["title"].as_str().unwrap().chars().count() < 200);
    assert!(item["body"].as_str().unwrap().len() <= 4096);
    assert!(item["data"].is_object());
    // serde_json's default map is ordered; this checks the canonical encoding
    // independently of the JavaScript helper's size calculation.
    assert!(serde_json::to_vec(&item["data"]).unwrap().len() <= 8192);
    assert!(serde_json::to_vec(item).unwrap().len() <= 16384);
    assert!(item["links"].as_array().unwrap().len() <= 8);
}

/// Real built-ins and digest effects use the existing mock. Overrides exercise
/// transient failures and an unusually large log without creating 500 commits.
#[derive(Default)]
struct TemplateHost {
    mock: MockHost,
    overrides: Mutex<VecDeque<(Primitive, HostOutcome)>>,
    bodies: Mutex<BTreeMap<String, Value>>,
    resolved: AtomicUsize,
}

impl TemplateHost {
    fn response(&self, primitive: Primitive, value: Value) {
        self.overrides
            .lock()
            .unwrap()
            .push_back((primitive, HostOutcome::fulfilled(text(value))));
    }

    fn reject(&self, primitive: Primitive, code: &str) {
        self.overrides.lock().unwrap().push_back((
            primitive,
            HostOutcome::rejected(text(json!({"code":code,"message":code}))),
        ));
    }

    fn answer(&self, request: &CallRequest) -> Option<HostOutcome> {
        if let CallKind::Primitive(p) = request.kind {
            if p == Primitive::SinkDigest {
                let args: Value = serde_json::from_str(request.args.as_str()).unwrap();
                limits(&args["item"]);
            }
            let mut overrides = self.overrides.lock().unwrap();
            if overrides
                .front()
                .is_some_and(|(expected, _)| *expected == p)
            {
                return overrides.pop_front().map(|(_, outcome)| outcome);
            }
        }
        if request.kind
            == (CallKind::Op {
                module: "plexus".into(),
                op: "events_get".into(),
            })
        {
            let args: Value = serde_json::from_str(request.args.as_str()).unwrap();
            let body = self.bodies.lock().unwrap()[args["event_key"].as_str().unwrap()].to_string();
            self.resolved.fetch_add(1, Ordering::Relaxed);
            return Some(HostOutcome::fulfilled(text(json!({
                "body":body,"digest":format!("{:x}",Sha256::digest(body.as_bytes())),
                "event_key":args["event_key"],"event_name":args["event_name"],
            }))));
        }
        None
    }
}

impl Host for TemplateHost {
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

struct WorkDirectory(PathBuf);
impl WorkDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = basal_host::builtins::expand_home("~/Work").unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!(
            "basal-template-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(common::canonical(&path))
    }
}
impl Drop for WorkDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

struct Test {
    f: Fixture,
    host: Arc<TemplateHost>,
    work: WorkDirectory,
}
impl Test {
    fn new() -> Self {
        let work = WorkDirectory::new();
        let host = Arc::new(TemplateHost::default());
        let catalog = MockCatalog::standard();
        catalog.add_named_agent(OWNER, "Owner");
        catalog.set_op(
            "plexus",
            "events_get",
            OpDecl {
                kind: Some(OpKind::Query),
                cause_echo: false,
                shell_capable: false,
            },
        );
        for name in [
            "github_pr_changed",
            "github_issue_changed",
            "github_release_published",
        ] {
            catalog.set_event(
                "plexus",
                name,
                1,
                EventDecl {
                    origin: EventOrigin::External,
                    body: EventBody::Resolved {
                        resolve_op: "events_get".into(),
                    },
                },
            );
        }
        let f = fixture(
            "published-templates",
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
        for (_, manifest, script) in TEMPLATES {
            let registered = f
                .module
                .handle(
                    &Caller::Operator,
                    "package.register",
                    json!({"manifest":manifest,"script":script}),
                )
                .unwrap();
            let stored = f
                .module
                .rt
                .get_package(registered["package"].as_str().unwrap(), 1)
                .unwrap();
            assert_eq!(stored["manifest"], manifest);
            assert_eq!(stored["script"], script);
        }
        Self { f, host, work }
    }

    fn create(&self, template: &str, params: Value) -> Value {
        self.f
            .module
            .handle(
                &agent(OWNER),
                "flow.instantiate",
                json!({
                    "template":template,"version":1,"params":params,"expires_in":"365d",
                }),
            )
            .unwrap()
    }

    fn live(&self, instance: &Value, expected: bool) {
        let live = self.f.module.rt.live_instances().unwrap();
        assert_eq!(
            live["instances"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["flow_id"] == instance["flow_id"]),
            expected,
            "{live}"
        );
    }

    fn run(&self, instance: &Value, key: &str) -> String {
        let run = self
            .f
            .module
            .rt
            .admit_trigger(instance["flow_id"].as_str().unwrap(), key, JsonText::null())
            .unwrap()
            .run_id()
            .unwrap()
            .to_owned();
        self.finish(&run);
        run
    }

    fn finish(&self, run: &str) {
        self.f.module.engine.run_until_idle(100).unwrap();
        let stored = self.f.module.rt.run(run).unwrap();
        assert_eq!(stored.state, RunState::Succeeded, "{stored:#?}");
    }

    fn event(&self, instance: &Value, name: &str, repo: &str, body: Value) -> String {
        let key = format!("{:064x}", self.host.bodies.lock().unwrap().len() + 1);
        let bytes = body.to_string();
        self.host.bodies.lock().unwrap().insert(key.clone(), body);
        self.f
            .module
            .rt
            .admit_events(&[basal_core::events::Notice {
                published_at_ms: self.f.clock.now_ms(),
                subject: format!("ck.account.event.plexus.{name}.v1"),
                event_key: key,
                digest: format!("{:x}", Sha256::digest(bytes.as_bytes())),
                headers: [("repo".into(), repo.into())].into(),
            }])
            .unwrap();
        let startable = self.f.module.rt.startable().unwrap();
        assert_eq!(startable.len(), 1, "{startable:?}");
        let run = startable[0].0.clone();
        assert_eq!(
            self.f.module.rt.run(&run).unwrap().flow_id,
            instance["flow_id"].as_str().unwrap()
        );
        self.finish(&run);
        run
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

    fn notified(&self, instance: &Value, run: &str, title: &str) -> Value {
        let items = self.items();
        assert_eq!(items.len(), 1, "{items:#?}");
        assert_eq!(items[0]["agent"], OWNER);
        assert_eq!(items[0]["action"], "wake");
        let item = &items[0]["item"];
        limits(item);
        assert_eq!(item["body"], instance["params"]["note"]);
        assert!(item["title"].as_str().unwrap().contains(title), "{item}");
        assert!(item["data"]["params"].get("note").is_none());
        self.live(instance, false);
        let calls = self.f.module.rt.calls(run).unwrap();
        let sink = calls
            .iter()
            .position(|call| call.kind == CallKind::Primitive(Primitive::SinkDigest))
            .unwrap();
        assert_eq!(
            calls[sink + 1].kind,
            CallKind::Primitive(Primitive::InstanceEnd)
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.kind == CallKind::Primitive(Primitive::SinkDigest))
                .count(),
            1
        );
        assert_eq!(
            self.f
                .module
                .rt
                .admit_trigger(
                    instance["flow_id"].as_str().unwrap(),
                    "after-end",
                    JsonText::null()
                )
                .unwrap(),
            Admission::NotApproved
        );
        item.clone()
    }

    fn dry(&self, template: &str, params: Value, trigger: Value) -> Value {
        let sends = self.host.mock.total_sends();
        let resolved = self.host.resolved.load(Ordering::Relaxed);
        let reply = self
            .f
            .module
            .handle(
                &agent(OWNER),
                "flow.dry_run",
                json!({
                    "flow_id":template,"params":params,"trigger":trigger,
                }),
            )
            .unwrap();
        assert_eq!(reply["summary"]["mode"], "capture");
        let runs = reply["summary"]["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["state"], "succeeded", "{reply:#}");
        assert_eq!(self.host.mock.total_sends(), sends);
        assert_eq!(self.host.resolved.load(Ordering::Relaxed), resolved);
        reply
    }
}

fn file_params(path: &Path, condition: &str, text: &str) -> Value {
    json!({"path":path,"condition":condition,"text":text,"every":"5m","note":"Grüße from the file watch"})
}
fn git_params(repo: &Path, condition: &str, reference: &str, pattern: &str) -> Value {
    json!({"repo":repo,"condition":condition,"ref":reference,"pattern":pattern,"every":"5m","note":"Review the new Git result"})
}
fn github_params(kind: &str, state: &str) -> Value {
    json!({"repo":"EXAMPLE/Project","kind":kind,"number":if kind == "pr" {42} else {43},"state":state,"note":"Review the GitHub result"})
}
fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Template Test",
            "-c",
            "user.email=template@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn repo(test: &Test) -> PathBuf {
    let repo = test.work.0.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Release already existed",
        ],
    );
    repo
}
fn baseline(test: &Test, instance: &Value) -> String {
    test.f
        .module
        .rt
        .store()
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT value FROM kv WHERE flow_id=?1 AND key='baseline'",
                [instance["flow_id"].as_str().unwrap()],
                |row| row.get::<_, String>(0),
            )?)
        })
        .map(|value| serde_json::from_str(&value).unwrap())
        .unwrap()
}

#[test]
fn template_digest_helpers_are_identical() {
    let helper = |script: &'static str| script.split("// END boundedDigest").next().unwrap();
    for (name, _, script) in TEMPLATES {
        assert_eq!(helper(script), helper(TEMPLATES[0].2), "{name}");
    }
}

#[test]
fn published_templates_declare_exact_bounds_and_self_only_grants() {
    for (name, manifest, _) in TEMPLATES {
        let m: Value = serde_json::from_str(manifest).unwrap();
        assert_eq!(m["id"], name);
        assert_eq!(m["version"], 1);
        assert_eq!(
            m["params"]["note"],
            json!({"type":"string","max_bytes":4096})
        );
        assert_eq!(
            m["template"],
            json!({"max_per_agent":100,"max_lifetime":"365d"})
        );
        assert_eq!(m["sinks"], json!([{"agent":"$self","digest_max":"wake"}]));
        assert!(m.get("audience").is_none());
        assert!(m.get("llm").is_none());
        assert!(m.get("net").is_none());
        if name == "watch-file" || name == "watch-git" {
            assert_eq!(
                m["params"]["every"],
                json!({"type":"duration","min":"5m","max":"1d"})
            );
            assert_eq!(m["trigger"]["schedule"]["interval"], "$params.every");
            let parameter = if name == "watch-file" { "path" } else { "repo" };
            assert_eq!(m["params"][parameter]["under"], json!(["~/Work"]));
        }
    }
}

#[test]
fn file_conditions_wait_then_notify_once_and_end() {
    for condition in ["exists", "contains", "lacks"] {
        let t = Test::new();
        let path = t.work.0.join("future/file");
        let instance = t.create("watch-file", file_params(&path, condition, "READY"));
        t.run(&instance, "missing");
        assert!(t.items().is_empty());
        t.live(&instance, true);
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        if condition != "exists" {
            std::fs::write(
                &path,
                if condition == "contains" {
                    "not ready"
                } else {
                    "READY"
                },
            )
            .unwrap();
            t.run(&instance, "not-met");
            assert!(t.items().is_empty());
            t.live(&instance, true);
        }
        std::fs::write(
            &path,
            if condition == "lacks" {
                "done"
            } else {
                "READY"
            },
        )
        .unwrap();
        let run = t.run(&instance, "met");
        let item = t.notified(&instance, &run, "File");
        let mut expected = instance["params"].clone();
        expected.as_object_mut().unwrap().remove("note");
        assert_eq!(item["data"]["params"], expected);
        assert_eq!(item["data"]["observed"]["exists"], true);
        if condition != "exists" {
            assert_eq!(
                item["data"]["observed"]["contains"],
                condition == "contains"
            );
        }
    }
}

#[test]
fn file_empty_content_condition_is_stopped_as_misconfigured() {
    for condition in ["contains", "lacks"] {
        let t = Test::new();
        let instance = t.create(
            "watch-file",
            file_params(&t.work.0.join("file"), condition, ""),
        );
        let run = t.run(&instance, "misconfigured");
        let item = t.notified(&instance, &run, "stopped: misconfigured");
        assert_eq!(item["data"]["observed"]["reason"], "empty_text");
        assert!(t.host.mock.builtin_sends().is_empty());
    }
}

#[test]
fn file_transient_and_current_content_failures_retry_without_ending() {
    for code in ["io_error", "not_found", "too_large", "not_utf8", "timeout"] {
        let t = Test::new();
        let path = t.work.0.join("file");
        std::fs::write(&path, "READY").unwrap();
        let instance = t.create("watch-file", file_params(&path, "contains", "READY"));
        t.host.reject(Primitive::FsRead, code);
        t.run(&instance, "unreadable");
        assert!(t.items().is_empty(), "{code}");
        t.live(&instance, true);
        let run = t.run(&instance, "readable");
        t.notified(&instance, &run, "contains");
        assert!(t.host.overrides.lock().unwrap().is_empty());
    }
}

#[test]
#[cfg(unix)]
fn file_lost_parameter_authority_stops_and_ends() {
    let t = Test::new();
    let path = t.work.0.join("file");
    std::fs::write(&path, "approved").unwrap();
    let instance = t.create("watch-file", file_params(&path, "exists", ""));
    let outside = t.f.dir.join("outside");
    std::fs::write(&outside, "not approved").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(outside, path).unwrap();
    let run = t.run(&instance, "escaped");
    let item = t.notified(&instance, &run, "stopped: denied");
    assert_eq!(item["data"]["observed"]["reason"], "denied");
}

#[test]
fn git_ref_waits_for_branch_then_notifies_once() {
    let t = Test::new();
    let repo = repo(&t);
    let instance = t.create(
        "watch-git",
        git_params(&repo, "ref_exists", "refs/heads/ready", ""),
    );
    t.run(&instance, "absent-ref");
    assert!(t.items().is_empty());
    t.live(&instance, true);
    git(&repo, &["branch", "ready"]);
    let run = t.run(&instance, "present-ref");
    let item = t.notified(&instance, &run, "Git ref exists");
    assert_eq!(
        item["data"]["observed"]["sha"],
        git(&repo, &["rev-parse", "HEAD"])
    );
    assert_eq!(item["links"], json!([]));
}

#[test]
fn git_existing_match_is_baselined_and_complete_nonmatches_advance_it() {
    let t = Test::new();
    let repo = repo(&t);
    let instance = t.create(
        "watch-git",
        git_params(&repo, "commit_matching", "", "Release"),
    );
    let start = git(&repo, &["rev-parse", "HEAD"]);
    t.run(&instance, "first");
    assert_eq!(baseline(&t, &instance), start);
    assert!(t.items().is_empty());
    t.live(&instance, true);
    git(
        &repo,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "release lowercase",
            "-m",
            "Release in the body only",
        ],
    );
    let next = git(&repo, &["rev-parse", "HEAD"]);
    t.run(&instance, "case-sensitive-subject-only");
    assert_eq!(baseline(&t, &instance), next);
    assert!(t.items().is_empty());
    t.live(&instance, true);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "Release new version"],
    );
    let run = t.run(&instance, "new-match");
    let item = t.notified(&instance, &run, "Matching Git commit landed");
    assert_eq!(item["data"]["observed"]["subject"], "Release new version");
    assert_eq!(item["data"]["observed"]["baseline"], next);
}

#[test]
fn git_transient_failure_retries_without_setting_a_baseline() {
    let t = Test::new();
    let repo = repo(&t);
    let instance = t.create(
        "watch-git",
        git_params(&repo, "commit_matching", "", "Release"),
    );
    t.host.reject(Primitive::GitRevParse, "timeout");
    t.run(&instance, "timeout");
    assert!(t.items().is_empty());
    t.live(&instance, true);
    assert_eq!(
        t.f.module
            .rt
            .store()
            .read(|conn| Ok(conn.query_row(
                "SELECT count(*) FROM kv WHERE flow_id=?1",
                [instance["flow_id"].as_str().unwrap()],
                |row| row.get::<_, i64>(0),
            )?))
            .unwrap(),
        0
    );
    t.run(&instance, "baseline");
    assert_eq!(baseline(&t, &instance), git(&repo, &["rev-parse", "HEAD"]));
    assert!(t.items().is_empty());
}

#[test]
fn git_disappeared_repository_writes_stopped_and_ends() {
    let t = Test::new();
    let repo = repo(&t);
    let instance = t.create("watch-git", git_params(&repo, "ref_exists", "", ""));
    std::fs::remove_dir_all(&repo).unwrap();
    let run = t.run(&instance, "gone");
    let item = t.notified(&instance, &run, "stopped: denied");
    assert_eq!(item["data"]["observed"]["reason"], "denied");
}

#[test]
fn git_rewritten_history_stops_instead_of_matching_older_commits() {
    let t = Test::new();
    let repo = repo(&t);
    let old = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "New baseline"],
    );
    let instance = t.create(
        "watch-git",
        git_params(&repo, "commit_matching", "", "Release"),
    );
    t.run(&instance, "baseline");
    git(&repo, &["reset", "--hard", &old]);
    let run = t.run(&instance, "rewound");
    let item = t.notified(&instance, &run, "stopped: watched history was rewritten");
    assert_eq!(item["data"]["observed"]["reason"], "history_rewritten");
}

#[test]
fn git_full_scan_cap_stops_but_a_match_at_the_cap_still_fires() {
    for matched in [false, true] {
        let t = Test::new();
        let repo = repo(&t);
        let instance = t.create(
            "watch-git",
            git_params(&repo, "commit_matching", "", "Release"),
        );
        t.run(&instance, "baseline");
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "Next"]);
        t.host.response(Primitive::GitLog, json!([]));
        let mut entries: Vec<Value> = (0..500)
            .map(|n| {
                json!({
                    "sha":format!("{n:040x}"),"subject":"Unrelated","author_ms":common::T0,
                })
            })
            .collect();
        if matched {
            entries[499]["subject"] = json!("Release found at the cap");
        }
        t.host.response(Primitive::GitLog, json!(entries));
        let run = t.run(&instance, "cap");
        let item = t.notified(
            &instance,
            &run,
            if matched {
                "Matching"
            } else {
                "stopped: more than 500"
            },
        );
        assert!(t.host.overrides.lock().unwrap().is_empty());
        if matched {
            assert_eq!(
                item["data"]["observed"]["subject"],
                "Release found at the cap"
            );
        } else {
            assert_eq!(item["data"]["observed"]["reason"], "history_limit");
        }
    }
}

#[test]
fn github_pr_filters_headers_number_kind_and_state_before_notifying() {
    for list_item in [false, true] {
        let t = Test::new();
        let instance = t.create("watch-github", github_params("pr", "merged"));
        let mut open: Value = serde_json::from_str(PR).unwrap();
        open["state"] = json!("open");
        open["merged"] = json!(false);
        open["merged_at"] = Value::Null;
        t.event(
            &instance,
            "github_pr_changed",
            "other/project",
            open.clone(),
        );
        let mut wrong_number = open.clone();
        wrong_number["number"] = json!(43);
        t.event(
            &instance,
            "github_pr_changed",
            "example/project",
            wrong_number,
        );
        t.event(
            &instance,
            "github_issue_changed",
            "example/project",
            open.clone(),
        );
        t.event(&instance, "github_pr_changed", "example/project", open);
        t.event(&instance, "github_pr_changed", "example/project", json!({}));
        assert!(t.items().is_empty());
        t.live(&instance, true);
        let mut met: Value = serde_json::from_str(PR).unwrap();
        if list_item {
            met.as_object_mut().unwrap().remove("merged");
            met["merged_at"] = json!("2026-05-01T00:00:00Z");
        }
        let run = t.event(
            &instance,
            "github_pr_changed",
            "example/project",
            met.clone(),
        );
        let item = t.notified(&instance, &run, "GitHub PR merged");
        assert_eq!(item["data"]["observed"]["merged"], true);
        assert_eq!(item["data"]["observed"]["number"], 42);
        assert_eq!(item["links"], json!([{"kind":"url","url":met["html_url"]}]));
        assert_eq!(t.host.resolved.load(Ordering::Relaxed), 6);
    }
}

#[test]
fn github_closed_pr_or_issue_and_any_change_notify_once() {
    for (kind, state, fixture) in [
        ("pr", "closed", PR),
        ("issue", "closed", ISSUE),
        ("pr", "any_change", PR),
        ("issue", "any_change", ISSUE),
    ] {
        let t = Test::new();
        let instance = t.create("watch-github", github_params(kind, state));
        let name = if kind == "pr" {
            "github_pr_changed"
        } else {
            "github_issue_changed"
        };
        let mut body: Value = serde_json::from_str(fixture).unwrap();
        t.event(&instance, name, "other/project", body.clone());
        assert!(t.items().is_empty());
        t.live(&instance, true);
        if state == "closed" {
            let mut open = body.clone();
            open["state"] = json!("open");
            t.event(&instance, name, "example/project", open);
            assert!(t.items().is_empty());
            t.live(&instance, true);
        } else {
            body.as_object_mut().unwrap().remove("state");
            body.as_object_mut().unwrap().remove("merged");
        }
        let run = t.event(&instance, name, "example/project", body.clone());
        let item = t.notified(
            &instance,
            &run,
            if state == "closed" {
                "closed"
            } else {
                "changed"
            },
        );
        assert_eq!(item["links"][0]["url"], body["html_url"]);
    }
}

#[test]
fn github_release_waits_for_its_repository_and_ignores_number_and_state() {
    let t = Test::new();
    let mut params = github_params("release", "merged");
    params["number"] = json!(10000000);
    let instance = t.create("watch-github", params);
    let body: Value = serde_json::from_str(RELEASE).unwrap();
    t.event(
        &instance,
        "github_release_published",
        "other/project",
        body.clone(),
    );
    t.event(
        &instance,
        "github_pr_changed",
        "example/project",
        serde_json::from_str(PR).unwrap(),
    );
    assert!(t.items().is_empty());
    t.live(&instance, true);
    let run = t.event(
        &instance,
        "github_release_published",
        "example/project",
        body.clone(),
    );
    let item = t.notified(&instance, &run, "GitHub release published");
    assert_eq!(item["data"]["observed"]["tag_name"], "v1.0.0");
    assert_eq!(item["links"][0]["url"], body["html_url"]);
}

#[test]
fn github_issue_cannot_merge_stops_on_its_first_subscribed_event() {
    let t = Test::new();
    let instance = t.create("watch-github", github_params("issue", "merged"));
    let run = t.event(
        &instance,
        "github_release_published",
        "other/project",
        serde_json::from_str(RELEASE).unwrap(),
    );
    let item = t.notified(&instance, &run, "stopped: issues cannot merge");
    assert_eq!(item["data"]["observed"]["reason"], "issue_cannot_merge");
}

#[test]
fn reminder_waits_until_due_then_catches_up_once_and_ends() {
    let t = Test::new();
    let at = jiff::Timestamp::from_millisecond(t.f.clock.now_ms() + 60_000)
        .unwrap()
        .to_string();
    let instance = t.create("remind-at", json!({"at":at,"note":"Check the rollout"}));
    assert!(
        t.f.module
            .rt
            .scheduler()
            .unwrap()
            .tick()
            .unwrap()
            .new_runs()
            .is_empty()
    );
    assert!(t.items().is_empty());
    t.live(&instance, true);
    t.f.clock.advance(180_000);
    let tick = t.f.module.rt.scheduler().unwrap().tick().unwrap();
    assert_eq!(tick.new_runs().len(), 1);
    let run = tick.new_runs()[0].to_owned();
    t.finish(&run);
    let item = t.notified(&instance, &run, "Reminder time reached");
    assert_eq!(item["data"]["observed"]["due"], at);
    assert!(
        t.f.module
            .rt
            .scheduler()
            .unwrap()
            .tick()
            .unwrap()
            .new_runs()
            .is_empty()
    );
}

#[test]
fn reminder_past_invalid_or_over_ceiling_timestamp_is_refused_at_creation() {
    let t = Test::new();
    for at in [
        "not-a-timestamp".into(),
        jiff::Timestamp::from_millisecond(t.f.clock.now_ms() - 1)
            .unwrap()
            .to_string(),
        jiff::Timestamp::from_millisecond(t.f.clock.now_ms() + 365 * 86_400_000 + 1)
            .unwrap()
            .to_string(),
    ] {
        let error = t.f.module.handle(&agent(OWNER), "flow.instantiate", json!({
            "template":"remind-at","version":1,"params":{"at":at,"note":"Never delivered"},"expires_in":"365d",
        })).unwrap_err();
        assert_eq!(error.code, "param_out_of_bounds");
        assert_eq!(error.detail.unwrap()["param"], "at");
    }
    assert!(t.items().is_empty());
    assert_eq!(
        t.f.module.rt.live_instances().unwrap()["instances"],
        json!([])
    );
}

#[test]
fn all_four_templates_dry_run_successfully_in_capture_mode() {
    let t = Test::new();
    let repo = repo(&t);
    t.dry(
        "watch-file",
        file_params(&t.work.0.join("future"), "contains", "READY"),
        json!({}),
    );
    t.dry(
        "watch-git",
        git_params(&repo, "commit_matching", "", "Release"),
        json!({}),
    );
    t.dry(
        "watch-github",
        github_params("pr", "merged"),
        json!({"event":{
            "module":"plexus","name":"github_pr_changed","version":1,"event_key":"0".repeat(64),
            "headers":{"repo":"example/project"},"body":serde_json::from_str::<Value>(PR).unwrap(),
        }}),
    );
    let at = jiff::Timestamp::from_millisecond(t.f.clock.now_ms() + 60_000)
        .unwrap()
        .to_string();
    t.dry(
        "remind-at",
        json!({"at":at,"note":"Preview reminder"}),
        json!({}),
    );
    assert!(t.items().is_empty());
    assert_eq!(
        t.f.module.rt.live_instances().unwrap()["instances"],
        json!([])
    );
}

#[test]
fn exact_file_and_git_template_roots_refuse_values_outside_work() {
    let t = Test::new();
    let outside = t.f.dir.join("outside");
    std::fs::create_dir(&outside).unwrap();
    git(&outside, &["init", "-q"]);
    for (template, values, param) in [
        (
            "watch-file",
            file_params(&outside.join("future"), "exists", ""),
            "path",
        ),
        (
            "watch-git",
            git_params(&outside, "ref_exists", "", ""),
            "repo",
        ),
    ] {
        let error =
            t.f.module
                .handle(
                    &agent(OWNER),
                    "flow.instantiate",
                    json!({
                        "template":template,"version":1,"params":values,"expires_in":"1d",
                    }),
                )
                .unwrap_err();
        assert_eq!(error.code, "param_out_of_bounds");
        assert_eq!(error.detail.unwrap()["param"], param);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn a_quote_filled_note_and_very_long_path_still_deliver_with_bounded_metadata() {
    let t = Test::new();
    // A long future path need not exist to instantiate. A synthetic stat
    // isolates digest sizing from creating many directories on the host.
    let path = t.work.0.join(
        std::iter::repeat_n("\"".repeat(200), 19)
            .collect::<Vec<_>>()
            .join("/"),
    );
    let mut params = file_params(&path, "exists", &"\"".repeat(1024));
    params["note"] = json!("\"".repeat(4096));
    let instance = t.create("watch-file", params);
    t.host.response(
        Primitive::FsStat,
        json!({"exists":true,"kind":"file","size":0,"mtime_ms":common::T0}),
    );
    let run = t.run(&instance, "large-escaped-values");
    let item = t.notified(&instance, &run, "File appeared");
    assert_eq!(item["body"].as_str().unwrap().len(), 4096);
    let original = instance["params"]["path"].as_str().unwrap();
    let shortened = item["data"]["params"]["path"].as_str().unwrap();
    assert!(shortened.len() < original.len());
    assert!(original.starts_with(shortened));
    assert_eq!(
        item["data"]["truncated"]["params.path"]["original_bytes"],
        original.len()
    );
    assert!(t.host.overrides.lock().unwrap().is_empty());
}

#[test]
fn digest_helper_bounds_escaped_note_long_metadata_and_links_on_every_platform() {
    let t = Test::new();
    let note = "\"".repeat(4096);
    let path = format!("/long/{}", "\"é🦀".repeat(2000));
    let url = format!(
        "https://github.com/example/project/pull/42?{}",
        "x".repeat(20000)
    );
    // Exercise the exact shared helper without pathname validation: macOS
    // refuses future paths above its smaller OS pathname limit at creation.
    let helper = TEMPLATES[0].2.split("// END boundedDigest").next().unwrap();
    let script = format!(
        "const params = {};\n{helper}\nawait finishWatch('File appeared', {{exists: true}}, [{{kind: 'url', url: {}}}]);",
        json!({"note":note,"path":path}),
        json!(url),
    );
    let manifest = json!({
        "id":"digest-helper","version":1,"purpose":"Exercise the published digest helper",
        "trigger":{"schedule":{"interval":"1h"}},"sinks":[{"agent":"$self","digest_max":"wake"}],
        "params":{"note":{"type":"string","max_bytes":4096}},
        "template":{"max_per_agent":100,"max_lifetime":"365d"},
    });
    t.f.module
        .handle(
            &Caller::Operator,
            "package.register",
            json!({"script":script,"manifest":manifest.to_string()}),
        )
        .unwrap();
    let instance = t.create("digest-helper", json!({"note":note}));
    let run = t.run(&instance, "bounded-metadata");
    let item = t.notified(&instance, &run, "File appeared");
    assert_eq!(item["body"], note);
    assert_eq!(
        item["data"]["truncated"]["params.path"]["original_bytes"],
        path.len()
    );
    assert_eq!(
        item["data"]["truncated"]["link_url"]["original_bytes"],
        url.len()
    );
    assert!(path.starts_with(item["data"]["params"]["path"].as_str().unwrap()));
    assert_eq!(item["links"], json!([]));
}
