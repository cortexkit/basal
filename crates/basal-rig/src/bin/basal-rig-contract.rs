//! `basal-rig-contract`: basal's live contract suite against the real
//! prefrontal-core on the ckdev-flows rig (`script/flows-rig.sh` builds and
//! describes the rig).
//!
//! `script/flows-rig.sh test` runs it, in the rig's environment, against a
//! started rig:
//!
//! ```text
//! basal-rig-contract --core-store <file> --basal-store <file>
//!     --machine-id <file> --kill-file <file> --unscoped-file <file> --results <file>
//!     --project-id <pj-…> --broca-index <file> [--no-kill-hook] [--models]
//!     --project-root <rig project directory>
//! ```
//!
//! `--no-kill-hook` is for a rig running a staged production ck-basal
//! (`flows-rig.sh place --from-stage`), which is built without the rig kill
//! switches: the crash and unscoped-send cases cannot run there, so each is reported as not run,
//! by name and with the reason, and never counted as passed.
//!
//! The rig runs core's projects registry as production does, against the
//! rig's entorhinal, so the test agent is a head of the project
//! `--project-id` names (flows-rig.sh registers it there). Everything the
//! agent does goes through core's `flow.relay`, as a head's `flow` tool does.
//!
//! The connection file comes from `SUBC_CONNECTION_FILE`, which the rig sets.
//! Every check is printed as it runs and written, with the replies core and
//! basal gave, to the results file. The exit status is 0 only when every
//! check passed. Model cases are opt-in with `--models` after the script has
//! verified the rig's isolated credential. Every model flow is installed by
//! the test agent through core's relay, never as a local caller.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use basal_rig::client::{Client, Relayed, Reply, evidence, suite_identity};
use basal_rig::flows::{self, Flow};
use basal_rig::models::{
    basal_first_principal, frozen_send, no_tools, non_carrier_refused, result_matches_transcript,
    scope_matches, selected_luna, settled_usage, stamped_flow, unscoped_send_refused,
};
use basal_rig::stores::{
    BasalStore, BrocaStore, Call, CoreStore, KIND_SINK_DIGEST, KIND_SINK_STATUS, Receipt, Run,
};
use serde_json::{Map, Value, json};
use subc_protocol::BindIdentity;

#[path = "basal-rig-contract/ownership.rs"]
mod ownership;
#[path = "basal-rig-contract/package_cases.rs"]
mod package_cases;
#[path = "basal-rig-contract/seeded.rs"]
mod seeded;

const CORE: &str = "prefrontal-core";
const BASAL: &str = "basal";
/// How long a flow approved now may take to finish its first run: up to a
/// minute to its cron boundary, plus the run itself.
const RUN_WAIT: Duration = Duration::from_secs(150);
/// The crash case's name, shared by the case and its not-run record.
const CRASH_CASE: &str = "exactly once across a kill -9 of ck-basal";
const NO_KILL_HOOK_REASON: &str =
    "the staged ck-basal is a production build without the rig-only hooks";
const MODEL_CASES: [&str; 9] = [
    "models: first minimal Luna call",
    "models: routing selection journaled before dispatch",
    "models: run.result text and run.status usage settlement",
    "models: classify returns a label",
    "models: token cap refuses before send",
    "models: crash recovers Broca result without a second run",
    "models: flow calls carry no tools",
    "models: a direct non-carrier cannot open the registered flow scope",
    "models: Broca refuses an unscoped basal send without starting a run",
];

struct Args {
    connection_file: PathBuf,
    core_store: PathBuf,
    basal_store: PathBuf,
    broca_index: PathBuf,
    machine_id: PathBuf,
    kill_file: PathBuf,
    unscoped_file: PathBuf,
    results: PathBuf,
    project_id: String,
    project_root: PathBuf,
    /// ck-basal has no kill switch, so the crash case is not run.
    no_kill_hook: bool,
    models: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut values = std::collections::HashMap::new();
    let mut no_kill_hook = false;
    let mut models = false;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        if flag == "--no-kill-hook" {
            no_kill_hook = true;
            continue;
        }
        if flag == "--models" {
            models = true;
            continue;
        }
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        values.insert(flag, PathBuf::from(value));
    }
    let mut take = |flag: &str| {
        values
            .remove(flag)
            .ok_or_else(|| format!("{flag} is required"))
    };
    let args = Args {
        connection_file: std::env::var_os("SUBC_CONNECTION_FILE")
            .map(PathBuf::from)
            .ok_or("SUBC_CONNECTION_FILE is not set; run this through flows-rig.sh test")?,
        core_store: take("--core-store")?,
        basal_store: take("--basal-store")?,
        broca_index: take("--broca-index")?,
        machine_id: take("--machine-id")?,
        kill_file: take("--kill-file")?,
        unscoped_file: take("--unscoped-file")?,
        results: take("--results")?,
        project_id: take("--project-id")?.to_string_lossy().into_owned(),
        project_root: take("--project-root")?,
        no_kill_hook,
        models,
    };
    match values.keys().next() {
        Some(unknown) => Err(format!("unknown argument {unknown}")),
        None => Ok(args),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

// ------------------------------------------------------------------ report

/// One case: its checks, in order, and the replies it saw.
struct Case {
    name: String,
    checks: Vec<Value>,
    evidence: Map<String, Value>,
    /// Why the case was not run, for a case the rig cannot exercise.
    not_run: Option<String>,
    /// Why an operation cannot complete, such as a provider rejecting the package card schema.
    blocked: Option<String>,
}

impl Case {
    fn new(name: &str) -> Self {
        println!("== {name}");
        Self {
            name: name.into(),
            checks: Vec::new(),
            evidence: Map::new(),
            not_run: None,
            blocked: None,
        }
    }

    fn skipped(name: &str, reason: &str) -> Self {
        println!("== {name}\n  NOT RUN: {reason}");
        Self {
            name: name.into(),
            checks: Vec::new(),
            evidence: Map::new(),
            not_run: Some(reason.into()),
            blocked: None,
        }
    }

    fn check(&mut self, what: &str, ok: bool, detail: Value) -> bool {
        println!("  {} {what}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            println!("       {detail}");
        }
        self.checks
            .push(json!({ "check": what, "passed": ok, "detail": detail }));
        ok
    }

    fn record(&mut self, key: &str, value: Value) {
        self.evidence.insert(key.into(), value);
    }

    fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c["passed"] == true)
    }

    fn to_json(&self) -> Value {
        if let Some(reason) = &self.blocked {
            return json!({"case":self.name,"passed":Value::Null,"blocked":reason,"checks":self.checks,"evidence":self.evidence});
        }
        match &self.not_run {
            // `passed` is null, not false and not true: nothing was checked.
            Some(reason) => json!({
                "case": self.name,
                "passed": Value::Null,
                "not_run": reason,
                "checks": self.checks,
                "evidence": self.evidence,
            }),
            None => json!({
                "case": self.name,
                "passed": self.passed(),
                "checks": self.checks,
                "evidence": self.evidence,
            }),
        }
    }
}

// ------------------------------------------------------------------ waiting

async fn poll<T, F, Fut>(timeout: Duration, mut attempt: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = attempt().await {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ------------------------------------------------------------------ the rig

struct Rig {
    client: Client,
    core: CoreStore,
    basal: BasalStore,
    broca: BrocaStore,
    kill_file: PathBuf,
    unscoped_file: PathBuf,
}

/// The suite's agent: its registry name and id, the residence session it
/// registered, and the bind identity (harness and session) its routes to
/// core carry, which is how core's flow relay knows the caller.
struct Agent {
    name: String,
    id: String,
    session: String,
    identity: BindIdentity,
}

/// The first string at `key` anywhere in `value`.
fn find_str(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| map.values().find_map(|v| find_str(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find_str(v, key)),
        _ => None,
    }
}

fn code(reply: &Reply) -> Option<&str> {
    reply.as_ref().err().map(|r| r.code.as_str())
}

fn install_params(flow: &Flow) -> Value {
    json!({ "script": flow.script, "manifest": flow.manifest })
}

impl Rig {
    /// One of the agent's `flow` tool calls, through core's relay.
    async fn relay(&self, agent: &Agent, action: &str, arguments: Value) -> Relayed {
        self.client.relay(&agent.identity, action, arguments).await
    }

    /// The agent's view of one flow through the relay: its `flow.list`
    /// entry, if basal lists it.
    async fn listed(&self, agent: &Agent, flow_id: &str) -> (Relayed, Option<Value>) {
        let listed = self
            .relay(agent, "list", json!({ "flow_ids": [flow_id] }))
            .await;
        let entry = listed.reply().and_then(|r| {
            r["flows"]
                .as_array()?
                .iter()
                .find(|f| f["flow_id"] == flow_id)
                .cloned()
        });
        (listed, entry)
    }

    /// Ordinary flow install cards live in core's elicitation API; package cards
    /// instead live in the consent/v1 provider. Query the ordinary card's store.
    async fn card(&self, flow: &Flow) -> Option<Value> {
        poll(Duration::from_secs(20), || async {
            let listed = self
                .client
                .operator("elicitation.list_pending", json!({}))
                .await
                .ok()?;
            listed["records"].as_array()?.iter().find_map(|record| {
                let install = &record["flowInstall"];
                (install["flow_id"] == flow.id.as_str() && install["version"] == flow.version)
                    .then(|| record.clone())
            })
        })
        .await
    }

    async fn answer(&self, card: &Value, choice: &str) -> Reply {
        let id = card_id(card);
        self.client
            .operator(
                "elicitation.answer",
                json!({ "elicitationId": id, "choiceId": choice }),
            )
            .await
    }

    /// basal's health entry for one flow, read as the operator.
    async fn health(&self, flow_id: &str) -> Option<Value> {
        let health = self
            .client
            .basal_as_operator("flow.health", json!({ "flow_ids": [flow_id] }))
            .await
            .ok()?;
        health["flows"]
            .as_array()?
            .iter()
            .find(|f| f["flow_id"] == flow_id)
            .cloned()
    }

    async fn wait_card_state(&self, flow: &Flow, want: &str) -> Option<String> {
        poll(Duration::from_secs(20), || async {
            let state = self.basal.card_state(&flow.id, flow.version).ok()??;
            (state == want).then_some(state)
        })
        .await
    }

    async fn wait_enabled(&self, flow: &Flow) -> Option<Value> {
        poll(Duration::from_secs(20), || async {
            let entry = self.health(&flow.id).await?;
            (entry["state"] == "enabled" && entry["approved_version"] == flow.version)
                .then_some(entry)
        })
        .await
    }

    /// The first run of `flow` once it has ended.
    async fn first_run(&self, flow: &Flow) -> Option<Run> {
        poll(RUN_WAIT, || async {
            let runs = self.basal.runs(&flow.id).ok()?;
            runs.into_iter().next().filter(Run::ended)
        })
        .await
    }

    async fn disable(&self, flow_id: &str) -> Reply {
        self.client
            .basal_as_operator(
                "flow.disable",
                json!({ "flow_id": flow_id, "reason": "basal rig contract suite" }),
            )
            .await
    }

    async fn fires(&self, agent: &Agent, since_ms: i64) -> Reply {
        self.client
            .call(
                CORE,
                &suite_identity(),
                "wake.fires_list",
                json!({ "agentId": agent.id, "sinceMs": since_ms }),
            )
            .await
    }
}

fn card_id(card: &Value) -> String {
    find_str(card, "elicitationId")
        .or_else(|| find_str(card, "elicitation_id"))
        .unwrap_or_default()
}

/// The value of the card fact labelled `label`.
fn fact<'a>(card: &'a Value, label: &str) -> Option<&'a str> {
    card["facts"]
        .as_array()?
        .iter()
        .find(|f| f["label"] == label)
        .and_then(|f| f["value"].as_str())
}

/// Checks that core rendered the card's capability rows from the manifest:
/// each row parses to exactly the manifest's value for its key.
fn check_rendered(case: &mut Case, card: &Value, flow: &Flow) {
    let manifest = flow.manifest_value();
    for (label, key) in [
        ("Flow", "id"),
        ("Version", "version"),
        ("Purpose", "purpose"),
        ("Sinks", "sinks"),
        ("Status targets", "status"),
        ("Claims", "claims"),
        ("Facts access", "facts"),
    ] {
        let rendered = fact(card, label).and_then(|v| serde_json::from_str::<Value>(v).ok());
        let expected = manifest.get(key).cloned().unwrap_or(Value::Null);
        case.check(
            &format!("card row '{label}' renders the manifest's {key}"),
            rendered.as_ref() == Some(&expected),
            json!({ "rendered": fact(card, label), "manifest": expected }),
        );
    }
    // basal leaves `placement` out of the request when the manifest states
    // none, and core then renders the row as "not stated".
    let placement = match manifest.get("placement") {
        Some(stated) => fact(card, "Placement")
            .and_then(|v| serde_json::from_str::<Value>(v).ok())
            .is_some_and(|v| v == *stated),
        None => fact(card, "Placement") == Some("not stated"),
    };
    case.check(
        "card row 'Placement' renders the manifest's placement, or 'not stated' when it has none",
        placement,
        json!({ "rendered": fact(card, "Placement"), "manifest": manifest.get("placement") }),
    );
    // Core fills the schedule's defaults into the trigger row.
    let mut trigger = manifest["trigger"].clone();
    if let Some(schedule) = trigger.get_mut("schedule").and_then(Value::as_object_mut) {
        schedule.entry("tz").or_insert(json!("UTC"));
        schedule.entry("missed").or_insert(json!("once"));
    }
    let label = "Trigger (zone, missed policy, event origin)";
    let rendered = fact(card, label).and_then(|v| serde_json::from_str::<Value>(v).ok());
    case.check(
        "card row 'Trigger' renders the manifest's schedule with core's defaults",
        rendered.as_ref() == Some(&trigger),
        json!({ "rendered": fact(card, label), "expected": trigger }),
    );
    case.check(
        "the card is a flow_install card for this flow version",
        card["kind"] == "flow_install"
            && card["flowInstall"]["flow_id"] == flow.id.as_str()
            && card["flowInstall"]["version"] == flow.version,
        json!({ "kind": card["kind"], "flowInstall.flow_id": card["flowInstall"]["flow_id"] }),
    );
    case.check(
        "core recorded the exact manifest bytes basal sent",
        card["flowInstall"]["manifest_json"] == flow.manifest.as_str(),
        json!({ "manifest_json": card["flowInstall"]["manifest_json"] }),
    );
}

/// The choice id of the card option with `id`, if the card offers it.
fn option(card: &Value, id: &str) -> Option<String> {
    card["options"]
        .as_array()?
        .iter()
        .find(|o| o["id"] == id)
        .map(|_| id.to_owned())
}

// ------------------------------------------------------------------ the contract cases

async fn reset(rig: &Rig, case: &mut Case) {
    // Flows a previous run of the suite left enabled would keep running on
    // their schedules; disable them so this run's counts and its kill switch
    // see only this run's flows.
    let health = rig
        .client
        .basal_as_operator("flow.health", Value::Null)
        .await;
    case.check(
        "the callosum stub reaches basal's operator-only flow.health",
        health.is_ok(),
        evidence(&health),
    );
    let mut disabled = Vec::new();
    if let Ok(health) = &health {
        for flow in health["flows"].as_array().into_iter().flatten() {
            let id = flow["flow_id"].as_str().unwrap_or("");
            if id.starts_with("rig-") && flow["state"] == "enabled" {
                disabled.push(json!({ "flow_id": id, "reply": evidence(&rig.disable(id).await) }));
            }
        }
    }
    case.record("disabled_leftover_flows", Value::Array(disabled));
    let _ = std::fs::remove_file(&rig.kill_file);
    let _ = std::fs::remove_file(&rig.unscoped_file);
}

async fn register_agent(
    rig: &Rig,
    case: &mut Case,
    tag: &str,
    machine_id: &str,
    project_id: &str,
) -> Result<Agent, String> {
    let name = format!("RigAgent{tag}");
    let session = format!("ses_rig_{tag}");
    let identity = BindIdentity::new("/", "opencode", &session);
    let residence = |session: &str| {
        json!({
            "machine_id": machine_id,
            "harness": "opencode",
            "address_json": { "version": 1, "server": "local", "session": session },
        })
    };

    // The gate is live: a route may not register another session's residence.
    let other = rig
        .client
        .call(
            CORE,
            &identity,
            "agent.create",
            json!({
                "role": "assistant",
                "name": format!("RigIntruder{tag}"),
                "tag": "basal rig contract",
                "residence": residence(&format!("{session}_other")),
            }),
        )
        .await;
    case.check(
        "agent.create refuses a residence naming a session other than the route's",
        code(&other) == Some("identity_conflict"),
        evidence(&other),
    );

    // Register a head, as a real one registers itself. Core accepts a head
    // only when its projects registry resolves the head's project to a
    // workspace, which the rig's entorhinal does for `project_id`.
    let head = json!({
        "role": "head",
        "name": name,
        "tag": "basal rig contract",
        "project_id": project_id,
        "residence": residence(&session),
    });
    let created = rig.client.call(CORE, &identity, "agent.create", head).await;
    case.record("agent_create", evidence(&created));
    let id = created
        .as_ref()
        .ok()
        .and_then(|v| find_str(v, "agent_id"))
        .unwrap_or_default();
    if !case.check(
        "agent.create accepts self-registration: the route's harness and session name the residence",
        !id.is_empty(),
        evidence(&created),
    ) {
        return Err("no test agent".into());
    }
    // Core allows one head per project and refuses a second as a permanent
    // conflict, not a transient storage failure a caller would retry.
    let second_session = format!("{session}_second");
    let second = rig
        .client
        .call(
            CORE,
            &BindIdentity::new("/", "opencode", &second_session),
            "agent.create",
            json!({
                "role": "head",
                "name": format!("RigSecondHead{tag}"),
                "tag": "basal rig contract",
                "project_id": project_id,
                "residence": residence(&second_session),
            }),
        )
        .await;
    case.check(
        "agent.create refuses a second head for the project with agent_project_taken",
        code(&second) == Some("agent_project_taken"),
        evidence(&second),
    );

    // Core admits the head to its flow relay once it has registered the
    // head's scope and the daemon holds it. Until then the relay refuses
    // (`flow_caller_no_scope`) or cannot open its route to basal, so the
    // first list is retried, and every distinct refusal seen is recorded.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut refusals: Vec<Value> = Vec::new();
    let listed = loop {
        let listed = rig.client.relay(&identity, "list", json!({})).await;
        if listed.reply().is_some() || Instant::now() >= deadline {
            break listed;
        }
        if refusals.last() != Some(&listed.evidence()) {
            refusals.push(listed.evidence());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    case.record("relay_refusals_before_admission", Value::Array(refusals));
    case.record("relay_first_list", listed.evidence());
    case.check(
        "core's flow relay admits the new head and relays its list to basal, which lists no flows for it",
        listed
            .reply()
            .is_some_and(|r| r["flows"] == json!([]) && r["as_of"].is_i64()),
        listed.evidence(),
    );
    // A plain route to basal bound with the head's harness and session is
    // not stamped with the head's scope, so basal sees a local caller there,
    // whom flow.health tells it needs the attested operator. Only core's
    // relay carries the head's scope to basal.
    let plain = rig
        .client
        .call(BASAL, &identity, "flow.health", Value::Null)
        .await;
    case.check(
        "basal sees a plain route bound with the head's identity as a local caller, not as the agent",
        code(&plain) == Some("operator_attestation_required"),
        evidence(&plain),
    );
    // basal checks a manifest's agent names against core's `agent.list` at
    // install. The agent's entry is recorded so its shape can be compared
    // with the entry basal's fake core (`tests/wire/mod.rs`) answers.
    let listed = rig
        .client
        .call(CORE, &suite_identity(), "agent.list", json!({}))
        .await;
    case.record(
        "agent_list_entry",
        match &listed {
            Ok(list) => list["agents"]
                .as_array()
                .and_then(|agents| agents.iter().find(|a| a["agent_id"] == id.as_str()))
                .cloned()
                .unwrap_or(Value::Null),
            Err(_) => evidence(&listed),
        },
    );
    Ok(Agent {
        name,
        id,
        session,
        identity,
    })
}

/// Installs `flow`, as a local caller or as `agent` through core's relay,
/// reads its card, checks the rendered rows, answers it and returns the card.
async fn install_and_answer(
    rig: &Rig,
    case: &mut Case,
    flow: &Flow,
    agent: Option<&Agent>,
    choice: &str,
) -> Option<Value> {
    install_with_pause(rig, case, flow, agent, choice, false).await
}

async fn install_with_pause(
    rig: &Rig,
    case: &mut Case,
    flow: &Flow,
    agent: Option<&Agent>,
    choice: &str,
    pause_before_approval: bool,
) -> Option<Value> {
    let installed = match agent {
        Some(agent) => {
            let relayed = rig.relay(agent, "install", install_params(flow)).await;
            case.check(
                "core relays the install to basal and passes basal's reply back as outcome: reply",
                relayed.reply().is_some(),
                relayed.evidence(),
            );
            relayed.into_reply()
        }
        None => {
            rig.client
                .call(
                    BASAL,
                    &suite_identity(),
                    "flow.install",
                    install_params(flow),
                )
                .await
        }
    };
    case.record("install_reply", evidence(&installed));
    case.check(
        "basal accepts the install and holds it pending consent",
        installed.as_ref().is_ok_and(|r| r["state"] == "pending"),
        evidence(&installed),
    );
    let card = rig.card(flow).await;
    let Some(card) = card else {
        case.check("core raised a flow_install card", false, Value::Null);
        return None;
    };
    case.record("card", card.clone());
    let requester = rig.core.legacy_requester(&flow.id);
    case.check(
        "core attested the ckdev-basal requester as reserved:basal, not its filename",
        requester == Ok(Some("reserved:basal".into())),
        json!(requester),
    );
    check_rendered(case, &card, flow);
    let pending = rig.health(&flow.id).await;
    case.check(
        "while its card is open, flow.health lists the flow as unapproved",
        pending
            .as_ref()
            .is_some_and(|e| e["state"] == "unapproved" && e["approved_version"].is_null()),
        pending.unwrap_or(Value::Null),
    );
    let Some(choice_id) = option(&card, choice) else {
        case.check(
            &format!("the card offers '{choice}'"),
            false,
            card["options"].clone(),
        );
        return None;
    };
    // A global flow can normally write to its foreign recipient immediately after
    // approval. Pause it first so it cannot write before core's authorship row is
    // seeded. Approval changes version/owner but does not lift flow.disable.
    if pause_before_approval {
        let paused = rig.disable(&flow.id).await;
        if !case.check(
            "pause the pending flow before approval and its first schedule boundary",
            paused.is_ok(),
            evidence(&paused),
        ) {
            return None;
        }
    }
    let answered = rig.answer(&card, &choice_id).await;
    case.record("answer_reply", evidence(&answered));
    case.check(
        &format!("the operator's '{choice}' is accepted by core's legacy card API"),
        answered.as_ref().is_ok_and(|r| r["ok"] == true),
        evidence(&answered),
    );
    Some(card)
}

async fn local_install(rig: &Rig, flow: &Flow) -> Case {
    let mut case = Case::new("install through the card: local caller");
    if let Some(card) = install_and_answer(rig, &mut case, flow, None, "approve").await {
        case.check(
            "the card's author is {local: true}",
            card["flowInstall"]["author"] == json!({ "local": true }),
            card["flowInstall"]["author"].clone(),
        );
        case.check(
            "core renders the Author row as 'unverified local caller', never 'operator'",
            fact(&card, "Author") == Some("unverified local caller"),
            json!(fact(&card, "Author")),
        );
        let enabled = rig.wait_enabled(flow).await;
        case.check(
            "basal enables the flow at the approved version",
            enabled.is_some(),
            enabled.clone().unwrap_or(Value::Null),
        );
        case.record("health_after_approval", enabled.unwrap_or(Value::Null));
    }
    case
}

/// Checks that basal sent the card as the agent's: basal names an agent
/// author by the scope the daemon stamped on its route, and core resolves
/// that scope back to the agent and its session.
fn check_agent_author(case: &mut Case, card: &Value, agent: &Agent) {
    let author = &card["flowInstall"]["author"];
    case.check(
        "the card's author is {scope: <the relay route's scope ref>} and nothing else",
        author
            .as_object()
            .is_some_and(|a| a.len() == 1 && a["scope"].as_str().is_some_and(|s| !s.is_empty())),
        author.clone(),
    );
    let verified = format!("{} (verified)", agent.name);
    case.check(
        "core renders the Author row as '<agent name> (verified)'",
        fact(card, "Author") == Some(verified.as_str()),
        json!(fact(card, "Author")),
    );
    case.check(
        "core resolved the card's session from the scope, as the agent's own",
        card["sessionRef"] == agent.session.as_str(),
        card.get("sessionRef").cloned().unwrap_or(Value::Null),
    );
}

async fn scope_install(rig: &Rig, flow: &Flow, agent: &Agent) -> Case {
    let mut case = Case::new("install through the card: agent through core's flow relay");
    if let Some(card) = install_and_answer(rig, &mut case, flow, Some(agent), "approve").await {
        check_agent_author(&mut case, &card, agent);
        let enabled = rig.wait_enabled(flow).await;
        case.check(
            "basal enables the flow at the approved version",
            enabled.is_some(),
            enabled.clone().unwrap_or(Value::Null),
        );
        case.record("health_after_approval", enabled.unwrap_or(Value::Null));
    }
    case
}

async fn declined_install(rig: &Rig, flow: &Flow) -> Case {
    let mut case = Case::new("install through the card: declined");
    if install_and_answer(rig, &mut case, flow, None, "decline")
        .await
        .is_some()
    {
        let state = rig.wait_card_state(flow, "rejected").await;
        case.check(
            "basal records the card as rejected",
            state.is_some(),
            json!(rig.basal.card_state(&flow.id, flow.version).ok()),
        );
        let entry = rig.health(&flow.id).await;
        case.check(
            "flow.health lists the declined flow as unapproved, with no approved version",
            entry
                .as_ref()
                .is_some_and(|e| e["state"] == "unapproved" && e["approved_version"].is_null()),
            entry.clone().unwrap_or(Value::Null),
        );
        case.record("health_after_decline", entry.unwrap_or(Value::Null));
        let installs = rig.core.installs(&flow.id);
        case.check(
            "core holds no install record for the declined flow",
            installs.as_ref().is_ok_and(|v| v == &json!([])),
            json!(installs.map_err(|e| e.to_string())),
        );
    }
    case
}

/// Every fact envelope in `value`: an object carrying `status`.
fn envelopes<'a>(value: &'a Value, path: String, out: &mut Vec<(String, &'a Value)>) {
    if let Value::Object(map) = value {
        if map.contains_key("status") {
            out.push((path, value));
            return;
        }
        for (key, child) in map {
            envelopes(child, format!("{path}.{key}"), out);
        }
    }
}

fn digest_calls(calls: &[Call]) -> Vec<&Call> {
    calls
        .iter()
        .filter(|c| c.kind_code == KIND_SINK_DIGEST)
        .collect()
}

fn rows<T>(case: &mut Case, what: &str, result: Result<Vec<T>, String>) -> Vec<T> {
    match result {
        Ok(rows) => rows,
        Err(error) => {
            case.check(what, false, json!({"error": error}));
            Vec::new()
        }
    }
}

/// Checks that each fulfilled sink write in basal's journal has exactly one
/// receipt in core, and that core holds no receipt basal did not journal.
fn check_receipts(case: &mut Case, flow: &Flow, calls: &[Call], receipts: &[Receipt]) {
    let writes: Vec<&Call> = calls
        .iter()
        .filter(|c| {
            matches!(c.kind_code, KIND_SINK_DIGEST | KIND_SINK_STATUS)
                && c.settlement.as_deref() == Some("fulfilled")
        })
        .collect();
    for call in &writes {
        let matching: Vec<&Receipt> = receipts
            .iter()
            .filter(|r| {
                if call.kind_code == KIND_SINK_DIGEST {
                    r.key == json!(["sink.digest", flow.id, call.run_id, call.position])
                } else {
                    call.request.as_ref().is_some_and(|request| {
                        request["agent"].is_string()
                            && request["revision"].is_i64()
                            && r.key
                                == json!([
                                    "sink.status",
                                    flow.id,
                                    request["agent"],
                                    request["revision"]
                                ])
                            && call.value.as_ref().is_some_and(|v| {
                                r.reply["accepted_revision"] == v["accepted_revision"]
                            })
                    })
                }
            })
            .collect();
        case.check(
            &format!(
                "exactly one core receipt for the {} write at {}#{}",
                if call.kind_code == KIND_SINK_DIGEST {
                    "digest"
                } else {
                    "status"
                },
                call.run_id,
                call.position
            ),
            matching.len() == 1,
            json!(
                matching
                    .iter()
                    .map(|r| json!({"key": r.key, "reply": r.reply}))
                    .collect::<Vec<_>>()
            ),
        );
    }
    case.check(
        "core holds no receipt for this flow beyond basal's journaled writes",
        receipts.len() == writes.len(),
        json!({ "receipts": receipts.len(), "journaled_writes": writes.len() }),
    );
}

async fn sinks_and_facts(rig: &Rig, flow: &Flow, agent: &Agent, since: i64) -> Vec<Case> {
    let mut sinks = Case::new("sinks: digest and status through core");
    let mut facts = Case::new("facts: agent.facts through core");
    let Some(run) = rig.first_run(flow).await else {
        sinks.check(
            "the flow ran",
            false,
            json!(rig.basal.runs(&flow.id).ok().map(|r| r.len())),
        );
        return vec![sinks, facts];
    };
    // Stop further runs so the counts below are of a settled set.
    sinks.record("disable", evidence(&rig.disable(&flow.id).await));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let result = run.result.clone().unwrap_or(Value::Null);
    sinks.record(
        "run",
        json!({ "run_id": run.run_id, "state": run.state, "error": run.error }),
    );
    sinks.record("run_result", result.clone());
    sinks.check(
        "the run succeeded",
        run.state == "succeeded",
        json!({ "state": run.state, "error": run.error }),
    );

    // The digest write: stored with a fire id, and one wake fire per write.
    let digest = &result["digest"];
    sinks.check(
        "sink.digest is stored, with a fire id, not replayed",
        digest["disposition"] == "stored"
            && digest["fire_id"].is_string()
            && digest["replayed"] == false,
        digest.clone(),
    );
    let fires = rig.fires(agent, since).await;
    sinks.record("wake_fires_list", evidence(&fires));
    let flow_fires: Vec<Value> = fires
        .as_ref()
        .ok()
        .and_then(|f| f["fires"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f["sourceKey"] == format!("flow:{}", flow.id))
        .collect();
    let calls = rows(&mut sinks, "read basal journal", rig.basal.calls(&flow.id));
    let digests = digest_calls(&calls);
    sinks.check(
        "wake.fires_list shows one fire per digest write, the first one carrying the run's fire id",
        flow_fires.len() == digests.len()
            && flow_fires.iter().any(|f| f["fireId"] == digest["fire_id"]),
        json!({ "fires": flow_fires, "digest_writes": digests.len() }),
    );

    // The status write: published to the agent's session scope and shown by
    // status.line, or the documented no_live_session answer.
    let status = &result["status"];
    match status["disposition"].as_str() {
        Some("published") => {
            let scope = status["scope"].as_str().unwrap_or("");
            let line = rig
                .client
                .call(
                    CORE,
                    &suite_identity(),
                    "status.line",
                    json!({ "scopes": [scope] }),
                )
                .await;
            sinks.record("status_line", evidence(&line));
            sinks.check(
                "sink.status is published to the agent's session scope",
                scope == format!("session:{}", agent.session),
                status.clone(),
            );
            sinks.check(
                "status.line shows the flow's segment in that scope",
                line.as_ref().is_ok_and(|l| {
                    l["segments"].as_array().is_some_and(|s| {
                        s.iter().any(|seg| {
                            seg["scope"] == scope && seg["text"] == "basal rig contract: sinks case"
                        })
                    })
                }),
                evidence(&line),
            );
        }
        Some("no_live_session") => {
            sinks.check(
                "sink.status answers no_live_session with null revision, segment and scope",
                status["accepted_revision"].is_null()
                    && status["segment"].is_null()
                    && status["scope"].is_null(),
                status.clone(),
            );
        }
        _ => {
            sinks.check(
                "sink.status answered a known disposition",
                false,
                status.clone(),
            );
        }
    }
    let receipts = rows(
        &mut sinks,
        "read core receipts",
        rig.core.receipts(&flow.id),
    );
    sinks.record(
        "receipts",
        json!(
            receipts
                .iter()
                .map(|r| json!({"key": r.key, "reply": r.reply}))
                .collect::<Vec<_>>()
        ),
    );
    check_receipts(&mut sinks, flow, &calls, &receipts);

    // The facts read: identity and clock, every field's envelope, and the
    // refusals for what the manifest does not grant.
    let granted = &result["facts"];
    facts.record("granted_reply", granted.clone());
    facts.check(
        "the reply names the agent and carries core's clock",
        granted["agent_id"] == agent.id.as_str()
            && granted["as_of"].is_i64()
            && granted["core_boot_at"].is_i64()
            && granted["home"].is_string(),
        json!({ "agent_id": granted["agent_id"], "as_of": granted["as_of"], "home": granted["home"] }),
    );
    let mut found = Vec::new();
    for group in ["identity", "residence", "activity"] {
        envelopes(&granted[group], group.to_owned(), &mut found);
    }
    let malformed: Vec<Value> = found
        .iter()
        .filter(|(_, f)| {
            !(f["status"].is_string()
                && f.get("observed_at").is_some()
                && f["as_of"].is_i64()
                && f["privacy"].is_string()
                && f["source"]["kind"].is_string()
                && f["source"]["ref"].is_string()
                && f["source"]["survives_restart"].is_boolean())
        })
        .map(|(path, f)| json!({ "field": path, "fact": f }))
        .collect();
    facts.check(
        "every granted field carries its status, observed time, source and privacy class",
        !found.is_empty() && malformed.is_empty(),
        json!({ "fields": found.len(), "malformed": malformed }),
    );
    let statuses: std::collections::BTreeSet<String> = found
        .iter()
        .filter_map(|(_, f)| f["status"].as_str().map(str::to_owned))
        .collect();
    facts.record("statuses_seen", json!(statuses));
    let other = &result["other"];
    facts.record("ungranted_target_reply", other.clone());
    facts.check(
        "a target the manifest does not grant is refused",
        other.get("refused").is_some(),
        other.clone(),
    );
    let text = &result["text"];
    facts.record("ungranted_text_reply", text.clone());
    let mut text_found = Vec::new();
    envelopes(&text["activity"], "activity".into(), &mut text_found);
    let leaked: Vec<Value> = text_found
        .iter()
        .filter(|(_, f)| {
            f["privacy"] == "private-text" && (f["status"] != "denied" || !f["value"].is_null())
        })
        .map(|(path, f)| json!({ "field": path, "fact": f }))
        .collect();
    facts.check(
        "private text the manifest does not grant is refused or denied, never returned",
        text.get("refused").is_some()
            || (leaked.is_empty()
                && text_found
                    .iter()
                    .any(|(_, f)| f["privacy"] == "private-text")),
        json!({ "reply": text, "leaked": leaked }),
    );
    vec![sinks, facts]
}

/// The operator revoked the flow in core right after approving it. basal
/// asks core before every activation, so the flow's next scheduled run is
/// cancelled before it is activated, and basal stops listing the flow as
/// enabled.
async fn revoked(rig: &Rig, flow: &Flow) -> Case {
    let mut case = Case::new(
        "revoked: basal asks core before activating, never runs the revoked flow, and stops listing it as enabled",
    );
    let Some(run) = rig.first_run(flow).await else {
        case.check(
            "the flow's next scheduled run ended",
            false,
            json!(rig.basal.runs(&flow.id).ok().map(|r| r.len())),
        );
        return case;
    };
    case.record(
        "run",
        json!({ "run_id": run.run_id, "state": run.state, "result": run.result, "error": run.error }),
    );
    case.check(
        "the next scheduled run is cancelled because core revoked the version",
        run.state == "cancelled"
            && run.error.as_ref().is_some_and(|e| {
                e["detail"]
                    .as_str()
                    .is_some_and(|d| d.contains("core revoked"))
            }),
        json!({ "state": run.state, "error": run.error }),
    );
    let activations = rig.basal.activations(&run.run_id);
    let calls = rows(&mut case, "read basal journal", rig.basal.calls(&flow.id));
    case.check(
        "the run was never activated: no activation recorded and no call journaled",
        activations == Ok(0) && calls.is_empty(),
        json!({ "activations": activations, "calls": calls.len() }),
    );
    let receipts = rows(&mut case, "read core receipts", rig.core.receipts(&flow.id));
    case.check(
        "core recorded no receipt for the revoked flow",
        receipts.is_empty(),
        json!(receipts.iter().map(|r| r.key.clone()).collect::<Vec<_>>()),
    );
    let entry = rig.health(&flow.id).await.unwrap_or(Value::Null);
    case.check(
        "flow.health lists the flow as unapproved, with no approved version, disabled by core",
        entry["state"] == "unapproved"
            && entry["approved_version"].is_null()
            && entry["disabled"]["by"] == "core",
        entry,
    );
    case
}

/// The agent manages its own flow through core's relay: re-install, dry run,
/// disable, enable and list, each checked for what shows basal took the
/// caller for the agent. `owned` is every flow the agent installed this run,
/// `flow` included; `others` are flows installed by other callers, which the
/// agent's list must leave out.
async fn relayed_actions(
    rig: &Rig,
    flow: &Flow,
    agent: &Agent,
    owned: &[&str],
    others: &[&str],
) -> Case {
    let mut case = Case::new("the agent's flow actions through core's flow relay");
    let Some(card) = install_and_answer(rig, &mut case, flow, Some(agent), "approve").await else {
        return case;
    };
    check_agent_author(&mut case, &card, agent);
    let enabled = rig.wait_enabled(flow).await;
    if !case.check(
        "basal enables the flow at the approved version",
        enabled.is_some(),
        enabled.unwrap_or(Value::Null),
    ) {
        return case;
    }

    // Installing the same script and manifest again finds the version basal
    // already holds.
    let first_hash = case.evidence["install_reply"]["ok"]["code_hash"].clone();
    let again = rig.relay(agent, "install", install_params(flow)).await;
    case.record("reinstall", again.evidence());
    case.check(
        "a re-install through the relay returns the approved version with new: false",
        again.reply().is_some_and(|r| {
            r["flow_id"] == flow.id.as_str()
                && r["version"] == flow.version
                && r["new"] == false
                && r["state"] == "approved"
                && r["code_hash"] == first_hash
                && first_hash.is_string()
        }),
        json!({ "reinstall": again.evidence(), "first_code_hash": first_hash }),
    );

    // A capture dry run is for the operator and the owning agent; basal
    // refuses a local caller the same request.
    let trigger = json!({ "rig": "synthetic trigger", "flow": flow.id });
    let request = json!({ "flow_id": flow.id, "trigger": trigger });
    let dry = rig.relay(agent, "dry_run", request.clone()).await;
    case.record("dry_run", dry.evidence());
    case.check(
        "the agent's capture dry run with a synthetic trigger runs once, on that trigger, with its writes captured",
        dry.reply().is_some_and(|r| {
            let runs = r["summary"]["runs"].as_array();
            r["flow_id"] == flow.id.as_str()
                && r["version"] == flow.version
                && r["summary"]["mode"] == "capture"
                && r["summary"]["window"] == json!({ "kind": "synthetic" })
                && runs.is_some_and(|runs| {
                    runs.len() == 1
                        && runs[0]["trigger_id"] == "dry-run:synthetic"
                        && runs[0]["trigger"] == trigger
                        && runs[0]["calls"].as_array().is_some_and(|calls| {
                            calls.iter().any(|c| c["action"] == "captured")
                        })
                })
        }),
        dry.evidence(),
    );
    let local = rig
        .client
        .call(BASAL, &suite_identity(), "flow.dry_run", request)
        .await;
    case.check(
        "basal refuses the same dry run to a local caller",
        code(&local) == Some("operator_attestation_required"),
        evidence(&local),
    );

    // The owner disables its flow and undoes its own disable.
    let disabled = rig
        .relay(
            agent,
            "disable",
            json!({ "flow_id": flow.id, "reason": "basal rig contract: the owner disables" }),
        )
        .await;
    case.check(
        "the agent disables its flow through the relay",
        disabled
            .reply()
            .is_some_and(|r| r["state"] == "disabled" && r["changed"] == true),
        disabled.evidence(),
    );
    let (listed, entry) = rig.listed(agent, &flow.id).await;
    case.check(
        "the agent's flow.list shows the disable as the owner's",
        entry
            .as_ref()
            .is_some_and(|e| e["state"] == "disabled" && e["disabled"]["by"] == "owner"),
        listed.evidence(),
    );
    let enabled = rig
        .relay(agent, "enable", json!({ "flow_id": flow.id }))
        .await;
    case.check(
        "the agent undoes its own disable through the relay",
        enabled
            .reply()
            .is_some_and(|r| r["state"] == "enabled" && r["changed"] == true),
        enabled.evidence(),
    );
    let (listed, entry) = rig.listed(agent, &flow.id).await;
    case.check(
        "the agent's flow.list shows the flow enabled, with no disable",
        entry
            .as_ref()
            .is_some_and(|e| e["state"] == "enabled" && e["disabled"].is_null()),
        listed.evidence(),
    );

    // An owner may not undo the operator's disable: basal refuses, and core
    // passes the refusal back as an outcome, not as an error of its own.
    let operator = rig.disable(&flow.id).await;
    case.check(
        "the operator disables the agent's flow",
        operator.as_ref().is_ok_and(|r| r["changed"] == true),
        evidence(&operator),
    );
    let refused = rig
        .relay(agent, "enable", json!({ "flow_id": flow.id }))
        .await;
    case.check(
        "basal's refusal of the owner's enable arrives as outcome: refused, code not_permitted",
        matches!(&refused, Relayed::Refused(r) if r.code == "not_permitted"),
        refused.evidence(),
    );
    let (listed, entry) = rig.listed(agent, &flow.id).await;
    case.check(
        "the flow stays disabled by the operator",
        entry
            .as_ref()
            .is_some_and(|e| e["state"] == "disabled" && e["disabled"]["by"] == "operator"),
        listed.evidence(),
    );

    // The agent's list holds exactly its own flows, while the operator's
    // shows the other callers' flows are there to be left out.
    let all = rig.relay(agent, "list", json!({})).await;
    case.record("agent_list", all.evidence());
    let ids = |reply: &Value| -> Vec<String> {
        reply["flows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["flow_id"].as_str().map(str::to_owned))
            .collect()
    };
    let mut expected: Vec<String> = owned.iter().map(|id| (*id).to_owned()).collect();
    expected.sort();
    case.check(
        "the agent's flow.list returns exactly the flows it installed",
        all.reply().is_some_and(|r| ids(r) == expected),
        json!({ "listed": all.reply().map(ids), "expected": expected }),
    );
    let everything = rig.client.basal_as_operator("flow.list", json!({})).await;
    let everything = everything.as_ref().map(ids).unwrap_or_default();
    case.check(
        "the operator's flow.list holds the other callers' flows the agent's leaves out",
        others.iter().all(|id| everything.iter().any(|f| f == id)),
        json!({ "operator_listed": everything, "others": others }),
    );
    let receipts = rig.core.receipts(&flow.id);
    case.check(
        "core holds no receipt for the flow: the dry run captured its writes and the flow never ran",
        receipts.as_ref().is_ok_and(Vec::is_empty) && rig.basal.runs(&flow.id).is_ok_and(|r| r.is_empty()),
        json!({
            "receipts": receipts.map(|r| r.len()),
            "runs": rig.basal.runs(&flow.id).ok().map(|r| r.len()),
        }),
    );
    case
}

/// The relay's own refusals, which never reach basal. `flow` is one the suite
/// never installs otherwise, so basal holding no card for it shows the
/// refused install was not sent.
async fn relay_refusals(rig: &Rig, flow: &Flow, agent: &Agent) -> Case {
    let mut case = Case::new("core's flow relay refuses before reaching basal");
    let stranger = BindIdentity::new("/", "opencode", format!("{}_unregistered", agent.session));
    let unregistered = rig.client.relay(&stranger, "list", json!({})).await;
    case.check(
        "a session that is no registered agent's residence is refused flow_caller_not_registered",
        matches!(&unregistered, Relayed::NotRelayed(r) if r.code == "flow_caller_not_registered"),
        unregistered.evidence(),
    );
    let mut arguments = install_params(flow);
    arguments["author"] = json!(agent.id);
    let authored = rig.relay(agent, "install", arguments).await;
    case.check(
        "an install naming an author is refused invalid_request by core",
        matches!(&authored, Relayed::NotRelayed(r) if r.code == "invalid_request"),
        authored.evidence(),
    );
    let card = rig.basal.card_state(&flow.id, flow.version);
    case.check(
        "basal never received the refused install: it holds no card for the flow",
        card == Ok(None),
        json!(card),
    );
    case
}

fn basal_pid() -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?;
            let program = parts.next()?;
            program
                .ends_with("/bin/ckdev-basal")
                .then(|| pid.to_owned())
        })
}

async fn crash(rig: &Rig, flow: &Flow, agent: &Agent, since: i64) -> Case {
    let mut case = Case::new(CRASH_CASE);
    // Arm the kill switch before approving, so the flow's first run cannot
    // start unarmed. The switch names this flow, so no other flow's run can
    // trip it; its only remote call is the digest, at position 0 (the journal
    // counts from 0).
    let point = "HostAnswered { position: 0 }";
    let armed = json!({ "flow_id": flow.id, "boundary": point });
    if let Err(e) = std::fs::write(&rig.kill_file, armed.to_string()) {
        case.check("the kill switch is armed", false, json!(e.to_string()));
        return case;
    }
    let before = basal_pid();
    case.record("basal_pid_before", json!(before));
    if install_and_answer(rig, &mut case, flow, None, "approve")
        .await
        .is_none()
    {
        let _ = std::fs::remove_file(&rig.kill_file);
        return case;
    }
    let enabled = rig.wait_enabled(flow).await;
    case.check(
        "basal enables the crash flow",
        enabled.is_some(),
        enabled.unwrap_or(Value::Null),
    );
    let fired = poll(RUN_WAIT, || async {
        (!rig.kill_file.exists()).then_some(())
    })
    .await;
    if !case.check(
        &format!("ck-basal killed itself at {point} of the crash flow's run"),
        fired.is_some(),
        json!({ "kill_file": rig.kill_file }),
    ) {
        let _ = std::fs::remove_file(&rig.kill_file);
        return case;
    }
    let after = poll(Duration::from_secs(60), || async {
        basal_pid().filter(|pid| Some(pid) != before.as_ref())
    })
    .await;
    case.record("basal_pid_after", json!(after));
    case.check(
        "the daemon restarted ck-basal as a new process",
        after.is_some(),
        json!({ "before": before, "after": after }),
    );
    let run = rig.first_run(flow).await;
    case.record("disable", evidence(&rig.disable(&flow.id).await));
    let Some(run) = run else {
        case.check(
            "the crash flow's run ended after the restart",
            false,
            Value::Null,
        );
        return case;
    };
    case.check(
        "the crash flow's run succeeded after the restart",
        run.state == "succeeded",
        json!({ "state": run.state, "error": run.error, "result": run.result }),
    );
    let calls = rig.basal.calls(&flow.id).unwrap_or_default();
    let digest = calls
        .iter()
        .find(|c| c.run_id == run.run_id && c.kind_code == KIND_SINK_DIGEST);
    case.record(
        "journal",
        json!(calls.iter().map(|c| json!({"run_id": c.run_id, "position": c.position, "kind": c.kind_code, "dispatch": c.dispatch, "attempts": c.attempts, "settlement": c.settlement, "value": c.value})).collect::<Vec<_>>()),
    );
    let value = digest.and_then(|c| c.value.clone()).unwrap_or(Value::Null);
    case.check(
        "the digest is the run's call at position 0 and its journaled reply is the re-issue's, replayed: true",
        digest.is_some_and(|c| c.position == 0)
            && value["replayed"] == true
            && value["disposition"] == "stored",
        value.clone(),
    );
    let receipts = rig.core.receipts(&flow.id).unwrap_or_default();
    case.record(
        "receipts",
        json!(
            receipts
                .iter()
                .map(|r| json!({"key": r.key, "reply": r.reply}))
                .collect::<Vec<_>>()
        ),
    );
    case.check(
        "core holds exactly one receipt for the write, naming the journaled fire",
        receipts.len() == 1
            && receipts[0].key == json!(["sink.digest", flow.id, run.run_id, 0])
            && receipts[0].reply["fire_id"] == value["fire_id"],
        json!(
            receipts
                .iter()
                .map(|r| json!({"key": r.key, "reply": r.reply}))
                .collect::<Vec<_>>()
        ),
    );
    let fires = rig.fires(agent, since).await;
    let flow_fires: Vec<Value> = fires
        .as_ref()
        .ok()
        .and_then(|f| f["fires"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f["sourceKey"] == format!("flow:{}", flow.id))
        .collect();
    case.check(
        "wake.fires_list shows exactly one fire for the write",
        flow_fires.len() == 1 && flow_fires[0]["fireId"] == value["fire_id"],
        json!(flow_fires),
    );
    case
}

// ------------------------------------------------------------------ running the suite

async fn model_run(rig: &Rig, agent: &Agent, flow: &Flow, case: &mut Case) -> Option<Run> {
    install_and_answer(rig, case, flow, Some(agent), "approve").await?;
    let run = rig.first_run(flow).await;
    case.record("disable", evidence(&rig.disable(&flow.id).await));
    case.check(
        "the agent-owned flow's first run ended",
        run.is_some(),
        json!(flow.id),
    );
    if let Some(run) = &run {
        case.record(
            "run",
            json!({"run_id": run.run_id, "state": run.state,
            "error": run.error, "result": run.result}),
        );
    }
    run
}

fn arm_model(rig: &Rig, flow: &Flow, boundary: &str) -> Result<(), String> {
    let _ = std::fs::remove_file(rig.kill_file.with_extension("evidence"));
    std::fs::write(
        &rig.kill_file,
        json!({"flow_id": flow.id, "boundary": boundary}).to_string(),
    )
    .map_err(|e| e.to_string())
}

fn kill_evidence(rig: &Rig) -> Value {
    std::fs::read(rig.kill_file.with_extension("evidence"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

/// Compare each send's independent admission evidence with core's durable selector.
async fn check_model_scope(rig: &Rig, case: &mut Case, flow: &Flow, run: &Run, snapshot: &Value) {
    let route = &snapshot["route"];
    let ownership = rig.broca.ownership(route);
    let registered = rig.core.flow_scope(&flow.id, flow.version);
    let detail = json!({"bind":route,"ownership":ownership,"core_registration":registered,
        "flow_id":flow.id,"run_id":run.run_id,"position":0});
    let owner = ownership.unwrap_or(Value::Null);
    let scope = registered.ok().flatten().unwrap_or(Value::Null);
    let prefix = format!("{} {}#0", flow.id, run.run_id);
    case.check(
        &format!("{prefix}: Broca evidence belongs to this call's per-call session"),
        route["session"] == format!("basal:flow-{}:{}:0", flow.id, run.run_id),
        detail.clone(),
    );
    case.check(&format!("{prefix}: scope owner is reserved:prefrontal-core and ref/epoch match core's registration"),
        scope_matches(&owner, &scope), detail.clone());
    case.check(
        &format!("{prefix}: attested first principal is reserved:basal"),
        basal_first_principal(&owner),
        detail.clone(),
    );
    case.check(
        &format!("{prefix}: Broca's scope stamp carries this flow_id"),
        stamped_flow(&owner, &flow.id),
        detail,
    );
}

async fn non_carrier(rig: &Rig, flow: &Flow, run: &Run) -> Result<Case, String> {
    let mut case = Case::new(MODEL_CASES[7]);
    let registered = rig
        .core
        .flow_scope(&flow.id, flow.version)?
        .ok_or("core has no registered flow scope")?;
    let bind = BindIdentity::new("/", "basal-rig", format!("non-carrier:{}", run.run_id));
    let route = serde_json::to_value(&bind).map_err(|e| e.to_string())?;
    case.check(
        "the direct probe bind is fresh in Broca before opening",
        rig.broca.meta_count(&route)? == 0
            && rig.broca.runs(&route)?.is_empty()
            && rig.broca.wal_has_no_records(&route)?,
        route.clone(),
    );
    let selector = json!({"owner":registered["owner"],"ref":registered["ref"],"scope_epoch":registered["epoch"]});
    let opened = rig.client.open_flow_scope(bind, selector.clone()).await;
    // This proves a non-carrier is refused; it can't read the carrier list,
    // which no store exposes. That the list is exactly [reserved:basal] is
    // tested in the prefrontal repository at cba528a11, in
    // crates/prefrontal-core-module/src/scope_owner.rs:
    // flow_scope_registration_is_basal_only,
    // registered_flow_scope_wire_vector_pins_attributes_and_the_entire_carrier_list,
    // registered_global_flow_scope_wire_vector_pins_no_agent_and_the_entire_carrier_list.
    case.check(
        "only an attested carrier can open the flow's scope; a direct client is refused",
        non_carrier_refused(&evidence(&opened)),
        json!({"selector":selector,"reply":evidence(&opened)}),
    );
    let meta = rig.broca.meta_count(&route)?;
    let runs = rig.broca.runs(&route)?;
    let no_records = rig.broca.wal_has_no_records(&route)?;
    case.check("the refused non-carrier open reaches no Broca session: no meta row, run_index entry or WAL record",
        meta == 0 && runs.is_empty() && no_records,
        json!({"bind":route,"meta_rows":meta,"run_index":runs,"wal_has_no_records":no_records}));
    Ok(case)
}

async fn unscoped_send(rig: &Rig, agent: &Agent, tag: &str) -> Result<Case, String> {
    let mut case = Case::new(MODEL_CASES[8]);
    let flow = flows::model(
        &format!("rig-model-unscoped-{tag}"),
        &agent.name,
        false,
        false,
    );
    std::fs::write(&rig.unscoped_file, json!({"flow_id":flow.id}).to_string())
        .map_err(|e| e.to_string())?;
    let run = model_run(rig, agent, &flow, &mut case).await;
    case.check(
        "the named flow consumed the one-shot unscoped-send arm",
        !rig.unscoped_file.exists(),
        json!({"arming_file":rig.unscoped_file}),
    );
    let _ = std::fs::remove_file(&rig.unscoped_file);
    if let Some(run) = run {
        let saved = rig.basal.model_call(&run.run_id)?.unwrap_or(Value::Null);
        case.check(
            "the running Broca refuses basal's unscoped send with flow_scope_required",
            unscoped_send_refused(&saved["broca"]),
            saved.clone(),
        );
        let runs = rig.broca.runs(&saved["broca"]["route"])?;
        let no_records = rig.broca.wal_has_no_records(&saved["broca"]["route"])?;
        case.check("the refused unscoped send writes no RunStarted or run_index entry for its per-call session",
            runs.is_empty() && no_records,
            json!({"bind":saved["broca"]["route"],"run_index":runs,"wal_has_no_records":no_records}));
    }
    Ok(case)
}

async fn models(
    rig: &Rig,
    args: &Args,
    agent: &Agent,
    tag: &str,
    cases: &mut Vec<Case>,
) -> Result<(), String> {
    let result = models_inner(rig, args, agent, tag, cases).await;
    if let Err(error) = &result {
        report_model_abort(cases, error);
    }
    result
}

fn report_model_abort(cases: &mut Vec<Case>, error: &str) {
    for name in MODEL_CASES {
        if !cases.iter().any(|case| case.name == name) {
            cases.push(Case::skipped(
                name,
                &format!("model suite aborted: {error}"),
            ));
        }
    }
}

async fn models_inner(
    rig: &Rig,
    args: &Args,
    agent: &Agent,
    tag: &str,
    cases: &mut Vec<Case>,
) -> Result<(), String> {
    if !args.models {
        for name in MODEL_CASES {
            cases.push(Case::skipped(
                name,
                "no --models: the isolated rig credential is not enabled",
            ));
        }
        return Ok(());
    }
    let first = flows::model(&format!("rig-model-first-{tag}"), &agent.name, false, false);
    let mut minimal = Case::new(MODEL_CASES[0]);
    // On hook builds, kill before the first dispatch. The hook captures the
    // committed request before the host has any snapshot; replay must use it.
    if !args.no_kill_hook {
        arm_model(rig, &first, "CallCommitted { position: 0 }")?;
    }
    let run = model_run(rig, agent, &first, &mut minimal).await;
    let _ = std::fs::remove_file(&rig.kill_file);
    let ok = run.as_ref().is_some_and(|r| {
        r.state == "succeeded" && r.result.as_ref().is_some_and(|v| v["text"].is_string())
    });
    minimal.check("the first call returns model text, without an access or scope refusal", ok,
        json!({"result": run.as_ref().and_then(|r| r.result.clone()), "error": run.as_ref().and_then(|r| r.error.clone())}));
    // A Broca scope or access refusal still leaves a valid routing proof. Read
    // the selected-but-unsent checkpoint before stopping the remaining cases.
    let call = run
        .as_ref()
        .map(|r| rig.basal.model_call(&r.run_id))
        .transpose()?
        .flatten()
        .unwrap_or(Value::Null);
    if let Some(r) = &run {
        check_model_scope(rig, &mut minimal, &first, r, &call["broca"]).await;
    }
    cases.push(minimal);
    let mut routing = if args.no_kill_hook {
        Case::skipped(MODEL_CASES[1], NO_KILL_HOOK_REASON)
    } else {
        Case::new(MODEL_CASES[1])
    };
    if !args.no_kill_hook {
        let before_send = kill_evidence(rig);
        routing.check("fixture-pinned route.select runner, provider, model and decision id were durable before the host prepared a send",
            run.as_ref().is_some_and(|r| before_send["run_id"] == r.run_id)
                && before_send["attempts"] == 1 && before_send["dispatch"] == "sent"
                && before_send["broca_snapshots"] == 0
                && before_send["settlement"].is_null() && selected_luna(&before_send["args"]["selection"])
                && before_send["args"]["selection"] == call["journal"]["selection"], before_send);
        if let Some(r) = &run {
            check_model_scope(rig, &mut routing, &first, r, &call["broca"]).await;
        }
    }
    cases.push(routing);
    if !ok {
        let detail = json!({"result": run.as_ref().and_then(|r| r.result.clone()), "error": run.as_ref().and_then(|r| r.error.clone())});
        for name in &MODEL_CASES[2..] {
            cases.push(Case::skipped(
                name,
                "first minimal model call refused or failed; no further model sends",
            ));
        }
        return Err(format!(
            "first Luna call stopped the model suite (no fallback): {detail}"
        ));
    }
    let run = run.expect("successful run checked above");
    if call.is_null() {
        return Err("first model call has no Broca snapshot".into());
    }
    let snapshot = &call["broca"];
    cases.push(non_carrier(rig, &first, &run).await?);

    let mut result = Case::new(MODEL_CASES[2]);
    let indexed = rig.broca.runs(&snapshot["route"])?;
    result.record("broca_run_index", json!(indexed));
    result.record(
        "broca_route",
        json!({"bind": snapshot["route"], "ownership": rig.broca.ownership(&snapshot["route"])?}),
    );
    result.record("saved_call", call.clone());
    check_model_scope(rig, &mut result, &first, &run, snapshot).await;
    let text = poll(Duration::from_secs(20), || async {
        rig.broca.final_text(&snapshot["route"]).ok().flatten()
    })
    .await;
    result.check(
        "llm returns the completed run.result text, equal to Broca's independent transcript",
        snapshot["state"] == "completed"
            && run
                .result
                .as_ref()
                .is_some_and(|v| result_matches_transcript(v, text.as_deref())),
        json!({"broca_text": text, "flow_result": run.result}),
    );
    let ledger = rig.basal.ledger(&run.run_id)?;
    let usage = indexed.first().map(|r| &r["usage"]).unwrap_or(&Value::Null);
    let charge = ledger.first().cloned().unwrap_or(Value::Null);
    // Broca reports canonical fresh input. Missing capped fields retain the
    // unmeasured reservation, not a fabricated zero cache-write count.
    result.check(
        "run.status usage settled the reservation, retaining only unmeasured charges",
        indexed.len() == 1
            && indexed[0]["run_id"] == snapshot["broca_run_id"]
            && ledger.len() == 1
            && settled_usage(&charge, usage),
        json!({"ledger": ledger, "run_status_usage": usage}),
    );
    cases.push(result);

    let mut tools = Case::new(MODEL_CASES[6]);
    check_model_scope(rig, &mut tools, &first, &run, snapshot).await;
    let send = frozen_send(snapshot).ok_or("invalid frozen Broca send")?;
    tools.check(
        "the frozen Broca send has no tools and tool_choice none",
        no_tools(&send),
        send,
    );

    let classify = flows::model(
        &format!("rig-model-classify-{tag}"),
        &agent.name,
        true,
        false,
    );
    let mut label = Case::new(MODEL_CASES[3]);
    let classified = model_run(rig, agent, &classify, &mut label).await;
    label.check(
        "classify returns one of the requested labels",
        classified.as_ref().is_some_and(|r| {
            r.state == "succeeded"
                && r.result
                    .as_ref()
                    .is_some_and(|v| v == "positive" || v == "negative")
        }),
        json!(classified.as_ref().and_then(|r| r.result.clone())),
    );
    if let Some(r) = &classified {
        let saved = rig.basal.model_call(&r.run_id)?.unwrap_or(Value::Null);
        check_model_scope(rig, &mut label, &classify, r, &saved["broca"]).await;
        check_model_scope(rig, &mut tools, &classify, r, &saved["broca"]).await;
        if !saved.is_null() {
            label.record(
                "broca_send",
                json!({"bind": saved["broca"]["route"],
                "ownership": rig.broca.ownership(&saved["broca"]["route"])? ,
                "runs": rig.broca.runs(&saved["broca"]["route"])?}),
            );
        }
        let send = frozen_send(&saved["broca"]).unwrap_or(Value::Null);
        tools.check(
            "classify's frozen send also has no tools and tool_choice none",
            no_tools(&send),
            send,
        );
    }
    cases.push(label);

    let capped = flows::model(&format!("rig-model-cap-{tag}"), &agent.name, false, true);
    let mut cap = Case::new(MODEL_CASES[4]);
    if let Some(r) = model_run(rig, agent, &capped, &mut cap).await {
        let calls = rig.basal.calls(&capped.id)?;
        let route = json!({"project_root": snapshot["route"]["project_root"], "harness": snapshot["route"]["harness"],
            "session": format!("basal:flow-{}:{}:0", capped.id, r.run_id)});
        let sends = rig.broca.runs(&route)?;
        cap.check("token_cap rejection is journaled with no dispatch, reservation or Broca run",
            r.result.as_ref().is_some_and(|v| v["refused"]["data"]["code"] == "token_cap")
                && calls.len() == 1 && calls[0].attempts == 0 && calls[0].settlement.as_deref() == Some("rejected")
                && rig.basal.ledger(&r.run_id)?.is_empty() && rig.basal.model_call(&r.run_id)?.is_none() && sends.is_empty(),
            json!({"result": r.result, "broca_runs": sends, "attempts": calls.first().map(|c| c.attempts)}));
    }
    cases.push(cap);

    if args.no_kill_hook {
        cases.push(Case::skipped(MODEL_CASES[5], NO_KILL_HOOK_REASON));
    } else {
        let flow = flows::model(&format!("rig-model-crash-{tag}"), &agent.name, false, false);
        let mut crash = Case::new(MODEL_CASES[5]);
        let before = basal_pid();
        arm_model(rig, &flow, "AcceptedCommitted { position: 0 }")?;
        let recovered = model_run(rig, agent, &flow, &mut crash).await;
        let checkpoint = kill_evidence(rig);
        let _ = std::fs::remove_file(&rig.kill_file);
        let after = basal_pid();
        crash.check(
            "kill -9 landed after Broca accepted the send and before journaled result",
            checkpoint["boundary"] == "AcceptedCommitted { position: 0 }"
                && checkpoint["dispatch"] == "accepted"
                && checkpoint["settlement"].is_null()
                && before.is_some()
                && after.is_some()
                && before != after,
            json!({"checkpoint": checkpoint, "pid_before": before, "pid_after": after}),
        );
        if let Some(r) = recovered {
            let saved = rig.basal.model_call(&r.run_id)?.unwrap_or(Value::Null);
            check_model_scope(rig, &mut crash, &flow, &r, &saved["broca"]).await;
            check_model_scope(rig, &mut tools, &flow, &r, &saved["broca"]).await;
            let sends = rig.broca.runs(&saved["broca"]["route"])?;
            crash.record(
                "broca_send",
                json!({"bind": saved["broca"]["route"],
                "ownership": rig.broca.ownership(&saved["broca"]["route"])? , "runs": sends}),
            );
            let calls = rig.basal.calls(&flow.id)?;
            let text = poll(Duration::from_secs(20), || async {
                rig.broca
                    .final_text(&saved["broca"]["route"])
                    .ok()
                    .flatten()
            })
            .await;
            crash.check("restart completes from Broca's result with exactly one independent session run",
                r.state == "succeeded" && r.result.as_ref().is_some_and(|v| result_matches_transcript(v, text.as_deref()))
                    && sends.len() == 1 && sends[0]["run_id"] == saved["broca"]["broca_run_id"]
                    && sends[0]["state"] == "completed" && calls.len() == 1 && calls[0].attempts == 1,
                json!({"result": r.result, "broca_run_index": sends, "attempts": calls.first().map(|c| c.attempts)}));
            let send = frozen_send(&saved["broca"]).unwrap_or(Value::Null);
            tools.check("the recovered send carries no tools", no_tools(&send), send);
        }
        cases.push(crash);
    }
    cases.push(tools);
    if args.no_kill_hook {
        cases.push(Case::skipped(MODEL_CASES[8], NO_KILL_HOOK_REASON));
    } else {
        cases.push(unscoped_send(rig, agent, tag).await?);
    }
    Ok(())
}

async fn suite(
    args: &Args,
    cases: &mut Vec<Case>,
    summary: &mut Map<String, Value>,
) -> Result<(), String> {
    let started = now_ms();
    let tag = format!("{:08x}", started & 0xffff_ffff);
    summary.insert("tag".into(), json!(tag));
    summary.insert("project_id".into(), json!(args.project_id));
    let machine_id = std::fs::read_to_string(&args.machine_id)
        .map_err(|e| format!("read {}: {e}", args.machine_id.display()))?
        .trim()
        .to_owned();
    let rig = Rig {
        client: Client::connect(&args.connection_file, Duration::from_secs(30)).await?,
        core: CoreStore {
            path: args.core_store.clone(),
        },
        basal: BasalStore {
            path: args.basal_store.clone(),
        },
        broca: BrocaStore {
            path: args.broca_index.clone(),
        },
        kill_file: args.kill_file.clone(),
        unscoped_file: args.unscoped_file.clone(),
    };

    let mut setup = Case::new("setup: reset and register the test agent");
    reset(&rig, &mut setup).await;
    let agent = register_agent(&rig, &mut setup, &tag, &machine_id, &args.project_id).await;
    cases.push(setup);
    let agent = match agent {
        Ok(agent) => agent,
        Err(error) => {
            rig.client.close().await;
            return Err(error);
        }
    };
    summary.insert(
        "agent".into(),
        json!({ "name": agent.name, "agent_id": agent.id, "session": agent.session }),
    );

    let sinks_flow = flows::sinks(&format!("rig-sinks-{tag}"), &agent.name, "NoSuchAgent");
    let scope_flow = flows::writer(
        &format!("rig-scope-{tag}"),
        &agent.name,
        "Basal rig contract: installed by an agent through core's flow relay, then revoked in core.",
    );
    let relayed_flow = flows::quiet(
        &format!("rig-relayed-{tag}"),
        &agent.name,
        "Basal rig contract: managed by its agent through core's flow relay; never runs.",
    );
    let refused_flow = flows::quiet(
        &format!("rig-refused-{tag}"),
        &agent.name,
        "Basal rig contract: an install core refuses before it reaches basal.",
    );
    let declined_flow = flows::writer(
        &format!("rig-declined-{tag}"),
        &agent.name,
        "Basal rig contract: a card the operator declines.",
    );
    let crash_flow = flows::crash(&format!("rig-crash-{tag}"), &agent.name);

    cases.push(local_install(&rig, &sinks_flow).await);
    let mut scope_case = scope_install(&rig, &scope_flow, &agent).await;
    // Revoke the agent's flow in core at once, before its first scheduled
    // run: basal hears of it only by asking core before that run activates
    // (the `revoked` case below).
    let revoke = rig
        .client
        .operator(
            "flow.revoke",
            json!({ "flow_id": scope_flow.id, "version": 1 }),
        )
        .await;
    scope_case.record("revoke_reply", evidence(&revoke));
    scope_case.check(
        "the operator's flow.revoke revokes the install in core",
        revoke.as_ref().is_ok_and(|r| r["revoked"] == 1)
            && rig
                .core
                .installs(&scope_flow.id)
                .is_ok_and(|v| v == json!([{ "version": 1, "revoked": true }])),
        json!({ "reply": evidence(&revoke), "installs": rig.core.installs(&scope_flow.id).ok() }),
    );
    cases.push(scope_case);
    cases.push(declined_install(&rig, &declined_flow).await);

    cases.extend(sinks_and_facts(&rig, &sinks_flow, &agent, started).await);
    cases.push(revoked(&rig, &scope_flow).await);

    let mut never = Case::new("declined: the flow never runs");
    never.check(
        "basal never ran the declined flow",
        rig.basal
            .runs(&declined_flow.id)
            .is_ok_and(|r| r.is_empty()),
        json!(rig.basal.runs(&declined_flow.id).ok().map(|r| r.len())),
    );
    never.check(
        "core holds no receipt for the declined flow",
        rig.core
            .receipts(&declined_flow.id)
            .is_ok_and(|r| r.is_empty()),
        Value::Null,
    );
    cases.push(never);

    // Without the kill switch the crash flow is never installed, so it is
    // not among the other callers' flows the operator's list must show.
    let mut others = vec![sinks_flow.id.as_str(), declined_flow.id.as_str()];
    if args.no_kill_hook {
        cases.push(Case::skipped(CRASH_CASE, NO_KILL_HOOK_REASON));
    } else {
        cases.push(crash(&rig, &crash_flow, &agent, started).await);
        others.push(crash_flow.id.as_str());
    }

    cases.push(
        relayed_actions(
            &rig,
            &relayed_flow,
            &agent,
            &[scope_flow.id.as_str(), relayed_flow.id.as_str()],
            &others,
        )
        .await,
    );
    cases.push(relay_refusals(&rig, &refused_flow, &agent).await);
    summary.insert(
        "model_token_allowance".into(),
        json!(if args.models { 4112 } else { 0 }),
    );
    let model_result = models(&rig, args, &agent, &tag, cases).await;
    cases.push(ownership::cross_agent(&rig, &agent, &tag, &machine_id).await);
    cases.push(package_cases::registration(&rig, &tag).await);
    cases.extend(package_cases::lifecycle(&rig, &agent, args, &tag).await);

    let mut cleanup = Case::new("cleanup");
    let mut cleanup_ids = vec![
        sinks_flow.id.clone(),
        scope_flow.id.clone(),
        crash_flow.id.clone(),
        relayed_flow.id.clone(),
        declined_flow.id.clone(),
        format!("rig-owner-{tag}"),
        basal_rig::packages::instance_id(&format!("rig-package-{tag}"), &agent.id),
    ];
    for kind in ["first", "classify", "cap", "crash", "unscoped"] {
        cleanup_ids.push(format!("rig-model-{kind}-{tag}"));
    }
    for flow_id in cleanup_ids {
        let instance = rig.basal.instance(&flow_id);
        cleanup.check(
            &format!("read {flow_id} removal state"),
            instance.is_ok(),
            json!(&instance),
        );
        let removed = instance
            .ok()
            .flatten()
            .is_some_and(|i| i["removed"] == true);
        if rig.basal.flow_enabled(&flow_id) == Ok(Some(true)) && !removed {
            let disabled = rig.disable(&flow_id).await;
            cleanup.check(
                &format!("disable {flow_id}"),
                disabled.is_ok(),
                evidence(&disabled),
            );
        }
        let enabled = rig.basal.flow_enabled(&flow_id);
        cleanup.check(
            &format!("{flow_id} is left disabled, removed or was never installed"),
            enabled
                .as_ref()
                .is_ok_and(|enabled| *enabled != Some(true) || removed),
            json!(enabled),
        );
    }
    // The revoked flow is scheduled every minute, and the later cases took
    // minutes; the disable that came with the revoke stopped its schedule,
    // so its one cancelled run is still its only run.
    let revoked_runs = rig.basal.runs(&scope_flow.id);
    cleanup.check(
        &format!(
            "{} was admitted no run after its cancelled one",
            scope_flow.id
        ),
        revoked_runs
            .as_ref()
            .is_ok_and(|r| r.len() == 1 && r[0].state == "cancelled"),
        json!(revoked_runs.map(|r| {
            r.iter()
                .map(|r| json!({"run_id": r.run_id, "state": r.state}))
                .collect::<Vec<_>>()
        })),
    );
    cases.push(cleanup);
    rig.client.close().await;
    model_result
}

fn main() -> std::process::ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("basal-rig-contract: {e}");
            return std::process::ExitCode::from(64);
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("basal-rig-contract: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let started = now_ms();
    let mut cases = Vec::new();
    let mut summary = Map::new();
    let outcome = runtime.block_on(suite(&args, &mut cases, &mut summary));
    if let Err(e) = &outcome {
        println!("ABORTED: {e}");
    }
    let not_run: Vec<&Case> = cases.iter().filter(|c| c.not_run.is_some()).collect();
    let blocked: Vec<&Case> = cases.iter().filter(|c| c.blocked.is_some()).collect();
    let passed = outcome.is_ok()
        && cases
            .iter()
            .filter(|c| c.not_run.is_none() && c.blocked.is_none())
            .all(Case::passed);
    let checks: usize = cases.iter().map(|c| c.checks.len()).sum();
    let failed: usize = cases
        .iter()
        .flat_map(|c| &c.checks)
        .filter(|c| c["passed"] != true)
        .count();
    let report = json!({
        "suite": "basal live contract",
        "started_at_ms": started,
        "finished_at_ms": now_ms(),
        "passed": passed,
        "complete": blocked.is_empty() && not_run.is_empty(),
        "blocked": blocked.iter().map(|c| json!({"case":c.name,"reason":c.blocked})).collect::<Vec<_>>(),
        "aborted": outcome.err(),
        "checks": checks,
        "failed_checks": failed,
        "not_run": not_run
            .iter()
            .map(|c| json!({ "case": c.name, "reason": c.not_run }))
            .collect::<Vec<_>>(),
        "summary": summary,
        "cases": cases.iter().map(Case::to_json).collect::<Vec<_>>(),
    });
    let text = serde_json::to_string_pretty(&report).expect("a JSON value always serialises");
    if let Err(e) = std::fs::write(&args.results, text + "\n") {
        eprintln!(
            "basal-rig-contract: cannot write {}: {e}",
            args.results.display()
        );
        return std::process::ExitCode::FAILURE;
    }
    let skipped = match not_run.as_slice() {
        [] => String::new(),
        cases => format!(
            ", {} case{} not run: {}",
            cases.len(),
            if cases.len() == 1 { "" } else { "s" },
            cases
                .iter()
                .map(|c| format!(
                    "\"{}\" ({})",
                    c.name,
                    c.not_run.as_deref().unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    println!(
        "{} checks, {failed} failed{skipped}, {} BLOCKED item(s); results in {}",
        checks,
        blocked.len(),
        args.results.display()
    );
    if passed {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
#[path = "basal-rig-contract/tests.rs"]
mod tests;
