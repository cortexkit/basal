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
        self.host.boards.lock().unwrap()[0]["board"]["laneBlocks"][0]["items"][0] = item;
    }
}
fn item(state: &str, changed: u64) -> Value {
    json!({"id":"item1","text":"Ship","state":state,"changed_at_ms":changed})
}
fn board(agent: &str, items: Vec<Value>) -> Value {
    json!({"agent_id":agent,"board":{"lanes":["work"],"laneBlocks":[{"id":"lane1","title":"Release","status":"active","updated_at_ms":BASE,"items":items}]}})
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
    assert_eq!(value["version"], 3);
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
fn lane_done_first_at_thirty_minutes_with_timestamp_fallback() {
    let mut missing = item("done", BASE);
    missing.as_object_mut().unwrap().remove("changed_at_ms");
    let mut t = Test::new(vec![board(
        "ag_a",
        vec![item("done", BASE - HOUR), missing],
    )]);
    assert!(t.run(BASE + 30 * MIN - 1).is_empty());
    let first = t.run(BASE + 30 * MIN);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["action"], "wake");
    assert_eq!(
        first[0]["item"]["data"]["entries"][0],
        json!({"kind":"board_nudge","rule":"lane_done","agent_id":"ag_a","lane_id":"lane1","lane_title":"Release","since_ms":BASE})
    );
    assert_eq!(t.run(BASE + 3 * HOUR)[0]["action"], "silent");
}

const RULES: [&str; 4] = ["wait_ended", "eta_passed", "stale_active", "lane_done"];

fn candidate(name: &str, id: &str, since: u64) -> Value {
    let state = match name {
        "wait_ended" => "blocked",
        "stale_active" => "active",
        "lane_done" => "done",
        _ => "pending",
    };
    let mut i = item(state, since);
    i["id"] = json!(id);
    if name == "wait_ended" {
        i["changed_at_ms"] = json!(since - MIN);
        i["wait"] = json!({"on":"work","ref_state":"terminal","ref_terminal_at_ms":since});
    } else if name == "eta_passed" {
        i["eta_ms"] = json!(since);
    }
    i
}

fn candidates(name: &str, count: usize) -> Value {
    let mut b = board("ag_a", vec![]);
    if name == "lane_done" {
        b["board"]["laneBlocks"] = json!(
            (0..count)
                .map(|n| {
                    let mut lane =
                        board("ag_a", vec![candidate(name, "item1", BASE)])["board"]["laneBlocks"]
                            [0]
                        .clone();
                    lane["id"] = json!(format!("lane{n}"));
                    lane
                })
                .collect::<Vec<_>>()
        );
    } else {
        b["board"]["laneBlocks"][0]["items"] = json!(
            (0..count)
                .map(|n| candidate(name, &format!("item{n}"), BASE))
                .collect::<Vec<_>>()
        );
    }
    b
}

fn active_test(boards: Vec<Value>) -> Test {
    let t = Test::new(boards);
    *t.host.activity.lock().unwrap() = json!({"status":"ok","value":BASE + MIN});
    t
}

#[test]
fn each_rule_keeps_its_initial_age_threshold() {
    for (name, delay) in [
        ("wait_ended", 30 * MIN),
        ("eta_passed", 0),
        ("stale_active", 6 * HOUR),
    ] {
        let mut t = active_test(vec![candidates(name, 1)]);
        assert!(t.run(BASE + delay - 1).is_empty(), "{name}");
        let writes = t.run(BASE + delay);
        assert_eq!(writes.len(), 1, "{name}");
        assert_eq!(writes[0]["item"]["data"]["entries"][0]["since_ms"], BASE);
        assert_eq!(writes[0]["action"], "wake");
    }
}

#[test]
fn several_members_aggregate_by_owner_and_rule() {
    for name in RULES {
        let mut other = candidates(name, 2);
        other["agent_id"] = json!("ag_b");
        let mut t = active_test(vec![candidates(name, 3), other]);
        let writes = t.run(BASE + 8 * HOUR);
        assert_eq!(writes.len(), 2, "{name}");
        for (write, count, owner) in [(&writes[0], 3, "ag_a"), (&writes[1], 2, "ag_b")] {
            assert_eq!(write["agent"], owner);
            assert_eq!(write["action"], "wake");
            assert_eq!(write["replace_key"], format!("board_nudge:{owner}:{name}"));
            assert_eq!(write["item"]["data"]["rule"], name);
            assert_eq!(write["item"]["data"]["total_count"], count);
            assert_eq!(
                write["item"]["data"]["entries"].as_array().unwrap().len(),
                count
            );
            assert!(write["item"]["body"].as_str().unwrap().contains("Release"));
            assert!(
                write["item"]["body"]
                    .as_str()
                    .unwrap()
                    .contains("board update:")
            );
        }
    }
}

#[test]
fn unchanged_members_and_edited_fields_never_wake() {
    for name in RULES {
        let mut t = active_test(vec![candidates(name, 2)]);
        t.run(BASE + 8 * HOUR);
        assert_eq!(t.run(BASE + 16 * HOUR)[0]["action"], "silent", "{name}");
        t.host.boards.lock().unwrap()[0]["board"]["laneBlocks"][0]["items"][0]["text"] =
            json!("Edited");
        assert_eq!(t.run(BASE + 24 * HOUR)[0]["action"], "silent", "{name}");
    }
}

#[test]
fn new_member_wakes_and_is_listed_for_each_rule() {
    for name in RULES {
        let mut t = active_test(vec![candidates(name, 2)]);
        t.run(BASE + 8 * HOUR);
        *t.host.boards.lock().unwrap() = vec![candidates(name, 3)];
        let writes = t.run(BASE + 9 * HOUR);
        assert_eq!(writes.len(), 1, "{name}");
        assert_eq!(writes[0]["action"], "wake", "{name}");
        assert_eq!(
            writes[0]["item"]["data"]["entries"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }
    let mut t = Test::new(vec![board(
        "ag_a",
        vec![candidate("eta_passed", "later", BASE + HOUR)],
    )]);
    assert!(t.run(BASE).is_empty());
    assert_eq!(t.run(BASE + HOUR)[0]["action"], "wake");
}

#[test]
fn leaving_members_and_empty_lists_update_silently() {
    for name in RULES {
        let mut t = active_test(vec![candidates(name, 3)]);
        t.run(BASE + 8 * HOUR);
        *t.host.boards.lock().unwrap() = vec![candidates(name, 2)];
        let writes = t.run(BASE + 9 * HOUR);
        assert_eq!(writes.len(), 1, "{name}");
        assert_eq!(writes[0]["action"], "silent", "{name}");
        assert_eq!(writes[0]["item"]["data"]["total_count"], 2);
        t.host.boards.lock().unwrap().clear();
        let empty = t.run(BASE + 10 * HOUR);
        assert_eq!(empty.len(), 1, "{name}");
        assert_eq!(empty[0]["action"], "silent", "{name}");
        assert_eq!(empty[0]["item"]["body"], "Nothing is outstanding.");
        assert_eq!(empty[0]["item"]["data"]["entries"], json!([]));
        assert!(t.run(BASE + 11 * HOUR).is_empty());
    }
}

#[test]
fn resolved_retired_and_moved_items_do_not_create_cleanup_wakes() {
    let mut t = Test::new(vec![candidates("eta_passed", 3)]);
    t.run(BASE + HOUR);
    {
        let mut boards = t.host.boards.lock().unwrap();
        let lane = &mut boards[0]["board"]["laneBlocks"][0];
        lane["id"] = json!("moved-lane");
        lane["items"][0]["state"] = json!("done");
        lane["items"][1]["eta_ms"] = json!(BASE + 4 * HOUR);
    }
    let updated = t.run(BASE + 2 * HOUR);
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0]["action"], "silent");
    assert_eq!(updated[0]["item"]["data"]["total_count"], 1);
    assert_eq!(
        updated[0]["item"]["data"]["entries"][0]["lane_id"],
        "moved-lane"
    );
    t.host.boards.lock().unwrap()[0]["board"]["laneBlocks"][0]["status"] = json!("retired");
    let emptied = t.run(BASE + 3 * HOUR);
    assert_eq!(emptied.len(), 1);
    assert_eq!(emptied[0]["action"], "silent");
    assert_eq!(emptied[0]["item"]["data"]["total_count"], 0);
}

#[test]
fn oversized_membership_is_refused_instead_of_repeatedly_waking() {
    let items = (0..1000)
        .map(|n| candidate("eta_passed", &format!("item{n:04}{}", "x".repeat(80)), BASE))
        .collect();
    let mut t = Test::new(vec![board("ag_a", items)]);
    for now in [BASE, BASE + HOUR] {
        assert!(t.run(now).is_empty());
        assert_eq!(
            t.result["refusals"][0]["message"],
            "board hygiene membership too large"
        );
        assert!(t.keys().is_empty());
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
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0]["agent"], recipient);
        assert_eq!(writes[0]["item"]["data"]["entries"][0]["agent_id"], "ag_a");
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
fn item_wait_never_copies_peer_commitments_to_waiting_owners() {
    let target = candidate("eta_passed", "item1", BASE);
    let mut waiting = item("blocked", BASE);
    waiting["wait"] = json!({"on":"ag_b","ref":"item:ag_b/item1"});
    waiting["eta_ms"] = json!(BASE);
    let mut t = Test::new(vec![
        board("ag_a", vec![waiting]),
        board("ag_b", vec![target]),
    ]);
    let first = t.run(BASE);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["agent"], "ag_b");
    assert_eq!(first[0]["action"], "wake");
    assert_eq!(
        first[0]["item"]["data"]["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        first[0]["item"]["data"]["entries"][0]
            .get("own_item_id")
            .is_none()
    );
    let repeated = t.run(BASE + 3 * HOUR);
    assert_eq!(repeated.len(), 1);
    assert_eq!(repeated[0]["agent"], "ag_b");
    assert_eq!(repeated[0]["action"], "silent");
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
        boards[0]["board"]["laneBlocks"] = json!([]);
        boards[1]["board"]["laneBlocks"][0]["items"] = json!([]);
        boards[2]["board"]["laneBlocks"][0]["items"][0]
            .as_object_mut()
            .unwrap()
            .remove("eta_ms");
    }
    let updated = t.run(BASE + MIN);
    assert_eq!(updated.len(), 33);
    assert!(updated.iter().all(|write| write["action"] == "silent"));
    let keys = t.keys();
    for agent in ["ag_00", "ag_01", "ag_02", "ag_03"] {
        assert!(!keys.iter().any(|key| key.contains(agent)), "{keys:?}");
    }
    assert_eq!(
        keys.iter().filter(|key| key.starts_with("nudge:")).count(),
        29
    );
    t.host.boards.lock().unwrap().clear();
    let emptied = t.run(BASE + 2 * MIN);
    assert_eq!(emptied.len(), 29);
    assert!(emptied.iter().all(|write| write["action"] == "silent"));
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
fn kv_limit_refusal_does_not_stop_other_digest_updates() {
    let mut a = item("pending", BASE);
    a["eta_ms"] = json!(BASE);
    let mut t = Test::new(vec![board("ag_a", vec![a.clone()]), board("ag_b", vec![a])]);
    assert_eq!(t.run(BASE).len(), 2);
    t.f.module.rt.store().write(|tx| {
        tx.execute("DELETE FROM kv WHERE key='nudge:ag_a:eta_passed'", [])?;
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
    assert!(!t.keys().contains(&"nudge:ag_a:eta_passed".into()));
    let next = t.run(BASE + MIN);
    assert_eq!(next.len(), 2);
    assert_eq!(next[0]["agent"], "ag_a");
    assert_eq!(next[0]["action"], "wake");
    assert_eq!(next[1]["action"], "silent");
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
        first[0]["item"]["data"]["entries"][0],
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
fn peer_waits_fold_into_peers_eta_list_without_waiting_owner_copies() {
    for reference in [None, Some("peer:ag_b")] {
        let mut waiting = item("blocked", BASE);
        waiting["eta_ms"] = json!(BASE + HOUR);
        waiting["wait"] = json!({"on":"ag_b","since_ms":BASE});
        if let Some(reference) = reference {
            waiting["wait"]["ref"] = json!(reference);
        }
        let mut t = Test::new(vec![
            board("ag_a", vec![waiting]),
            board("ag_b", vec![candidate("eta_passed", "own", BASE)]),
        ]);
        assert_eq!(t.run(BASE)[0]["item"]["data"]["total_count"], 1);
        let first = t.run(BASE + HOUR);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["agent"], "ag_b");
        assert_eq!(first[0]["action"], "wake");
        assert_eq!(first[0]["replace_key"], "board_nudge:ag_b:eta_passed");
        let entries = first[0]["item"]["data"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1]["agent_id"], "ag_a");
        assert_eq!(entries[1]["lane_id"], "lane1");
        assert_eq!(entries[1]["item_id"], "item1");
        assert_eq!(entries[1]["item_text"], "Ship");
        assert_eq!(entries[1]["item_state"], "blocked");
        assert_eq!(entries[1]["item_wait"]["on"], "ag_b");
        assert!(entries[1].get("own_item_id").is_none());
        let body = first[0]["item"]["body"].as_str().unwrap();
        assert!(body.contains("ag_a is waiting on you"));
        assert!(body.contains("own board, or decline"));
        assert_eq!(t.run(BASE + 3 * HOUR)[0]["action"], "silent");
        t.host.boards.lock().unwrap()[0]["board"]["laneBlocks"][0]["items"][0]["state"] =
            json!("done");
        let leaving = rule(&t.run(BASE + 4 * HOUR), "eta_passed");
        assert_eq!(leaving.len(), 1);
        assert_eq!(leaving[0]["action"], "silent");
        assert_eq!(leaving[0]["item"]["data"]["total_count"], 1);
    }
}

#[test]
fn replacement_keys_are_stable_for_owner_rule_updates() {
    let mut t = Test::new(vec![
        candidates("eta_passed", 1),
        board("ag_c", vec![item("done", BASE - HOUR)]),
    ]);
    let first = t.run(BASE);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0]["replace_key"], "board_nudge:ag_a:eta_passed");
    assert_eq!(first[1]["replace_key"], "board_nudge:ag_c:lane_done");
    *t.host.boards.lock().unwrap() = vec![candidates("eta_passed", 2)];
    let updated = t.run(BASE + 2 * HOUR);
    assert_eq!(updated.len(), 2);
    for write in &updated {
        let old = first
            .iter()
            .find(|old| old["agent"] == write["agent"])
            .unwrap();
        assert_eq!(write["replace_key"], old["replace_key"]);
    }
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
    let repeated = t.run(BASE + 2 * HOUR);
    assert_eq!(repeated.len(), 2);
    assert!(
        repeated
            .iter()
            .all(|write| write.get("replace_key").is_none() && write["action"] == "silent")
    );
    assert_eq!(t.result["refusals"].as_array().unwrap().len(), 1);
}

#[test]
fn long_item_ids_do_not_expand_owner_replacement_keys() {
    let agent = format!("ag_{}", "a".repeat(125));
    let mut i = item("pending", BASE);
    i["id"] = json!("b".repeat(80));
    i["eta_ms"] = json!(BASE);
    let mut t = Test::new(vec![board(&agent, vec![i])]);
    let first = t.run(BASE);
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0]["replace_key"],
        format!("board_nudge:{agent}:eta_passed")
    );
    assert_eq!(first[0]["agent"], agent);
    assert_eq!(t.result["refusals"], json!([]));
    assert_eq!(t.run(BASE + 2 * HOUR)[0]["action"], "silent");
}

#[test]
fn overflowing_lists_keep_oldest_entries_and_track_hidden_membership() {
    let mut items: Vec<_> = (0..100)
        .map(|n| {
            let mut i = candidate("eta_passed", &format!("item{n:03}"), BASE + n * MIN);
            i["text"] = json!(format!("Task {n} {}", "🧭".repeat(20)));
            i
        })
        .collect();
    items.reverse();
    let mut t = Test::new(vec![board("ag_a", items)]);
    let first = t.run(BASE + 8 * HOUR);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["action"], "wake");
    let data = &first[0]["item"]["data"];
    let entries = data["entries"].as_array().unwrap();
    assert!(!entries.is_empty());
    assert!(entries.len() < 100);
    assert_eq!(entries[0]["item_id"], "item000");
    for (n, entry) in entries.iter().enumerate() {
        assert_eq!(entry["item_id"], format!("item{n:03}"));
    }
    assert_eq!(data["total_count"], 100);
    assert_eq!(data["more_count"], 100 - entries.len());
    assert!(
        first[0]["item"]["body"]
            .as_str()
            .unwrap()
            .ends_with(&format!("and {} more", 100 - entries.len()))
    );
    assert_eq!(t.run(BASE + 9 * HOUR)[0]["action"], "silent");
    t.host.boards.lock().unwrap()[0]["board"]["laneBlocks"][0]["items"]
        .as_array_mut()
        .unwrap()
        .push(candidate("eta_passed", "new-hidden", BASE + 2 * HOUR));
    let joined = t.run(BASE + 10 * HOUR);
    assert_eq!(joined[0]["action"], "wake");
    assert_eq!(joined[0]["item"]["data"]["total_count"], 101);
    t.host.boards.lock().unwrap()[0]["board"]["laneBlocks"][0]["items"]
        .as_array_mut()
        .unwrap()
        .pop();
    assert_eq!(t.run(BASE + 11 * HOUR)[0]["action"], "silent");
}

#[test]
fn real_board_lane_blocks_shape_runs_without_throwing() {
    let mut waiting = item("blocked", BASE - MIN);
    waiting["wait"] = json!({"on":"work","ref":"work:task","ref_state":"terminal",
        "since_ms":BASE,"last_idle_transition_ms":BASE-MIN,"agent_id":"ag_owner",
        "display_name":"Synthetic owner","ref_terminal_at_ms":BASE});
    let mut eta = item("pending", BASE);
    eta["id"] = json!("eta-item");
    eta["eta_ms"] = json!(BASE + MIN);
    eta["follow_up"] = json!("Synthetic follow-up");
    let mut active = item("active", BASE);
    active["id"] = json!("active-item");
    let boards = vec![
        json!({"agent_id":"ag_shape","board":{
            "lanes":["asks","status","artifacts","work"],
            "laneBlocks":[
                {"id":"lane-empty","title":"Empty","status":"active","updated_at_ms":BASE,"items":[]},
                {"id":"lane-work","title":"Work","status":"active","updated_at_ms":BASE,"items":[waiting,eta,active]}
            ]
        }}),
        json!({"agent_id":"ag_empty","board":{"lanes":["asks","work"],"laneBlocks":[]}}),
    ];
    let mut t = Test::new(boards);
    *t.host.activity.lock().unwrap() = json!({"status":"ok","value":BASE + MIN});
    let writes = t.run(BASE + 8 * HOUR);
    assert_eq!(t.result["boards_scanned"], 2);
    assert_eq!(writes.len(), 3);
    assert_eq!(rule(&writes, "wait_ended").len(), 1);
    assert_eq!(rule(&writes, "eta_passed").len(), 1);
    assert_eq!(rule(&writes, "stale_active").len(), 1);
    assert_eq!(rule(&writes, "lane_done").len(), 0);
    assert_eq!(t.result["skipped_lane_blocks"], 0);
    assert_eq!(t.result["skipped_lane_items"], 0);
}

#[test]
fn malformed_lane_blocks_and_items_are_skipped_while_valid_lanes_nudge() {
    let mut missing_id = item("pending", BASE);
    missing_id.as_object_mut().unwrap().remove("id");
    let mut first = item("pending", BASE);
    first["id"] = json!("valid-first");
    first["eta_ms"] = json!(BASE);
    let mut second = item("pending", BASE);
    second["id"] = json!("valid-second");
    second["eta_ms"] = json!(BASE);
    let boards = vec![json!({"agent_id":"ag_malformed","board":{
        "lanes":["asks","work"],
        "laneBlocks":[
            "opaque lane element",
            {"id":"missing-items","title":"Invalid","status":"active","updated_at_ms":BASE},
            {"id":"lane-one","title":"First","status":"active","updated_at_ms":BASE,"items":[missing_id,first]},
            {"id":"lane-two","title":"Second","status":"active","updated_at_ms":BASE,"items":[second]}
        ]
    }})];
    let mut t = Test::new(boards);
    let writes = t.run(BASE);
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0]["item"]["data"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["item_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["valid-first", "valid-second"]
    );
    assert_eq!(t.result["skipped_lane_blocks"], 2);
    assert_eq!(t.result["skipped_lane_items"], 1);
}

#[test]
#[ignore = "requires private board.read pages and a locally runnable worker"]
fn local_board_read_pages_complete_without_throwing() {
    let directory = std::env::var_os("BOARD_HYGIENE_LIVE_PAGES")
        .map(std::path::PathBuf::from)
        .expect(
            "set BOARD_HYGIENE_LIVE_PAGES to the directory containing the private page JSON files",
        );
    let mut boards = Vec::new();
    for name in ["board-read-page1.json", "board-read-page2.json"] {
        let page: Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join(name))
                .unwrap_or_else(|error| panic!("cannot read {name}: {error}")),
        )
        .unwrap_or_else(|error| panic!("cannot parse {name}: {error}"));
        boards.extend(page["boards"].as_array().unwrap().iter().cloned());
    }
    assert_eq!(boards.len(), 31, "the two pages should contain 31 boards");
    let mut t = Test::new(boards);
    let writes = t.run(BASE + 8 * HOUR);
    assert_eq!(t.result["boards_scanned"], 31);
    let mut by_owner_rule =
        std::collections::BTreeMap::<(String, String), (usize, usize, u64)>::new();
    for write in &writes {
        let owner = write["agent"].as_str().unwrap().to_owned();
        let rule = write["item"]["data"]["rule"].as_str().unwrap().to_owned();
        let counts = by_owner_rule.entry((owner, rule)).or_default();
        counts.0 += 1;
        counts.1 += usize::from(write["action"] == "wake");
        counts.2 += write["item"]["data"]["total_count"].as_u64().unwrap();
    }
    println!(
        "private pages completed; boards=31, digest_items={}",
        writes.len()
    );
    for name in RULES {
        let selected: Vec<_> = writes
            .iter()
            .filter(|write| write["item"]["data"]["rule"] == name)
            .collect();
        println!(
            "rule={name} digest_items={} wakes={}",
            selected.len(),
            selected
                .iter()
                .filter(|write| write["action"] == "wake")
                .count()
        );
    }
    for ((owner, rule), (items, wakes, members)) in by_owner_rule {
        println!("owner={owner} rule={rule} digest_items={items} wakes={wakes} members={members}");
        assert_eq!(items, 1);
    }
}
