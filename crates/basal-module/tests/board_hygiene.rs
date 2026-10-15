//! Offline execution of the shipped flow through basal's real worker and durable KV.
mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use basal_core::{Manifest, NoHooks, RunState};
use basal_host::mock::MockHost;
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, InstallStatus,
    MockCatalog, MockConsent, OpDecl, OpKind, TransportError,
};
use basal_module::{caller::Caller, module::Hosts};
use basal_proto::{CallKind, JsonText, Primitive};
use common::{Fixture, Options};
use serde_json::{Value, json};

const MANIFEST: &str = include_str!("../../../flows/board-hygiene/manifest.json");
const SCRIPT: &str = include_str!("../../../flows/board-hygiene/flow.js");
const MIN: u64 = 60_000;
const HOUR: u64 = 60 * MIN;
const BASE: u64 = 1_800_000_000_000;

#[derive(Default)]
struct BoardHost {
    mock: MockHost,
    boards: Mutex<Vec<Value>>,
    activity: Mutex<Value>,
    facts_calls: AtomicUsize,
    reads: Mutex<Vec<Value>>,
    writes: Mutex<Vec<Value>>,
    refuse: AtomicUsize,
    replacement_supported: AtomicBool,
}

impl BoardHost {
    fn answer(&self, request: &CallRequest) -> Option<HostOutcome> {
        let args: Value = serde_json::from_str(request.args.as_str()).unwrap();
        let value = match &request.kind {
            CallKind::Op { module, op } if module == "prefrontal-core" && op == "board.read" => {
                assert!(args.get("for").is_none());
                assert_eq!(args["limit"], 16);
                assert!(args.get("agents").is_none());
                self.reads.lock().unwrap().push(args.clone());
                let boards = self.boards.lock().unwrap();
                let start = args["cursor"].as_str().map_or(0, |cursor| {
                    boards.iter().position(|b| b["agent_id"] == cursor).unwrap() + 1
                });
                let end = (start + 16).min(boards.len());
                json!({"boards": &boards[start..end], "next_cursor": if end < boards.len() {
                    boards[end-1]["agent_id"].clone()
                } else { Value::Null }, "page_cap": 16})
            }
            CallKind::Primitive(Primitive::Facts) => {
                self.facts_calls.fetch_add(1, Ordering::Relaxed);
                assert_eq!(args["include"], json!(["activity"]));
                json!({"activity":{"last_activity_at":self.activity.lock().unwrap().clone()}})
            }
            CallKind::Primitive(Primitive::SinkDigest) => {
                let item = &args["item"];
                assert!(item["title"].as_str().unwrap().chars().count() <= 200);
                assert!(item["body"].as_str().unwrap().len() <= 4096);
                assert!(serde_json::to_vec(&item["data"]).unwrap().len() <= 8192);
                assert!(serde_json::to_vec(item).unwrap().len() <= 16384);
                assert!(
                    item.get("links")
                        .is_none_or(|links| links.as_array().unwrap().len() <= 8)
                );
                assert!(args.get("options").is_none_or(Value::is_null));
                if let Some(key) = args.get("replace_key") {
                    assert!((1..=200).contains(&key.as_str().unwrap().len()));
                }
                self.writes.lock().unwrap().push(args);
                if self.refuse.swap(0, Ordering::Relaxed) > 0 {
                    return Some(HostOutcome::rejected(text(
                        json!({"code":"sink_target_not_in_audience","message":"moved"}),
                    )));
                }
                json!({"ok":true})
            }
            _ => return None,
        };
        Some(HostOutcome::fulfilled(text(value)))
    }
}

fn text(value: Value) -> JsonText {
    JsonText::new(value.to_string()).unwrap()
}

impl Host for BoardHost {
    fn supports_core_capability(&self, capability: &str) -> bool {
        capability == basal_host::core_host::FLOW_DIGEST_REPLACE_KEY_V1
            && self.replacement_supported.load(Ordering::Relaxed)
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
    host: Arc<BoardHost>,
    runs: usize,
    result: Value,
}
impl Test {
    fn new(boards: Vec<Value>) -> Self {
        let host = Arc::new(BoardHost::default());
        host.replacement_supported.store(true, Ordering::Relaxed);
        *host.boards.lock().unwrap() = boards;
        let catalog = MockCatalog::standard();
        catalog.set_flow_capable("prefrontal-core", true);
        catalog.set_op(
            "prefrontal-core",
            "board.read",
            OpDecl {
                kind: Some(OpKind::Query),
                cause_echo: false,
                shell_capable: false,
            },
        );
        let consent = MockConsent::new();
        let f = common::fixture(
            "board-hygiene",
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
        Self {
            f,
            host,
            runs: 0,
            result: Value::Null,
        }
    }
    fn run(&mut self, now: u64) -> Vec<Value> {
        self.host.mock.set_clock(now as f64);
        self.host.writes.lock().unwrap().clear();
        self.runs += 1;
        let run = common::admit(&self.f, "board-hygiene", &format!("run-{}", self.runs));
        self.f.module.engine.run_until_idle(1000).unwrap();
        let done = self.f.module.rt.run(&run).unwrap();
        assert_eq!(done.state, RunState::Succeeded, "{done:#?}");
        self.result = serde_json::from_str(done.result.as_ref().unwrap()).unwrap();
        self.host.writes.lock().unwrap().clone()
    }
    fn keys(&self) -> Vec<String> {
        self.f
            .module
            .rt
            .store()
            .read(|c| {
                let mut stmt =
                    c.prepare("SELECT key FROM kv WHERE flow_id='board-hygiene' ORDER BY key")?;
                Ok(stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<Result<_, _>>()?)
            })
            .unwrap()
    }
    fn item(&self, item: Value) {
        self.host.boards.lock().unwrap()[0]["board"]["lanes"][0]["items"][0] = item;
    }
}
fn item(state: &str, changed: u64) -> Value {
    json!({"id":"item1","text":"Ship","state":state,"changed_at_ms":changed})
}
fn board(agent: &str, items: Vec<Value>) -> Value {
    json!({"agent_id":agent,"board":{"lanes":[{"id":"lane1","title":"Release","status":"active","updated_at_ms":BASE,"items":items}]}})
}
fn rule(writes: &[Value], name: &str) -> Vec<Value> {
    writes
        .iter()
        .filter(|w| w["item"]["data"]["rule"] == name)
        .cloned()
        .collect()
}

#[test]
fn published_manifest_has_global_minimal_grants_and_five_minute_schedule() {
    let manifest = Manifest::parse(MANIFEST).unwrap();
    let value = serde_json::to_value(manifest).unwrap();
    assert_eq!(value["id"], "board-hygiene");
    assert_eq!(value["version"], 2);
    assert_eq!(value["board_read"], json!(["$audience"]));
    assert_eq!(value["audience"], json!({"kind":"global"}));
    assert_eq!(
        value["trigger"],
        json!({"schedule":{"cron":"*/5 * * * *","tz":"UTC","missed":"skip"}})
    );
    assert_eq!(
        value["ops"],
        json!([{"module":"prefrontal-core","op":"board.read"}])
    );
    assert_eq!(value["facts"]["targets"], json!(["$audience"]));
    assert_eq!(
        value["sinks"],
        json!([{"agent":"$audience","digest_max":"wake","break_through":false}])
    );
    assert!(value["llm"].is_null());
    assert!(value["net"].is_null());
}

#[test]
fn install_card_dry_run_text_matches_its_committed_fixture() {
    let t = Test::new(Vec::new());
    let card = t.f.module.rt.cards("board-hygiene").unwrap().remove(0);
    let fields: Value = serde_json::from_str(&card.card).unwrap();
    // Every replayed run stops at its first board read, which a dry run
    // captures; none of that is a problem.
    let runs = fields["dry_run_summary"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 10, "{fields:#}");
    assert!(
        runs.iter().all(|run| run["state"] == "failed"),
        "{fields:#}"
    );
    assert_eq!(fields["dry_run_text"]["problems"], json!([]));
    common::assert_dry_run_text_fixture("board-hygiene.json", &fields["dry_run_text"]);
}

#[test]
fn wait_ended_first_at_thirty_minutes_and_repeats_after_thirty_minutes() {
    let mut i = item("blocked", BASE - MIN);
    i["wait"] =
        json!({"on":"work","ref":"work:x","ref_state":"terminal","ref_terminal_at_ms":BASE});
    let mut t = Test::new(vec![board("ag_a", vec![i])]);
    assert!(t.run(BASE + 30 * MIN - 1).is_empty());
    let first = t.run(BASE + 30 * MIN);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["action"], "wake");
    assert_eq!(first[0]["item"]["data"]["since_ms"], BASE);
    assert!(t.run(BASE + 60 * MIN - 1).is_empty());
    assert_eq!(t.run(BASE + 60 * MIN).len(), 1);
}

#[test]
fn eta_first_at_eta_and_repeats_after_two_hours() {
    let mut i = item("pending", BASE - MIN);
    i["eta_ms"] = json!(BASE);
    let mut t = Test::new(vec![board("ag_a", vec![i])]);
    assert!(t.run(BASE - 1).is_empty());
    let first = t.run(BASE);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["action"], "wake");
    assert_eq!(
        first[0]["item"]["data"],
        json!({"kind":"board_nudge","rule":"eta_passed","agent_id":"ag_a","lane_id":"lane1","lane_title":"Release","item_id":"item1","item_text":"Ship","item_state":"pending","since_ms":BASE,"eta_ms":BASE})
    );
    assert!(t.run(BASE + 2 * HOUR - 1).is_empty());
    let repeated = t.run(BASE + 2 * HOUR);
    assert_eq!(repeated.len(), 1);
    assert_eq!(repeated[0]["action"], "piggyback");
}

#[test]
fn stale_active_first_at_six_hours_and_repeats_after_six_hours() {
    let mut t = Test::new(vec![board("ag_a", vec![item("active", BASE)])]);
    *t.host.activity.lock().unwrap() = json!({"status":"ok","value":BASE + MIN});
    assert!(t.run(BASE + 6 * HOUR - 1).is_empty());
    assert_eq!(t.host.facts_calls.load(Ordering::Relaxed), 0);
    let first = t.run(BASE + 6 * HOUR);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["action"], "piggyback");
    assert_eq!(
        first[0]["item"]["data"],
        json!({"kind":"board_nudge","rule":"stale_active","agent_id":"ag_a","lane_id":"lane1","lane_title":"Release","item_id":"item1","item_text":"Ship","item_state":"active","since_ms":BASE})
    );
    assert!(t.run(BASE + 12 * HOUR - 1).is_empty());
    assert_eq!(t.run(BASE + 12 * HOUR).len(), 1);
}

#[test]
fn lane_done_first_at_thirty_minutes_and_repeats_after_two_hours_with_fallback() {
    let mut missing = item("done", BASE);
    missing.as_object_mut().unwrap().remove("changed_at_ms");
    let mut t = Test::new(vec![board(
        "ag_a",
        vec![item("done", BASE - HOUR), missing],
    )]);
    assert!(t.run(BASE + 30 * MIN - 1).is_empty());
    let first = t.run(BASE + 30 * MIN);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["action"], "piggyback");
    assert_eq!(
        first[0]["item"]["data"],
        json!({"kind":"board_nudge","rule":"lane_done","agent_id":"ag_a","lane_id":"lane1","lane_title":"Release","since_ms":BASE})
    );
    assert!(t.run(BASE + 30 * MIN + 2 * HOUR - 1).is_empty());
    assert_eq!(t.run(BASE + 30 * MIN + 2 * HOUR).len(), 1);
}

#[test]
fn changed_items_drop_memory_without_sending_on_that_run() {
    for name in ["wait_ended", "eta_passed", "stale_active", "lane_done"] {
        let mut i = item(
            match name {
                "wait_ended" => "blocked",
                "stale_active" => "active",
                "lane_done" => "done",
                _ => "pending",
            },
            BASE,
        );
        if name == "wait_ended" {
            i["wait"] = json!({"on":"work","ref_state":"terminal","ref_terminal_at_ms":BASE + MIN});
        }
        if name == "eta_passed" {
            i["eta_ms"] = json!(BASE);
        }
        let mut t = Test::new(vec![board("ag_a", vec![i.clone()])]);
        *t.host.activity.lock().unwrap() = json!({"status":"ok","value":BASE + 2 * MIN});
        assert_eq!(rule(&t.run(BASE + 6 * HOUR), name).len(), 1, "{name}");
        i["changed_at_ms"] = json!(BASE + 1);
        t.item(i);
        assert!(rule(&t.run(BASE + 12 * HOUR), name).is_empty(), "{name}");
        let key = format!(
            "nudge:ag_a:{}:{name}",
            if name == "lane_done" {
                "lane1"
            } else {
                "item1"
            }
        );
        assert!(!t.keys().contains(&key), "{name}");
    }
}

#[test]
fn wait_ended_does_not_fire_after_owner_reacted() {
    let mut i = item("blocked", BASE + MIN);
    i["wait"] = json!({"ref_state":"terminal","ref_terminal_at_ms":BASE});
    let mut t = Test::new(vec![board("ag_a", vec![i])]);
    assert!(t.run(BASE + HOUR).is_empty());
}

#[test]
fn plain_peer_wait_nudges_the_peer_and_nonpeer_wait_nudges_owner() {
    for (on, reference, recipient) in [
        ("ag_b", Value::Null, "ag_b"),
        ("agent_peer", json!("peer:agent_peer"), "agent_peer"),
        ("work", json!("work:x"), "ag_a"),
    ] {
        let mut i = item("blocked", BASE);
        i["eta_ms"] = json!(BASE + 6 * HOUR);
        i["wait"] = json!({"on":on,"since_ms":BASE});
        if !reference.is_null() {
            i["wait"]["ref"] = reference;
        }
        let mut t = Test::new(vec![board("ag_a", vec![i])]);
        let writes = t.run(BASE + 6 * HOUR);
        assert_eq!(writes.len(), if recipient == "ag_a" { 1 } else { 2 });
        assert_eq!(writes[0]["agent"], recipient);
        assert_eq!(writes[0]["item"]["data"]["agent_id"], "ag_a");
        if recipient != "ag_a" {
            assert!(
                writes[0]["item"]["body"]
                    .as_str()
                    .unwrap()
                    .contains("own board, or decline")
            );
        }
    }
}

#[test]
fn item_wait_copies_only_the_first_target_eta_nudge_to_each_waiting_owner() {
    let mut target = item("pending", BASE);
    target["eta_ms"] = json!(BASE + HOUR);
    let mut waiting = item("blocked", BASE);
    waiting["eta_ms"] = json!(BASE + HOUR);
    waiting["wait"] = json!({"on":"ag_b","ref":"item:ag_b/item1"});
    waiting["id"] = json!("waiting1");
    waiting["text"] = json!("Publish");
    let mut a = board("ag_a", vec![waiting.clone()]);
    a["board"]["lanes"][0]["id"] = json!("waiting-lane");
    let mut c = board("ag_c", vec![waiting]);
    c["board"]["lanes"][0]["id"] = json!("waiting-lane");
    let mut t = Test::new(vec![a, board("ag_b", vec![target]), c]);
    let first = t.run(BASE + HOUR);
    assert_eq!(first.len(), 3);
    assert_eq!(first[0]["agent"], "ag_b");
    assert_eq!(first[0]["action"], "wake");
    for (copy, owner) in first[1..].iter().zip(["ag_a", "ag_c"]) {
        assert_eq!(copy["agent"], owner);
        assert_eq!(copy["action"], "piggyback");
        assert_eq!(copy["item"]["data"]["agent_id"], "ag_b");
        assert_eq!(copy["item"]["data"]["copy_of"], owner);
        assert!(copy["item"]["body"].as_str().unwrap().contains(
            "Follow up with ag_b, or re-plan your item: set a new ETA or change its wait."
        ));
        assert!(!copy["item"]["body"].as_str().unwrap().contains("→ active"));
        assert_eq!(
            copy["item"]["data"],
            json!({"kind":"board_nudge","rule":"eta_passed","agent_id":"ag_b","lane_id":"lane1","lane_title":"Release","item_id":"item1","item_text":"Ship","since_ms":BASE+HOUR,"eta_ms":BASE+HOUR,"copy_of":owner,"own_lane_id":"waiting-lane","own_item_id":"waiting1","own_item_text":"Publish","item_state":"blocked","item_wait":{"on":"ag_b","ref":"item:ag_b/item1"}})
        );
    }
    let repeated = t.run(BASE + 3 * HOUR);
    assert_eq!(repeated.len(), 1);
    assert_eq!(repeated[0]["agent"], "ag_b");
    assert!(repeated[0]["item"]["data"].get("copy_of").is_none());
}

#[test]
fn unknown_activity_skips_stale_items() {
    let mut t = Test::new(vec![board("ag_a", vec![item("active", BASE)])]);
    *t.host.activity.lock().unwrap() = json!({"status":"unknown","value":BASE + MIN});
    assert!(t.run(BASE + 6 * HOUR).is_empty());
    assert_eq!(t.host.facts_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn activity_not_later_than_item_skips_stale_items_and_facts_are_once_per_owner() {
    let mut t = Test::new(vec![board(
        "ag_a",
        vec![item("active", BASE), item("active", BASE)],
    )]);
    for activity in [BASE - 1, BASE] {
        *t.host.activity.lock().unwrap() = json!({"status":"ok","value":activity});
        assert!(t.run(BASE + 6 * HOUR).is_empty());
    }
    assert_eq!(t.host.facts_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn paging_reads_all_boards_and_prunes_disappeared_agents_lanes_items_and_conditions() {
    let mut boards = Vec::new();
    for n in 0..33 {
        let mut i = item("pending", BASE);
        i["eta_ms"] = json!(BASE);
        boards.push(board(&format!("ag_{n:02}"), vec![i]));
    }
    let mut t = Test::new(boards);
    assert_eq!(t.run(BASE).len(), 33);
    assert_eq!(t.host.reads.lock().unwrap().len(), 3);
    assert_eq!(t.host.reads.lock().unwrap()[1]["cursor"], "ag_15");
    {
        let mut boards = t.host.boards.lock().unwrap();
        boards.remove(0);
        boards[0]["board"]["lanes"] = json!([]);
        boards[1]["board"]["lanes"][0]["items"] = json!([]);
        boards[2]["board"]["lanes"][0]["items"][0]
            .as_object_mut()
            .unwrap()
            .remove("eta_ms");
    }
    assert!(t.run(BASE + MIN).is_empty());
    let keys = t.keys();
    for agent in ["ag_00", "ag_01", "ag_02", "ag_03"] {
        assert!(!keys.iter().any(|key| key.contains(agent)), "{keys:?}");
    }
    assert_eq!(
        keys.iter().filter(|key| key.starts_with("nudge:")).count(),
        29
    );
    t.host.boards.lock().unwrap().clear();
    assert!(t.run(BASE + 2 * MIN).is_empty());
    assert!(t.keys().iter().all(|key| key.starts_with("index:")));
    let entries: Vec<Value> =
        t.f.module
            .rt
            .store()
            .read(|c| {
                let mut stmt = c.prepare("SELECT value FROM kv WHERE flow_id='board-hygiene'")?;
                Ok(stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
                    .collect())
            })
            .unwrap();
    assert!(entries.iter().all(|value| value == &json!([])));
}

#[test]
fn kv_limit_refusal_does_not_stop_other_rate_limited_nudges() {
    let mut a = item("pending", BASE);
    a["eta_ms"] = json!(BASE);
    let mut t = Test::new(vec![board("ag_a", vec![a.clone()]), board("ag_b", vec![a])]);
    assert_eq!(t.run(BASE).len(), 2);
    t.f.module.rt.store().write(|tx| {
        tx.execute("DELETE FROM kv WHERE key='nudge:ag_a:item1:eta_passed'", [])?;
        let mut total: usize = tx.query_row("SELECT SUM(bytes) FROM kv", [], |r| r.get(0))?;
        for n in 0..32 {
            let key = format!("padding:{n}");
            let remaining = 1024 * 1024 - 16 - total;
            if remaining <= key.len() + 2 { break; }
            let value = json!("x".repeat((remaining - key.len() - 2).min(63_000))).to_string();
            let size = key.len() + value.len(); total += size;
            tx.execute("INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES('board-hygiene',?1,?2,?3,1)", rusqlite::params![key,value,size])?;
        }
        Ok(())
    }).unwrap();
    let writes = t.run(BASE + 2 * HOUR);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["agent"], "ag_b");
    assert_eq!(t.result["refusals"][0]["code"], "kv_limit");
}

#[test]
fn refused_digest_is_logged_and_does_not_stop_later_nudges() {
    let mut i = item("pending", BASE);
    i["eta_ms"] = json!(BASE);
    let mut t = Test::new(vec![board("ag_a", vec![i.clone()]), board("ag_b", vec![i])]);
    t.host.refuse.store(1, Ordering::Relaxed);
    assert_eq!(t.run(BASE).len(), 2);
    assert_eq!(
        t.result["refusals"][0]["code"],
        "sink_target_not_in_audience"
    );
    assert!(!t.keys().contains(&"nudge:ag_a:item1:eta_passed".into()));
    let next = t.run(BASE + MIN);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0]["agent"], "ag_a");
}

#[test]
fn digest_data_is_exact_and_escaped_large_text_stays_within_core_limits() {
    let mut i = item("blocked", BASE - MIN);
    i["eta_ms"] = json!(BASE + HOUR);
    i["follow_up"] = json!("Publish release");
    i["note_ref"] = json!("mc_note_1");
    i["wait"] = json!({"on":"ag_b","ref":"item:ag_b/item2","ref_state":"terminal","ref_terminal_at_ms":BASE});
    let mut t = Test::new(vec![board("ag_a", vec![i.clone()])]);
    let first = t.run(BASE + 30 * MIN);
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0]["item"]["data"],
        json!({"kind":"board_nudge","rule":"wait_ended","agent_id":"ag_a","lane_id":"lane1","lane_title":"Release","item_id":"item1","item_text":"Ship","item_state":"blocked","item_wait":{"on":"ag_b","ref":"item:ag_b/item2"},"since_ms":BASE,"ref":"item:ag_b/item2","ref_terminal_at_ms":BASE,"eta_ms":BASE+HOUR,"follow_up":"Publish release","note_ref":"mc_note_1"})
    );
    assert!(
        first[0]["item"]["body"]
            .as_str()
            .unwrap()
            .contains("board update: item \"Ship\" → done")
    );
    i["text"] = json!("🧭".repeat(300));
    i["follow_up"] = json!("\"".repeat(4096));
    t.item(i);
    let repeated = t.run(BASE + HOUR);
    assert_eq!(repeated.len(), 1);
    assert!(
        repeated[0]["item"]["body"]
            .as_str()
            .unwrap()
            .contains("Follow-up:")
    );
}

#[test]
fn plain_peer_wait_copies_first_eta_once_with_own_item_data_and_replanning_choices() {
    for reference in [None, Some("peer:ag_b")] {
        let mut waiting = item("blocked", BASE);
        waiting["eta_ms"] = json!(BASE + HOUR);
        waiting["wait"] = json!({"on":"ag_b","since_ms":BASE});
        if let Some(reference) = reference {
            waiting["wait"]["ref"] = json!(reference);
        }
        let mut t = Test::new(vec![board("ag_a", vec![waiting])]);
        assert!(t.run(BASE + HOUR - 1).is_empty());
        let first = t.run(BASE + HOUR);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0]["agent"], "ag_b");
        assert_eq!(first[0]["action"], "wake");
        let mut expected = json!({"kind":"board_nudge","rule":"eta_passed","agent_id":"ag_a","lane_id":"lane1","lane_title":"Release","item_id":"item1","item_text":"Ship","since_ms":BASE+HOUR,"eta_ms":BASE+HOUR,"item_state":"blocked","item_wait":{"on":"ag_b"}});
        if let Some(reference) = reference {
            expected["ref"] = json!(reference);
            expected["item_wait"]["ref"] = json!(reference);
        }
        assert_eq!(first[0]["item"]["data"], expected);
        expected["copy_of"] = json!("ag_a");
        expected["own_lane_id"] = json!("lane1");
        expected["own_item_id"] = json!("item1");
        expected["own_item_text"] = json!("Ship");
        assert_eq!(first[1]["agent"], "ag_a");
        assert_eq!(first[1]["action"], "piggyback");
        assert_eq!(first[1]["item"]["data"], expected);
        assert_eq!(
            first[1]["item"]["body"],
            "ag_b's item you wait on passed its ETA 0 min ago. Follow up with ag_b, or re-plan your item: set a new ETA or change its wait."
        );
        assert!(t.run(BASE + 3 * HOUR - 1).is_empty());
        let repeated = t.run(BASE + 3 * HOUR);
        assert_eq!(repeated.len(), 1);
        assert_eq!(repeated[0]["agent"], "ag_b");
        assert_eq!(repeated[0]["action"], "piggyback");
        assert!(repeated[0]["item"]["data"].get("copy_of").is_none());
    }
}

#[test]
fn replacement_keys_are_sent_for_items_lanes_copies_and_repeats() {
    let mut target = item("pending", BASE);
    target["eta_ms"] = json!(BASE);
    let mut waiting = item("blocked", BASE);
    waiting["wait"] = json!({"on":"ag_b","ref":"item:ag_b/item1"});
    let mut t = Test::new(vec![
        board("ag_a", vec![waiting]),
        board("ag_b", vec![target]),
        board("ag_c", vec![item("done", BASE - HOUR)]),
    ]);
    let first = t.run(BASE);
    assert_eq!(first.len(), 3);
    assert_eq!(first[0]["replace_key"], "board_nudge:ag_b:item1:eta_passed");
    assert_eq!(first[1]["replace_key"], "board_nudge:ag_b:item1:eta_passed");
    assert_eq!(first[0]["agent"], "ag_b");
    assert_eq!(first[1]["agent"], "ag_a");
    assert_eq!(first[2]["replace_key"], "board_nudge:ag_c:lane1:lane_done");
    let repeated = t.run(BASE + 2 * HOUR);
    assert_eq!(repeated.len(), 2);
    assert_eq!(repeated[0]["replace_key"], first[0]["replace_key"]);
    assert_eq!(repeated[1]["replace_key"], first[2]["replace_key"]);
}

#[test]
fn unsupported_replacement_retries_without_key_and_logs_once_per_run() {
    let mut i = item("pending", BASE);
    i["eta_ms"] = json!(BASE);
    let mut t = Test::new(vec![board("ag_a", vec![i.clone()]), board("ag_b", vec![i])]);
    t.host.replacement_supported.store(false, Ordering::Relaxed);
    let first = t.run(BASE);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0]["agent"], "ag_a");
    assert_eq!(first[1]["agent"], "ag_b");
    assert!(
        first
            .iter()
            .all(|write| write.get("replace_key").is_none() && write["action"] == "wake")
    );
    assert_eq!(
        t.result["refusals"],
        json!([{"message":"board hygiene digest replacement unsupported","code":"digest_replace_key_unsupported"}])
    );
    assert!(t.run(BASE + 2 * HOUR - 1).is_empty());
    assert_eq!(t.result["refusals"], json!([]));
    let repeated = t.run(BASE + 2 * HOUR);
    assert_eq!(repeated.len(), 2);
    assert!(
        repeated
            .iter()
            .all(|write| write.get("replace_key").is_none() && write["action"] == "piggyback")
    );
    assert_eq!(t.result["refusals"].as_array().unwrap().len(), 1);
}

#[test]
fn replacement_key_over_two_hundred_bytes_is_omitted_without_refusal() {
    let agent = format!("ag_{}", "a".repeat(125));
    let mut i = item("pending", BASE);
    i["id"] = json!("b".repeat(80));
    i["eta_ms"] = json!(BASE);
    let mut t = Test::new(vec![board(&agent, vec![i])]);
    t.host.replacement_supported.store(false, Ordering::Relaxed);
    let first = t.run(BASE);
    assert_eq!(first.len(), 1);
    assert!(first[0].get("replace_key").is_none());
    assert_eq!(first[0]["agent"], agent);
    assert_eq!(t.result["refusals"], json!([]));
    assert!(t.run(BASE + 2 * HOUR - 1).is_empty());
    assert_eq!(t.run(BASE + 2 * HOUR).len(), 1);
}

#[test]
fn copied_renderer_data_with_large_follow_up_and_own_text_stays_bounded() {
    let mut target = item("pending", BASE);
    target["eta_ms"] = json!(BASE);
    target["text"] = json!("🧭".repeat(300));
    target["follow_up"] = json!("\"".repeat(4096));
    let mut waiting = item("blocked", BASE);
    waiting["text"] = json!("🧭".repeat(300));
    waiting["wait"] = json!({"on":"ag_b","ref":"item:ag_b/item1"});
    let mut t = Test::new(vec![
        board("ag_a", vec![waiting]),
        board("ag_b", vec![target]),
    ]);
    let writes = t.run(BASE);
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[1]["item"]["data"]["own_lane_id"], "lane1");
    assert_eq!(writes[1]["item"]["data"]["own_item_id"], "item1");
    assert_eq!(writes[1]["item"]["data"]["item_state"], "blocked");
    assert_eq!(
        writes[1]["item"]["data"]["item_wait"],
        json!({"on":"ag_b","ref":"item:ag_b/item1"})
    );
    assert!(writes[1]["item"]["data"]["own_item_text"].is_string());
}
