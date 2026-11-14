//! Exercise the published docs-proposal template with Magic Context's real
//! sample notices, query replies, and capture-only dry runs.
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use basal_core::{NoHooks, RunState};
use basal_host::mock::MockHost;
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, EventBody, EventDecl, EventOrigin, Host,
    HostOutcome, InstallStatus, MockCatalog, MockConsent, OpDecl, OpKind, TransportError,
};
use basal_module::{caller::Caller, module::Hosts};
use basal_proto::{CallKind, JsonText, Settlement};
use common::{Fixture, Options, agent, fixture};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MANIFEST: &str = include_str!("../../../templates/docs-proposals/manifest.json");
const SCRIPT: &str = include_str!("../../../templates/docs-proposals/flow.js");
const OWNER: &str = "agent_47120287c700722b";
const PROJECT: &str = "pj-docs";
const INSTRUCTION: &str = "Read .cortexkit/magic-context/docs-update-proposals/1.md, then apply the changes you agree with by editing ARCHITECTURE.md and STRUCTURE.md. Magic Context marks the proposal applied when the files match; the person can dismiss it in Alfonso Desktop.";
const TITLE: &str = "Docs proposal for pj-docs: 1 change in ARCHITECTURE.md";

fn samples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/mc-docs-proposal-samples-v1")
}

fn sample(name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(samples_dir().join(name)).unwrap()).unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn text(value: Value) -> JsonText {
    JsonText::new(value.to_string()).unwrap()
}

fn pending_body() -> Value {
    serde_json::from_str(
        sample("events-get-pending.json")["response"]["result"]["body"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
}

fn expected_body() -> String {
    format!(
        "ARCHITECTURE.md: 1 change\n- replace ## Core — src/parser.rs moved to src/parse/.\n{INSTRUCTION}"
    )
}

struct DocsHost {
    mock: MockHost,
    events: Value,
    state: Value,
    requests: Mutex<Vec<Value>>,
}

impl DocsHost {
    fn new(state: &str) -> Self {
        Self {
            mock: MockHost::default(),
            events: sample(&format!("events-get-{state}.json")),
            state: sample(&format!("state-{state}.json")),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn answer(&self, request: &CallRequest) -> Option<HostOutcome> {
        let CallKind::Op { module, op } = &request.kind else {
            return None;
        };
        assert_eq!(module, "magic-context", "unexpected operation: {request:?}");
        let fixture = match op.as_str() {
            "events_get" => &self.events,
            "docs_proposals.state" => &self.state,
            _ => panic!("unexpected docs-proposal operation: {request:?}"),
        };
        let args: Value = serde_json::from_str(request.args.as_str()).unwrap();
        let actual = json!({"method":op,"params":args});
        assert_eq!(actual, fixture["request"]);
        self.requests.lock().unwrap().push(actual);
        let response = fixture["response"].clone();
        Some(
            if response.get("error").is_some() || response.get("code").is_some() {
                HostOutcome::rejected(text(response))
            } else {
                HostOutcome::fulfilled(text(response))
            },
        )
    }
}

impl Host for DocsHost {
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
    host: Arc<DocsHost>,
}

impl Test {
    fn new(state: &str) -> Self {
        Self::with_host(DocsHost::new(state))
    }

    fn with_host(host: DocsHost) -> Self {
        let host = Arc::new(host);
        let catalog = MockCatalog::standard();
        catalog.add_named_agent(OWNER, "Owner");
        catalog.set_flow_capable("magic-context", true);
        for op in ["events_get", "docs_proposals.state"] {
            catalog.set_op(
                "magic-context",
                op,
                OpDecl {
                    kind: Some(OpKind::Query),
                    cause_echo: false,
                    shell_capable: false,
                },
            );
        }
        catalog.set_event(
            "magic-context",
            "docs_proposal",
            1,
            EventDecl {
                headers: vec!["event_key".into(), "project_id".into()],
                origin: EventOrigin::External,
                body: EventBody::Resolved {
                    resolve_op: "events_get".into(),
                },
            },
        );
        let f = fixture(
            "docs-proposals-template",
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
        let registered = f
            .module
            .handle(
                &Caller::Operator,
                "package.register",
                json!({"manifest":MANIFEST,"script":SCRIPT}),
            )
            .unwrap();
        assert_eq!(registered["package"], "docs-proposals");
        let stored = f.module.rt.get_package("docs-proposals", 1).unwrap();
        assert_eq!(stored["manifest"], MANIFEST);
        assert_eq!(stored["script"], SCRIPT);
        Self { f, host }
    }

    fn instance(&self) -> Value {
        self.f
            .module
            .handle(
                &agent(OWNER),
                "flow.instantiate",
                json!({"template":"docs-proposals","version":1,
                    "params":{"project_id":PROJECT},"expires_in":"365d"}),
            )
            .unwrap()
    }

    fn admit(&self, notice: &Value) -> usize {
        self.host.mock.set_clock(self.f.clock.now_ms() as f64);
        assert_eq!(notice["body"], "");
        let headers: BTreeMap<String, String> =
            serde_json::from_value(notice["headers"].clone()).unwrap();
        self.f
            .module
            .rt
            .admit_events(&[basal_core::events::Notice {
                published_at_ms: self.f.clock.now_ms(),
                subject: notice["subject"].as_str().unwrap().to_owned(),
                event_key: headers["event_key"].clone(),
                digest: headers["Ck-Content-Digest"].clone(),
                headers,
            }])
            .unwrap()
            .admitted
    }

    fn run_one(&self, instance: &Value, expected: RunState) -> String {
        let mut startable = self.f.module.rt.startable().unwrap();
        assert_eq!(startable.len(), 1, "{startable:?}");
        let (run, _) = startable.pop().unwrap();
        assert_eq!(
            self.f.module.rt.run(&run).unwrap().flow_id,
            instance["flow_id"]
        );
        self.f.module.engine.run_until_idle(100).unwrap();
        let done = self.f.module.rt.run(&run).unwrap();
        assert_eq!(done.state, expected, "{done:#?}");
        run
    }

    fn requests(&self) -> Vec<Value> {
        self.host.requests.lock().unwrap().clone()
    }

    fn health(&self, instance: &Value) -> basal_core::ops::FlowHealth {
        self.f
            .module
            .rt
            .flow_health()
            .unwrap()
            .into_iter()
            .find(|h| h.flow_id == instance["flow_id"])
            .unwrap()
    }

    fn assert_no_failure_latch(&self, instance: &Value) {
        let latch: (i64, Option<i64>, Option<i64>) = self.f.module.rt.store().read(|c| {
            Ok(c.query_row(
                "SELECT episode,failing_episode,failing_since_ms FROM flow_owner_notice_latches WHERE flow_id=?1",
                [instance["flow_id"].as_str().unwrap()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        }).unwrap();
        assert_eq!(latch, (0, None, None));
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
        assert_eq!(
            items[0]["replace_key"], "docs-proposal:pj-docs",
            "{items:#?}"
        );
        items[0]["item"].clone()
    }
}

#[test]
fn sample_files_match_upstream_checksums() {
    let dir = samples_dir();
    let sums = std::fs::read(dir.join("SHA256SUMS")).unwrap();
    assert_eq!(
        sha256(&sums),
        "e00943abab3071d94f2ba8e207feef8d581da8bf7f5ae828e2be31f6cf46f972"
    );
    let mut covered = BTreeSet::new();
    for line in std::str::from_utf8(&sums).unwrap().lines() {
        let (expected, name) = line.split_once("  ").unwrap();
        assert!(
            covered.insert(name.to_owned()),
            "duplicate checksum: {name}"
        );
        assert_eq!(
            sha256(&std::fs::read(dir.join(name)).unwrap()),
            expected,
            "{name}"
        );
    }
    assert_eq!(covered.len(), 10);
    // Upstream deliberately leaves its README out of SHA256SUMS; pin it separately.
    assert_eq!(
        sha256(&std::fs::read(dir.join("README.md")).unwrap()),
        "b42daab8622e0584ca453783817f093b594affe752a5136aa1df8f48c4675f58"
    );
    covered.extend(["SHA256SUMS".to_owned(), "README.md".to_owned()]);
    let actual: BTreeSet<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(actual, covered);
}

#[test]
fn manifest_passes_new_version_intake_and_instantiates_project() {
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    assert!(
        manifest["explanation"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert!(manifest.get("changes").is_none());
    let t = Test::new("pending");
    assert_eq!(t.instance()["params"], json!({"project_id":"pj-docs"}));
}

fn assert_body_digest(events: &Value, notice: &Value) {
    let body = events["response"]["result"]["body"].as_str().unwrap();
    let digest = notice["headers"]["Ck-Content-Digest"].as_str().unwrap();
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert_eq!(sha256(body.as_bytes()), digest);
    assert_eq!(events["response"]["result"]["digest"], digest);
}

#[test]
fn pending_sample_wakes_creating_agent_with_full_proposal_and_instruction() {
    let t = Test::new("pending");
    let instance = t.instance();
    let notice = sample("notice.json");
    assert_body_digest(&t.host.events, &notice);
    assert_eq!(t.admit(&notice), 1);
    t.run_one(&instance, RunState::Succeeded);
    assert_eq!(
        t.requests(),
        vec![
            t.host.events["request"].clone(),
            t.host.state["request"].clone()
        ]
    );
    assert_eq!(t.requests()[1]["params"]["proposal_id"].as_i64(), Some(1));
    let item = t.one_item();
    assert_eq!(item["title"], TITLE);
    assert_eq!(item["body"], expected_body());
    assert_eq!(
        item["body"].as_str().unwrap().lines().last(),
        Some(INSTRUCTION)
    );
    assert_eq!(
        item["data"],
        json!({
            "proposal_id":1,"project_id":"pj-docs",
            "file":".cortexkit/magic-context/docs-update-proposals/1.md"
        })
    );
}

#[test]
fn non_pending_sample_states_succeed_without_wake_or_failure_latch() {
    for state in ["stale", "applied", "dismissed"] {
        let t = Test::new(state);
        let instance = t.instance();
        let notice = sample("notice.json");
        assert_body_digest(&t.host.events, &notice);
        assert_eq!(t.admit(&notice), 1, "{state}");
        t.run_one(&instance, RunState::Succeeded);
        assert_eq!(
            t.requests(),
            vec![
                t.host.events["request"].clone(),
                t.host.state["request"].clone()
            ],
            "{state}"
        );
        assert!(t.items().is_empty(), "{state}: {:?}", t.items());
        let health = t.health(&instance);
        assert_eq!(health.consecutive_failures, 0, "{state}");
        assert_eq!(health.failure_first_seen_ms, None, "{state}");
        assert!(health.enabled && !health.auto_disabled, "{state}");
        t.assert_no_failure_latch(&instance);
    }
}

#[test]
fn another_projects_notice_is_filtered_before_admission() {
    let t = Test::new("pending");
    t.instance();
    let mut notice = sample("notice.json");
    notice["headers"]["project_id"] = json!("pj-other");
    assert_eq!(t.admit(&notice), 0);
    assert!(t.f.module.rt.startable().unwrap().is_empty());
    assert!(t.requests().is_empty());
    assert!(t.items().is_empty());
}

#[test]
fn unknown_key_fails_with_event_not_visible_before_state_query() {
    let mut host = DocsHost::new("pending");
    host.events = sample("events-get-unknown-key.json");
    let mut notice = sample("notice.json");
    let key = host.events["request"]["params"]["event_key"].clone();
    notice["headers"]["event_key"] = key.clone();
    notice["headers"]["Nats-Msg-Id"] = key;
    let t = Test::with_host(host);
    let instance = t.instance();
    assert_eq!(t.admit(&notice), 1);
    let run = t.run_one(&instance, RunState::Failed);
    let done = t.f.module.rt.run(&run).unwrap();
    assert_eq!(done.error_kind.as_deref(), Some("event_not_visible"));
    assert_eq!(t.requests(), vec![t.host.events["request"].clone()]);
    assert!(t.items().is_empty());
}

#[test]
fn state_query_refusal_other_than_captured_still_fails_the_run() {
    let mut host = DocsHost::new("pending");
    host.state["response"] = json!({
        "code":"flow_route_not_admitted", "message":"This route cannot call the operation."
    });
    let t = Test::with_host(host);
    let instance = t.instance();
    assert_eq!(t.admit(&sample("notice.json")), 1);
    let run = t.run_one(&instance, RunState::Failed);
    let done = t.f.module.rt.run(&run).unwrap();
    assert_eq!(done.error_kind.as_deref(), Some("script"));
    assert_eq!(
        done.error_detail.as_deref(),
        Some("HostError: This route cannot call the operation.")
    );
    let calls = t.f.module.rt.calls(&run).unwrap();
    let outcome = calls.last().unwrap().outcome.as_ref().unwrap();
    assert_eq!(outcome.settlement, Settlement::Rejected);
    let refusal: Value = serde_json::from_str(outcome.value.as_str()).unwrap();
    assert_eq!(refusal, t.host.state["response"]);
    assert_eq!(
        t.requests(),
        vec![
            t.host.events["request"].clone(),
            t.host.state["request"].clone()
        ]
    );
    assert!(t.items().is_empty());
}

#[test]
fn long_pending_proposal_clips_body_but_preserves_instruction() {
    let mut host = DocsHost::new("pending");
    let mut proposal = pending_body();
    proposal["documents"][0]["changes"] = Value::Array(
        (0..30)
            .map(|n| {
                json!({
                    "action":"replace", "heading":format!("## Section {n}"),
                    "reason":"The parser moved to a new module. ".repeat(100)
                })
            })
            .collect(),
    );
    let bytes = proposal.to_string();
    let digest = sha256(bytes.as_bytes());
    host.events["response"]["result"]["body"] = json!(bytes);
    host.events["response"]["result"]["digest"] = json!(digest);
    let mut notice = sample("notice.json");
    notice["headers"]["Ck-Content-Digest"] = json!(digest);
    assert_body_digest(&host.events, &notice);
    let t = Test::with_host(host);
    let instance = t.instance();
    assert_eq!(t.admit(&notice), 1);
    t.run_one(&instance, RunState::Succeeded);
    let item = t.one_item();
    assert_eq!(
        item["title"],
        "Docs proposal for pj-docs: 30 changes in ARCHITECTURE.md"
    );
    let body = item["body"].as_str().unwrap();
    assert_eq!(body.encode_utf16().count(), 4000);
    assert!(body.starts_with("ARCHITECTURE.md: 30 changes\n- replace ## Section 0 — "));
    assert!(body.strip_suffix(INSTRUCTION).unwrap().ends_with("…\n"));
    assert_eq!(body.lines().last(), Some(INSTRUCTION));
}

#[test]
fn capture_dry_run_records_state_query_and_expected_wake_without_sending() {
    let t = Test::new("pending");
    let sends = t.host.mock.total_sends();
    let reply =
        t.f.module
            .handle(
                &agent(OWNER),
                "flow.dry_run",
                json!({
                    "flow_id":"docs-proposals","params":{"project_id":PROJECT},
                    "trigger":{"event":{"module":"magic-context","name":"docs_proposal","version":1,
                        "headers":sample("notice.json")["headers"],"body":pending_body()}}
                }),
            )
            .unwrap();
    assert_eq!(reply["summary"]["mode"], "capture");
    let runs = reply["summary"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "{reply:#}");
    let run = &runs[0];
    assert_eq!(run["state"], "succeeded", "{reply:#}");
    assert!(run["error"].is_null(), "{reply:#}");
    assert_eq!(run["calls_total"], 2, "{reply:#}");
    let calls = run["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "{reply:#}");
    for call in calls {
        assert_eq!(call["action"], "captured", "{call:#}");
        assert_eq!(call["outcome"]["rejected"], "captured", "{call:#}");
    }
    assert_eq!(calls[0]["kind"], "op");
    assert_eq!(calls[0]["module"], "magic-context");
    assert_eq!(calls[0]["op"], "docs_proposals.state");
    assert_eq!(calls[0]["args"], t.host.state["request"]["params"]);
    assert_eq!(calls[1]["kind"], "sink.digest");
    assert_eq!(calls[1]["args"]["agent"], OWNER);
    assert_eq!(calls[1]["args"]["action"], "wake");
    assert_eq!(
        calls[1]["args"]["options"]["replace_key"],
        "docs-proposal:pj-docs"
    );
    assert_eq!(calls[1]["args"]["item"]["title"], TITLE);
    assert_eq!(calls[1]["args"]["item"]["body"], expected_body());
    assert_eq!(calls[1]["sink"]["effective"], "wake");
    assert_eq!(t.host.mock.total_sends(), sends);
    assert!(t.requests().is_empty());
    assert!(t.items().is_empty());
}
