//! `basal-rig-contract`: basal's live contract suite against the real
//! prefrontal-core on the ckdev-flows rig (`docs/rig.md`).
//!
//! `script/flows-rig.sh test` runs it, in the rig's environment, against a
//! started rig:
//!
//! ```text
//! basal-rig-contract --core-store <file> --basal-store <file>
//!     --machine-id <file> --kill-file <file> --results <file>
//!     --mode registry|registry-disabled [--project-id <pj-…>]
//! ```
//!
//! `--mode` says how the rig configured core's projects registry, and is
//! written into the results. With the registry on, as production runs, the
//! test agent is a head of the project `--project-id` names (flows-rig.sh
//! registers it in the rig's entorhinal). With the registry-disabled fallback
//! core cannot resolve a head's project, so the agent is an assistant, which
//! passes the same self-registration gate and gets the same head scope.
//!
//! The connection file comes from `SUBC_CONNECTION_FILE`, which the rig sets.
//! Every check is printed as it runs and written, with the replies core and
//! basal gave, to the results file. The exit status is 0 only when every
//! check passed. No case makes a model call: no flow is granted `llm`.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use basal_rig::client::{Client, Reply, evidence, suite_identity};
use basal_rig::flows::{self, Flow};
use basal_rig::stores::{
    BasalStore, Call, CoreStore, KIND_SINK_DIGEST, KIND_SINK_STATUS, Receipt, Run,
};
use serde_json::{Map, Value, json};
use subc_protocol::scope::ScopeSelector;
use subc_protocol::{BindIdentity, Principal};

const CORE: &str = "prefrontal-core";
const BASAL: &str = "basal";
/// How long a flow approved now may take to finish its first run: up to a
/// minute to its cron boundary, plus the run itself.
const RUN_WAIT: Duration = Duration::from_secs(150);

struct Args {
    connection_file: PathBuf,
    core_store: PathBuf,
    basal_store: PathBuf,
    machine_id: PathBuf,
    kill_file: PathBuf,
    results: PathBuf,
    mode: String,
    project_id: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut values = std::collections::HashMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
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
        machine_id: take("--machine-id")?,
        kill_file: take("--kill-file")?,
        results: take("--results")?,
        mode: take("--mode")?.to_string_lossy().into_owned(),
        project_id: values
            .remove("--project-id")
            .map(|v| v.to_string_lossy().into_owned()),
    };
    match (args.mode.as_str(), &args.project_id) {
        ("registry", Some(_)) | ("registry-disabled", None) => {}
        ("registry", None) => return Err("--mode registry needs --project-id".into()),
        ("registry-disabled", Some(_)) => {
            return Err("--project-id is for --mode registry only".into());
        }
        (other, _) => return Err(format!("unknown mode {other}")),
    }
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
}

impl Case {
    fn new(name: &str) -> Self {
        println!("== {name}");
        Self {
            name: name.into(),
            checks: Vec::new(),
            evidence: Map::new(),
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
        json!({
            "case": self.name,
            "passed": self.passed(),
            "checks": self.checks,
            "evidence": self.evidence,
        })
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
    kill_file: PathBuf,
}

/// The suite's agent: its registry name and id, the residence session it
/// registered, and the core scope its routes are opened under.
struct Agent {
    name: String,
    id: String,
    session: String,
    identity: BindIdentity,
    scope: ScopeSelector,
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

impl Rig {
    /// Installs `flow` on `route` and returns basal's reply.
    async fn install(&self, flow: &Flow, agent: Option<&Agent>) -> Reply {
        let params = json!({ "script": flow.script, "manifest": flow.manifest });
        match agent {
            Some(agent) => {
                self.client
                    .call_scoped(BASAL, &agent.identity, &agent.scope, "flow.install", params)
                    .await
            }
            None => {
                self.client
                    .call(BASAL, &suite_identity(), "flow.install", params)
                    .await
            }
        }
    }

    /// Core's pending card for one flow version, as the operator sees it.
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
        ("Placement", "placement"),
    ] {
        let rendered = fact(card, label).and_then(|v| serde_json::from_str::<Value>(v).ok());
        let expected = manifest.get(key).cloned().unwrap_or(Value::Null);
        case.check(
            &format!("card row '{label}' renders the manifest's {key}"),
            rendered.as_ref() == Some(&expected),
            json!({ "rendered": fact(card, label), "manifest": expected }),
        );
    }
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
}

async fn register_agent(
    rig: &Rig,
    case: &mut Case,
    tag: &str,
    machine_id: &str,
    project_id: Option<&str>,
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
    // workspace. In the rig's registry-disabled fallback core cannot do that,
    // so the suite records core's refusal and registers an assistant instead:
    // core admits it through the same self-registration check and gives it a
    // head scope just like a head's.
    let head = json!({
        "role": "head",
        "name": name,
        "tag": "basal rig contract",
        "project_id": project_id.map(str::to_owned).unwrap_or_else(|| format!("pj-rig{tag}")),
        "residence": residence(&session),
    });
    let mut created = rig.client.call(CORE, &identity, "agent.create", head).await;
    case.record("agent_create", evidence(&created));
    if project_id.is_none() && created.is_err() {
        println!("  head agent.create answered: {}", evidence(&created));
        case.record("head_refused_without_registry", evidence(&created));
        created = rig
            .client
            .call(
                CORE,
                &identity,
                "agent.create",
                json!({
                    "role": "assistant",
                    "name": name,
                    "tag": "basal rig contract",
                    "residence": residence(&session),
                }),
            )
            .await;
        case.record("agent_create", evidence(&created));
    }
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

    let scope = poll(Duration::from_secs(30), || async {
        rig.core.head_scope(&id).ok().flatten()
    })
    .await;
    let Some((scope_ref, epoch)) = scope else {
        case.check(
            "core registered a head scope for the agent",
            false,
            json!({ "agent_id": id }),
        );
        return Err("no scope".into());
    };
    case.check(
        "core registered a head scope for the agent",
        true,
        json!({ "scope_ref": scope_ref, "scope_epoch": epoch }),
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
        scope: ScopeSelector {
            owner: Principal::Reserved {
                module_id: CORE.into(),
            },
            scope_ref,
            scope_epoch: Some(epoch),
        },
    })
}

/// Installs `flow`, reads its card, checks the author and the rendered rows,
/// answers it and returns the card.
async fn install_and_answer(
    rig: &Rig,
    case: &mut Case,
    flow: &Flow,
    agent: Option<&Agent>,
    choice: &str,
) -> Option<Value> {
    let installed = rig.install(flow, agent).await;
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
    check_rendered(case, &card, flow);
    let Some(choice_id) = option(&card, choice) else {
        case.check(
            &format!("the card offers '{choice}'"),
            false,
            card["options"].clone(),
        );
        return None;
    };
    let answered = rig.answer(&card, &choice_id).await;
    case.record("answer_reply", evidence(&answered));
    case.check(
        &format!("the operator's '{choice}' is accepted by core"),
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

async fn scope_install(rig: &Rig, flow: &Flow, agent: &Agent) -> Case {
    let mut case = Case::new("install through the card: agent on its core scope");
    if let Some(card) = install_and_answer(rig, &mut case, flow, Some(agent), "approve").await {
        case.check(
            "the card's author is {scope: <the route's scope_ref>} and nothing else",
            card["flowInstall"]["author"] == json!({ "scope": agent.scope.scope_ref }),
            card["flowInstall"]["author"].clone(),
        );
        let verified = format!("{} (verified)", agent.name);
        case.check(
            "core renders the Author row as '<agent name> (verified)'",
            fact(&card, "Author") == Some(verified.as_str()),
            json!(fact(&card, "Author")),
        );
        case.record(
            "core_resolved_session_ref",
            card.get("sessionRef").cloned().unwrap_or(Value::Null),
        );
        case.check(
            "core resolved the card's session from the scope, as the agent's own",
            card["sessionRef"] == agent.session.as_str(),
            card.get("sessionRef").cloned().unwrap_or(Value::Null),
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
        // basal's flow row reports `state: enabled` from the install on, with
        // no approved version; what keeps it from running is that no version
        // is approved, so that is what is checked here.
        case.check(
            "basal has no approved version of the declined flow",
            entry
                .as_ref()
                .is_none_or(|e| e["approved_version"].is_null()),
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
                    r.key[0] == "sink.status"
                        && call.value.as_ref().is_some_and(|v| {
                            v["accepted_revision"].is_null()
                                || r.reply["accepted_revision"] == v["accepted_revision"]
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
    let calls = rig.basal.calls(&flow.id).unwrap_or_default();
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
    let receipts = rig.core.receipts(&flow.id).unwrap_or_default();
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

async fn revoked(rig: &Rig, flow: &Flow) -> Case {
    let mut case =
        Case::new("revoked: core itself refuses a flow version it holds no approved install for");
    let Some(run) = rig.first_run(flow).await else {
        case.check("the flow ran", false, Value::Null);
        return case;
    };
    case.record("disable", evidence(&rig.disable(&flow.id).await));
    let result = run.result.clone().unwrap_or(Value::Null);
    case.record(
        "run",
        json!({ "run_id": run.run_id, "state": run.state, "result": result }),
    );
    for write in ["digest", "status"] {
        let reply = &result[write];
        case.check(
            &format!("core refuses the revoked flow's sink.{write} with sink_flow_not_installed"),
            reply["refused"]["data"]["code"] == "sink_flow_not_installed",
            reply.clone(),
        );
    }
    let receipts = rig.core.receipts(&flow.id).unwrap_or_default();
    case.check(
        "core recorded no receipt for the revoked flow",
        receipts.is_empty(),
        json!(receipts.iter().map(|r| r.key.clone()).collect::<Vec<_>>()),
    );
    let calls = rig.basal.calls(&flow.id).unwrap_or_default();
    case.check(
        "basal sent both writes and journaled core's refusals",
        calls
            .iter()
            .filter(|c| matches!(c.kind_code, KIND_SINK_DIGEST | KIND_SINK_STATUS))
            .all(|c| c.settlement.as_deref() == Some("rejected") && c.dispatch != "none")
            && calls.iter().any(|c| c.kind_code == KIND_SINK_DIGEST),
        json!(calls.iter().map(|c| json!({"position": c.position, "kind": c.kind_code, "dispatch": c.dispatch, "settlement": c.settlement, "value": c.value})).collect::<Vec<_>>()),
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
    let mut case = Case::new("exactly once across a kill -9 of ck-basal");
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

async fn suite(
    args: &Args,
    cases: &mut Vec<Case>,
    summary: &mut Map<String, Value>,
) -> Result<(), String> {
    let started = now_ms();
    let tag = format!("{:08x}", started & 0xffff_ffff);
    summary.insert("tag".into(), json!(tag));
    summary.insert("core_projects_registry".into(), json!(args.mode));
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
        kill_file: args.kill_file.clone(),
    };

    let mut setup = Case::new("setup: reset and register the test agent");
    reset(&rig, &mut setup).await;
    let agent = register_agent(
        &rig,
        &mut setup,
        &tag,
        &machine_id,
        args.project_id.as_deref(),
    )
    .await;
    cases.push(setup);
    let agent = agent?;
    summary.insert(
        "agent".into(),
        json!({ "name": agent.name, "agent_id": agent.id, "session": agent.session,
                "scope_ref": agent.scope.scope_ref, "scope_epoch": agent.scope.scope_epoch }),
    );

    let sinks_flow = flows::sinks(&format!("rig-sinks-{tag}"), &agent.name, "NoSuchAgent");
    let scope_flow = flows::writer(
        &format!("rig-scope-{tag}"),
        &agent.name,
        "Basal rig contract: installed by an agent on its core scope, then revoked in core.",
    );
    let declined_flow = flows::writer(
        &format!("rig-declined-{tag}"),
        &agent.name,
        "Basal rig contract: a card the operator declines.",
    );
    let crash_flow = flows::crash(&format!("rig-crash-{tag}"), &agent.name);

    cases.push(local_install(&rig, &sinks_flow).await);
    let mut scope_case = scope_install(&rig, &scope_flow, &agent).await;
    // Revoke the agent's flow in core at once: basal keeps it enabled (core
    // tells it nothing), so its first run shows what core does by itself.
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

    cases.push(crash(&rig, &crash_flow, &agent, started).await);

    let mut cleanup = Case::new("cleanup");
    for flow in [&sinks_flow, &scope_flow, &crash_flow] {
        let entry = rig.health(&flow.id).await;
        cleanup.check(
            &format!("{} is left disabled", flow.id),
            entry.as_ref().is_none_or(|e| e["state"] != "enabled"),
            entry.unwrap_or(Value::Null),
        );
    }
    cases.push(cleanup);
    rig.client.close().await;
    Ok(())
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
    println!("core projects registry: {}", args.mode);
    let started = now_ms();
    let mut cases = Vec::new();
    let mut summary = Map::new();
    let outcome = runtime.block_on(suite(&args, &mut cases, &mut summary));
    if let Err(e) = &outcome {
        println!("ABORTED: {e}");
    }
    let passed = outcome.is_ok() && cases.iter().all(Case::passed);
    let checks: usize = cases.iter().map(|c| c.checks.len()).sum();
    let failed: usize = cases
        .iter()
        .flat_map(|c| &c.checks)
        .filter(|c| c["passed"] != true)
        .count();
    let report = json!({
        "suite": "basal I1b live contract",
        "core_projects_registry": args.mode,
        "started_at_ms": started,
        "finished_at_ms": now_ms(),
        "passed": passed,
        "aborted": outcome.err(),
        "checks": checks,
        "failed_checks": failed,
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
    println!(
        "{} checks, {failed} failed; results in {}",
        checks,
        args.results.display()
    );
    if passed {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
