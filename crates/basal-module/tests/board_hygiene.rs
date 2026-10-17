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
    primary_turn_end: Mutex<Value>,
    facts_calls: AtomicUsize,
    reads: Mutex<Vec<Value>>,
    writes: Mutex<Vec<Value>>,
    refuse: AtomicUsize,
    replacement_supported: AtomicBool,
    /// `work.unsettled` reply pages in order; none means one empty page.
    unsettled: Mutex<Vec<Value>>,
    /// Refuse `work.unsettled` reads of this page index and later with this body.
    unsettled_refusal: Mutex<Option<(usize, Value)>>,
    unsettled_reads: Mutex<Vec<Value>>,
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
            CallKind::Op { module, op }
                if module == "prefrontal-core" && op == "work.unsettled" =>
            {
                // Basal adds the approved stamp; the script sends only a cursor.
                let object = args.as_object().unwrap();
                assert!(object["stamp"].is_object());
                assert!(object.keys().all(|key| key == "stamp" || key == "cursor"));
                self.unsettled_reads.lock().unwrap().push(args.clone());
                let pages = self.unsettled.lock().unwrap();
                let index = args.get("cursor").map_or(0, |cursor| {
                    pages
                        .iter()
                        .position(|page| page["next_cursor"] == *cursor)
                        .unwrap()
                        + 1
                });
                if let Some((from, refusal)) = self.unsettled_refusal.lock().unwrap().clone()
                    && index >= from
                {
                    return Some(HostOutcome::rejected(text(refusal)));
                }
                pages.get(index).cloned().unwrap_or(json!({"items":[]}))
            }
            CallKind::Primitive(Primitive::Facts) => {
                self.facts_calls.fetch_add(1, Ordering::Relaxed);
                // Core selects fact groups by `fields`; `include` only opts into
                // private text, which this flow's grant doesn't cover.
                assert_eq!(args["fields"], json!(["activity"]));
                assert_eq!(args["include"], json!([]));
                json!({"activity":{
                    "last_activity_at":self.activity.lock().unwrap().clone(),
                    "primary_turn_end_at":self.primary_turn_end.lock().unwrap().clone(),
                }})
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
        for op in ["board.read", "work.unsettled"] {
            catalog.set_op(
                "prefrontal-core",
                op,
                OpDecl {
                    kind: Some(OpKind::Query),
                    cause_echo: false,
                    shell_capable: false,
                },
            );
        }
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
    assert_eq!(value["version"], 12);
    assert_eq!(value["board_read"], json!(["$audience"]));
    assert_eq!(value["audience"], json!({"kind":"global"}));
    assert_eq!(
        value["trigger"],
        json!({"schedule":{"cron":"*/5 * * * *","tz":"UTC","missed":"skip"}})
    );
    assert_eq!(
        value["ops"],
        json!([
            {"module":"prefrontal-core","op":"board.read"},
            {"module":"prefrontal-core","op":"work.unsettled"}
        ])
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
    assert!(t.run(BASE + 3 * HOUR).is_empty());
}

const RULES: [&str; 6] = [
    "wait_ended",
    "eta_passed",
    "stale_active",
    "stale_wait",
    "stale_pending",
    "lane_done",
];

fn candidate(name: &str, id: &str, since: u64) -> Value {
    let state = match name {
        "wait_ended" | "stale_wait" => "blocked",
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
    } else if name == "stale_wait" {
        // Already a day old, so the shared tests can run it at the same times
        // as the other rules.
        let waited = since - 24 * HOUR;
        i["changed_at_ms"] = json!(waited);
        i["wait"] =
            json!({"on":"work","ref":"work:wi_silent","ref_state":"pending","since_ms":waited});
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
        ("stale_pending", 6 * HOUR),
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
fn unchanged_lists_are_not_rewritten_and_edits_update_silently() {
    // Rewriting an unchanged list each run would replace a wake that core
    // hasn't delivered yet with a silent update, so an unchanged list sends
    // nothing; an edit to what the list shows updates it without waking.
    for name in RULES {
        let mut t = active_test(vec![candidates(name, 2)]);
        assert_eq!(t.run(BASE + 8 * HOUR)[0]["action"], "wake", "{name}");
        assert!(t.run(BASE + 16 * HOUR).is_empty(), "{name}");
        t.host.boards.lock().unwrap()[0]["board"]["laneBlocks"][0]["title"] = json!("Edited");
        let edited = t.run(BASE + 24 * HOUR);
        assert_eq!(edited.len(), 1, "{name}");
        assert_eq!(edited[0]["action"], "silent", "{name}");
        assert!(t.run(BASE + 32 * HOUR).is_empty(), "{name}");
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
fn an_item_still_blocked_on_an_ended_wait_nudges_after_its_last_change() {
    // The owner touched the item after the wait ended, or set the wait on
    // something already finished; it is still blocked on an ended wait.
    let mut i = item("blocked", BASE + HOUR);
    i["wait"] =
        json!({"on":"work","ref":"work:wi_done","ref_state":"terminal","ref_terminal_at_ms":BASE});
    let mut t = Test::new(vec![board("ag_a", vec![i])]);
    assert!(rule(&t.run(BASE + HOUR + 30 * MIN - 1), "wait_ended").is_empty());
    let writes = rule(&t.run(BASE + HOUR + 30 * MIN), "wait_ended");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["action"], "wake");
    // The body still dates the end of the wait, not the edit.
    assert_eq!(writes[0]["item"]["data"]["entries"][0]["since_ms"], BASE);
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
    assert!(t.run(BASE + 3 * HOUR).is_empty());
}

#[test]
fn unknown_activity_skips_stale_items() {
    let mut t = Test::new(vec![board("ag_a", vec![item("active", BASE)])]);
    *t.host.activity.lock().unwrap() = json!({"status":"unknown","value":BASE + MIN});
    assert!(t.run(BASE + 6 * HOUR).is_empty());
    assert_eq!(t.host.facts_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn stored_turn_end_counts_when_in_memory_activity_is_unknown() {
    // After a core restart last_activity_at is unknown until core observes the
    // head again; the stored primary_turn_end_at must still show activity.
    let mut t = Test::new(vec![board("ag_a", vec![item("active", BASE)])]);
    *t.host.activity.lock().unwrap() = json!({"status":"unknown","value":null});
    *t.host.primary_turn_end.lock().unwrap() = json!({"status":"ok","value":BASE + MIN});
    let writes = t.run(BASE + 6 * HOUR);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["item"]["data"]["rule"], "stale_active");
}

#[test]
fn the_later_of_the_two_activity_facts_decides() {
    // Either fact alone may lag the other; the owner was active after the item
    // changed if either known value is later.
    for (primary, memory) in [(BASE - 1, BASE + MIN), (BASE + MIN, BASE - 1)] {
        let mut t = Test::new(vec![board("ag_a", vec![item("active", BASE)])]);
        *t.host.primary_turn_end.lock().unwrap() = json!({"status":"ok","value":primary});
        *t.host.activity.lock().unwrap() = json!({"status":"ok","value":memory});
        assert_eq!(
            t.run(BASE + 6 * HOUR).len(),
            1,
            "primary {primary} memory {memory}"
        );
    }
    let mut t = Test::new(vec![board("ag_a", vec![item("active", BASE)])]);
    *t.host.primary_turn_end.lock().unwrap() = json!({"status":"ok","value":BASE - 1});
    *t.host.activity.lock().unwrap() = json!({"status":"ok","value":BASE});
    assert!(t.run(BASE + 6 * HOUR).is_empty());
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
    // Only the four changed owners get a (silent) update; the other 29 lists
    // are unchanged and are not rewritten.
    let updated = t.run(BASE + MIN);
    assert_eq!(updated.len(), 4);
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
    // ag_b's list changes too, so it has an update to send despite ag_a's refusal.
    t.host.boards.lock().unwrap()[1]["board"]["laneBlocks"][0]["title"] = json!("Edited");
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
    // The refused list is retried as a wake; ag_b's delivered list is unchanged.
    let next = t.run(BASE + MIN);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0]["agent"], "ag_a");
    assert_eq!(next[0]["action"], "wake");
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
fn digest_greedily_fills_limits_in_oldest_first_order_with_exact_more_count() {
    let mut items: Vec<_> = (0..80)
        .map(|n| {
            let mut i = candidate("eta_passed", &format!("item{n:03}"), BASE);
            i["text"] = json!("Quote \" and é🦀\n".repeat(6));
            i
        })
        .collect();
    let mut single = Test::new(vec![board("ag_a", vec![items[0].clone()])]);
    let example = single.run(BASE).remove(0);
    let line = example["item"]["body"].as_str().unwrap();
    // Digests order equally old entries by member ID, regardless of traversal order.
    items.reverse();
    let mut t = Test::new(vec![board("ag_a", items)]);
    let writes = t.run(BASE);
    let digest = &writes[0]["item"];
    let entries = digest["data"]["entries"].as_array().unwrap();
    let included = entries.len();
    assert!((1..80).contains(&included));
    assert_eq!(digest["data"]["total_count"], 80);
    assert_eq!(digest["data"]["more_count"], 80 - included);
    for (n, entry) in entries.iter().enumerate() {
        assert_eq!(entry["item_id"], format!("item{n:03}"));
    }
    let mut lines = vec![line; included];
    let expected = format!("{}\n\nand {} more", lines.join("\n\n"), 80 - included);
    assert_eq!(digest["body"], expected);
    let mut next = digest.clone();
    let mut entry = example["item"]["data"]["entries"][0].clone();
    entry["item_id"] = json!(format!("item{included:03}"));
    next["data"]["entries"].as_array_mut().unwrap().push(entry);
    next["data"]["more_count"] = json!(79 - included);
    lines.push(line);
    next["body"] = json!(format!(
        "{}\n\nand {} more",
        lines.join("\n\n"),
        79 - included
    ));
    assert!(
        next["body"].as_str().unwrap().len() > 4096
            || serde_json::to_vec(&next["data"]).unwrap().len() > 8192
            || serde_json::to_vec(&next).unwrap().len() > 16384,
        "the first omitted entry must not fit"
    );
}

#[test]
fn a_planned_item_idle_six_hours_without_an_eta_nudges_its_owner() {
    let mut t = Test::new(vec![board("ag_a", vec![item("pending", BASE)])]);
    assert!(rule(&t.run(BASE + 6 * HOUR - 1), "stale_pending").is_empty());
    let writes = rule(&t.run(BASE + 6 * HOUR), "stale_pending");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["agent"], "ag_a");
    assert_eq!(writes[0]["action"], "wake");
    let body = writes[0]["item"]["body"].as_str().unwrap();
    assert!(body.contains("has sat 6 h with no ETA"), "{body}");
    assert!(body.contains("an ask for the user"), "{body}");

    // An ETA hands the item to eta_passed; a change restarts the clock.
    let mut dated = item("pending", BASE);
    dated["eta_ms"] = json!(BASE + 48 * HOUR);
    let mut t = Test::new(vec![board("ag_a", vec![dated])]);
    assert!(rule(&t.run(BASE + 10 * HOUR), "stale_pending").is_empty());
    let mut t = Test::new(vec![board("ag_a", vec![item("pending", BASE + 4 * HOUR)])]);
    assert!(rule(&t.run(BASE + 9 * HOUR), "stale_pending").is_empty());
    assert_eq!(rule(&t.run(BASE + 10 * HOUR), "stale_pending").len(), 1);
}

#[test]
fn a_wait_unchanged_for_a_day_without_an_eta_nudges_the_owner() {
    // A blocked item can wait on a worker that went silent: its wait never
    // ends and it has no ETA, so no other rule fires.
    let blocked = || {
        let mut i = item("blocked", BASE);
        i["wait"] = json!({"on":"agent","ref":"work:wi_silent","since_ms":BASE});
        i
    };
    let mut t = Test::new(vec![board("ag_a", vec![blocked()])]);
    assert!(rule(&t.run(BASE + 24 * HOUR - 1), "stale_wait").is_empty());
    let writes = rule(&t.run(BASE + 24 * HOUR), "stale_wait");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["agent"], "ag_a");
    assert_eq!(writes[0]["action"], "wake");
    let body = writes[0]["item"]["body"].as_str().unwrap();
    assert!(body.contains("has waited 24 h on work:wi_silent"), "{body}");
    assert!(!body.contains("ago"), "{body}");

    // Long waits read in days.
    let mut t = Test::new(vec![board("ag_a", vec![blocked()])]);
    let writes = rule(&t.run(BASE + 106 * HOUR), "stale_wait");
    let body = writes[0]["item"]["body"].as_str().unwrap();
    assert!(body.contains("has waited 4 days on"), "{body}");

    // An older wait on the user with no ask asks the owner to file one.
    let mut legacy = blocked();
    legacy["wait"] = json!({"on":"user","since_ms":BASE});
    let mut t = Test::new(vec![board("ag_a", vec![legacy])]);
    let writes = rule(&t.run(BASE + 30 * HOUR), "stale_wait");
    let body = writes[0]["item"]["body"].as_str().unwrap();
    assert!(body.contains("on the user with no ask"), "{body}");
    assert!(body.contains("File an ask"), "{body}");

    // An ETA, an ended wait or a declined wait each take the item off this rule.
    let mut with_eta = blocked();
    with_eta["eta_ms"] = json!(BASE + 48 * HOUR);
    let mut ended = blocked();
    ended["wait"]["ref_state"] = json!("terminal");
    ended["wait"]["ref_terminal_at_ms"] = json!(BASE + HOUR);
    let mut declined = blocked();
    declined["wait"]["declined_by"] = json!("ag_b");
    // A wait on an ask is the user's to answer; the owner can't move it.
    let mut on_user = blocked();
    on_user["wait"] = json!({"on":"user","ref":"ask:ask_0123abcd","since_ms":BASE});
    for (name, i) in [
        ("eta", with_eta),
        ("ended", ended),
        ("declined", declined),
        ("ask", on_user),
    ] {
        let mut t = Test::new(vec![board("ag_a", vec![i])]);
        assert!(
            rule(&t.run(BASE + 30 * HOUR), "stale_wait").is_empty(),
            "{name}"
        );
    }

    // A change to the item restarts the clock.
    let mut touched = blocked();
    touched["changed_at_ms"] = json!(BASE + 10 * HOUR);
    let mut t = Test::new(vec![board("ag_a", vec![touched])]);
    assert!(rule(&t.run(BASE + 30 * HOUR), "stale_wait").is_empty());
    assert_eq!(rule(&t.run(BASE + 34 * HOUR), "stale_wait").len(), 1);
}

#[test]
fn a_declined_or_ended_peer_wait_nudges_the_owner_never_the_peer() {
    // When the waited-on peer declines, core makes the wait terminal and adds
    // declined_by; the overdue item is then the owner's to act on.
    for declined in [true, false] {
        let mut waiting = item("blocked", BASE);
        waiting["eta_ms"] = json!(BASE);
        waiting["wait"] = json!({"on":"ag_b","ref":"peer:ag_b","since_ms":BASE});
        if declined {
            waiting["wait"]["declined_by"] = json!("ag_b");
        } else {
            waiting["wait"]["ref_state"] = json!("terminal");
            waiting["wait"]["ref_terminal_at_ms"] = json!(BASE);
        }
        let mut t = Test::new(vec![board("ag_a", vec![waiting]), board("ag_b", vec![])]);
        let writes = rule(&t.run(BASE + HOUR), "eta_passed");
        assert_eq!(writes.len(), 1, "declined {declined}");
        assert_eq!(writes[0]["agent"], "ag_a", "declined {declined}");
    }
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
        assert!(rule(&t.run(BASE + 3 * HOUR), "eta_passed").is_empty());
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
    {
        let mut boards = t.host.boards.lock().unwrap();
        for board in boards.iter_mut() {
            board["board"]["laneBlocks"][0]["title"] = json!("Edited");
        }
    }
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
    assert!(t.run(BASE + 2 * HOUR).is_empty());
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
    assert!(t.run(BASE + 9 * HOUR).is_empty());
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
    // Run at the time the pages were read: the test clock's BASE lies months
    // after any real board, which would make every ETA and wait look overdue.
    let now = std::env::var("BOARD_HYGIENE_LIVE_NOW_MS")
        .expect("set BOARD_HYGIENE_LIVE_NOW_MS to the Unix ms time the pages were read")
        .parse()
        .expect("BOARD_HYGIENE_LIVE_NOW_MS is a Unix ms time");
    let writes = t.run(now);
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

/// Core's captured `work.unsettled` vectors, copied unchanged from prefrontal.
const UNSETTLED_VECTORS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test-vectors/work-unsettled-v1"
);

fn vector(name: &str) -> Value {
    serde_json::from_slice(
        &std::fs::read(std::path::Path::new(UNSETTLED_VECTORS).join(name)).unwrap(),
    )
    .unwrap()
}

impl Test {
    fn unsettled(&self, pages: Vec<Value>) {
        *self.host.unsettled.lock().unwrap() = pages;
    }
    fn seen(&self) -> Option<Value> {
        self.f
            .module
            .rt
            .store()
            .read(|c| {
                let mut stmt = c.prepare(
                    "SELECT value FROM kv WHERE flow_id='board-hygiene' AND key='unsettled_seen'",
                )?;
                Ok(stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
                    .next())
            })
            .unwrap()
    }
}

#[test]
fn work_unsettled_vectors_are_pinned() {
    use sha2::{Digest, Sha256};
    let dir = std::path::Path::new(UNSETTLED_VECTORS);
    let sums = std::fs::read(dir.join("SHA256SUMS")).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&sums)),
        "c198f914f6288cfbf36f11077ee3cd9b3927a8c789c04202b21169e98cc726c2"
    );
    let entries: Vec<_> = String::from_utf8(sums)
        .unwrap()
        .lines()
        .map(|line| {
            let (hash, file) = line.split_once("  ").unwrap();
            (hash.to_owned(), file.to_owned())
        })
        .collect();
    assert_eq!(entries.len(), 11);
    // The vectors, SHA256SUMS and core's README, nothing else.
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), entries.len() + 2);
    for (hash, file) in entries {
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(std::fs::read(dir.join(&file)).unwrap())
            ),
            hash,
            "{file}"
        );
    }
}

#[test]
fn a_merged_delivery_nudges_its_owner_twenty_minutes_after_it_is_first_seen() {
    let mut t = Test::new(Vec::new());
    t.unsettled(vec![vector("reply-merged-blocked.json")]);
    // The delivery ended long before BASE; the clock starts when this flow
    // first sees it merged, because core does not say when it was merged.
    assert!(t.run(BASE).is_empty());
    assert_eq!(t.seen(), Some(json!([["wi_merged_slice", BASE]])));
    assert!(t.run(BASE + 20 * MIN - 1).is_empty());
    let writes = t.run(BASE + 20 * MIN);
    assert_eq!(writes.len(), 1);
    let write = &writes[0];
    assert_eq!(write["agent"], "ag_owner");
    assert_eq!(write["action"], "wake");
    assert_eq!(
        write["replace_key"],
        "board_nudge:ag_owner:unsettled_merged"
    );
    assert_eq!(
        write["item"]["title"],
        "1 merged delivery is waiting to be settled."
    );
    assert_eq!(write["item"]["data"]["total_count"], 1);
    assert_eq!(
        write["item"]["data"]["entries"][0],
        json!({"kind":"board_nudge","rule":"unsettled_merged","agent_id":"ag_owner",
            "item_id":"wi_merged_slice","item_title":"Merged slice","merged_ref":"main",
            "since_ms":BASE,"blocks":[{"item_id":"wi_blocked_slice","title":"Blocked slice"}],
            "campaign_id":"ct_hygiene","delivered_at_ms":9_000_000})
    );
    assert_eq!(
        write["item"]["body"],
        "\"Merged slice\" (wi_merged_slice) has been merged into main and left unsettled for at least 20 min. \
         Waiting on it: \"Blocked slice\" (wi_blocked_slice). \
         Score the delivery and settle it: work(action=\"settle\", id=\"wi_merged_slice\", score=<0-100>, merged_ref=\"main\")."
    );
    // An unchanged list is never rewritten, however much time passes.
    assert!(t.run(BASE + 40 * MIN).is_empty());
    assert!(t.run(BASE + 3 * HOUR).is_empty());
    // A new dependent changes what the list shows: it updates without waking.
    let mut page = vector("reply-merged-blocked.json");
    page["items"][0]["blocks"]
        .as_array_mut()
        .unwrap()
        .push(json!({"item_id":"wi_later_slice","title":"Later slice"}));
    t.unsettled(vec![page]);
    let updated = t.run(BASE + 4 * HOUR);
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0]["action"], "silent");
    let body = updated[0]["item"]["body"].as_str().unwrap();
    assert!(
        body.contains(
            "Waiting on it: \"Blocked slice\" (wi_blocked_slice), \"Later slice\" (wi_later_slice)."
        ),
        "{body}"
    );
    assert!(t.run(BASE + 5 * HOUR).is_empty());
    // Settled: the item leaves the list, which clears silently, and its
    // first-seen time is forgotten.
    t.unsettled(vec![vector("reply-empty.json")]);
    let cleared = t.run(BASE + 6 * HOUR);
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0]["action"], "silent");
    assert_eq!(cleared[0]["item"]["data"]["total_count"], 0);
    assert_eq!(t.seen(), None);
    assert!(t.keys().iter().all(|key| key.starts_with("index:")));
}

#[test]
fn unowned_unmerged_unknown_and_malformed_deliveries_are_not_nudged() {
    let owned = vector("reply-merged-blocked.json")["items"][0].clone();
    // No single live head owns this one, so nobody can be nudged about it.
    let mut unowned = owned.clone();
    unowned["item_id"] = json!("wi_unowned");
    unowned["agent_id"] = Value::Null;
    let unmerged = vector("reply-unmerged.json")["items"][0].clone();
    let unknown = vector("reply-page-2.json")["items"][0].clone();
    let mut errored = unknown.clone();
    errored["item_id"] = json!("wi_errored");
    errored["delivery"] = json!("error");
    let mut no_ref = owned.clone();
    no_ref["item_id"] = json!("wi_no_ref");
    no_ref.as_object_mut().unwrap().remove("merged_ref");
    let mut t = Test::new(Vec::new());
    t.unsettled(vec![
        json!({"items":[owned, unowned, unmerged, unknown, errored, no_ref]}),
    ]);
    assert!(t.run(BASE).is_empty());
    let writes = t.run(BASE + 20 * MIN);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["agent"], "ag_owner");
    let entries = writes[0]["item"]["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["item_id"], "wi_merged_slice");
    assert_eq!(t.result["unsettled_scanned"], 6);
    assert_eq!(t.result["skipped_unsettled_unowned"], 1);
    assert_eq!(t.result["skipped_unsettled_items"], 1);
    assert_eq!(t.result["unsettled_read_error"], Value::Null);
    assert_eq!(t.seen(), Some(json!([["wi_merged_slice", BASE]])));
}

#[test]
fn every_unsettled_page_is_read_each_run() {
    let mut last = vector("reply-page-2.json");
    last["items"][0]["delivery"] = json!("merged");
    last["items"][0]["merged_ref"] = json!("main");
    let mut t = Test::new(Vec::new());
    t.unsettled(vec![vector("reply-page-1.json"), last]);
    assert!(t.run(BASE).is_empty());
    {
        let reads = t.host.unsettled_reads.lock().unwrap();
        assert_eq!(reads.len(), 2);
        assert!(reads[0].get("cursor").is_none());
        assert_eq!(reads[1]["cursor"], "1199:bg_page_199");
    }
    assert_eq!(t.result["unsettled_scanned"], 201);
    let writes = t.run(BASE + 20 * MIN);
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0]["item"]["data"]["entries"][0]["item_id"],
        "wi_page_200"
    );
    assert_eq!(t.host.unsettled_reads.lock().unwrap().len(), 4);
}

#[test]
fn a_refused_unsettled_read_is_logged_and_the_board_rules_still_run() {
    let mut t = Test::new(vec![candidates("eta_passed", 1)]);
    t.unsettled(vec![vector("reply-merged-blocked.json")]);
    assert_eq!(rule(&t.run(BASE), "eta_passed").len(), 1);
    assert_eq!(rule(&t.run(BASE + 20 * MIN), "unsettled_merged").len(), 1);
    let refusal = vector("refusal-non-global.json");
    *t.host.unsettled_refusal.lock().unwrap() = Some((0, refusal.clone()));
    // The board list changes during the refused read and is still updated.
    t.host.boards.lock().unwrap()[0]["board"]["laneBlocks"][0]["title"] = json!("Edited");
    let writes = t.run(BASE + 30 * MIN);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["item"]["data"]["rule"], "eta_passed");
    assert_eq!(writes[0]["action"], "silent");
    assert_eq!(t.result["unsettled_read_error"], refusal["code"]);
    assert_eq!(
        t.result["refusals"],
        json!([{"message":"board hygiene unsettled read failed","code":refusal["code"]}])
    );
    // Missing data clears nothing: the list and its first-seen time remain.
    assert_eq!(t.seen(), Some(json!([["wi_merged_slice", BASE]])));
    assert!(t.keys().contains(&"nudge:ag_owner:unsettled_merged".into()));
    // A refusal on a later page fails the whole read the same way.
    let mut first = vector("reply-merged-blocked.json");
    first["next_cursor"] = json!("9000000:bg_merged");
    t.unsettled(vec![first, vector("reply-empty.json")]);
    *t.host.unsettled_refusal.lock().unwrap() = Some((1, vector("refusal-unknown-stamp.json")));
    assert!(t.run(BASE + 40 * MIN).is_empty());
    assert_eq!(t.result["unsettled_read_error"], "sink_flow_not_installed");
    assert_eq!(t.seen(), Some(json!([["wi_merged_slice", BASE]])));
    // Once core answers again the unchanged list is left alone.
    *t.host.unsettled_refusal.lock().unwrap() = None;
    t.unsettled(vec![vector("reply-merged-blocked.json")]);
    assert!(t.run(BASE + 50 * MIN).is_empty());
    assert_eq!(t.result["unsettled_read_error"], Value::Null);
}

#[test]
fn unsettled_memory_is_bounded() {
    let template = vector("reply-merged-blocked.json")["items"][0].clone();
    let items: Vec<_> = (0..400)
        .map(|n| {
            let mut item = template.clone();
            item["item_id"] = json!(format!("wi_{n:03}"));
            item
        })
        .collect();
    let mut t = Test::new(Vec::new());
    t.unsettled(vec![json!({"items": items})]);
    t.run(BASE);
    assert_eq!(t.seen().unwrap().as_array().unwrap().len(), 384);
    assert_eq!(
        t.result["refusals"],
        json!([{"message":"board hygiene unsettled memory full","cap":384}])
    );
    let writes = t.run(BASE + 20 * MIN);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["item"]["data"]["total_count"], 384);
}
