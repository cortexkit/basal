use super::*;
use basal_host::mock::MockHost;
use serde_json::json;
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::time::Duration;

use super::super::store::tests::temp_store;

const NOW: i64 = 100;

fn request(id: &str, agent: &str) -> Value {
    json!({
        "run_id": id, "agent_id": agent, "program": "return 1;", "catalog": [],
        "scope": {"owner": {"kind": "reserved", "module_id": "prefrontal-core"}, "ref": "scope:run", "epoch": 19},
        "deadline_ms": 1000
    })
}

fn description(agent: &str) -> basal_host::ScopeDescription {
    serde_json::from_value(json!({
        "status": "live", "scope_epoch": 19,
        "daemon_incarnation": "daemon:attesting", "owner_synced": true,
        "owner_configured": true,
        "scope": {"owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": "scope:run", "scope_epoch": 19, "kind": "head",
            "attributes": {"agent_id": agent}, "owner_authorized": true}
    }))
    .unwrap()
}

fn set_description(host: &MockHost, answer: basal_host::ScopeDescription) {
    host.set_scope_description(
        Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "scope:run",
        Ok(answer),
    );
}

fn host(agent: &str) -> MockHost {
    let host = MockHost::new();
    set_description(&host, description(agent));
    host
}

struct DescribeHost<F>(F);

impl<F> Host for DescribeHost<F>
where
    F: Fn(
            &Principal,
            &str,
        )
            -> std::result::Result<basal_host::ScopeDescription, basal_host::ScopeDescribeError>
        + Send
        + Sync,
{
    fn scope_describe(
        &self,
        owner: &Principal,
        scope_ref: &str,
    ) -> std::result::Result<basal_host::ScopeDescription, basal_host::ScopeDescribeError> {
        (self.0)(owner, scope_ref)
    }
    fn classify(&self, _: &basal_proto::CallKind) -> basal_host::CallClass {
        panic!("admission must not classify a flow call")
    }
    fn dispatch(
        &self,
        _: &basal_host::CallRequest,
    ) -> std::result::Result<basal_host::Dispatched, basal_host::TransportError> {
        panic!("admission must not dispatch a flow call")
    }
    fn attach(&self, _: Arc<dyn basal_host::CompletionSink>) {}
    fn now_ms(&self) -> f64 {
        panic!("admission must use its injected clock")
    }
    fn random(&self) -> f64 {
        panic!("admission must not request host randomness")
    }
}

fn database_path(store: &Store) -> String {
    store
        .read(|c| {
            Ok(c.query_row(
                "SELECT file FROM pragma_database_list WHERE name = 'main'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap()
}

fn accept(store: &Store, host: &MockHost, clock: &Clock, request: &Value) -> Admission {
    admit(store, host, clock, Platform::Unix, request).unwrap()
}

fn stored(admission: Admission) -> (Box<RunRecord>, Option<Box<Start>>) {
    match admission {
        Admission::Admitted { run, start } => (run, start),
        other => panic!("expected a new run, got {other:?}"),
    }
}

fn refused(store: &Store, host: &MockHost, request: &Value, code: &str) {
    let before = row_count(store);
    match accept(store, host, &Clock::manual(NOW), request) {
        Admission::Refused(refusal) => {
            assert_eq!(refusal.code, code);
            assert!(!refusal.message.is_empty());
        }
        other => panic!("expected {code} with no start token, got {other:?}"),
    }
    assert_eq!(row_count(store), before, "refusal must create no row");
}

fn row_count(store: &Store) -> i64 {
    store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM codemode_runs", [], |r| r.get(0))?))
        .unwrap()
}

fn entry(name: &str) -> Value {
    json!({"name": name, "input_schema": true, "module": "notes", "op": "list"})
}

fn seed(store: &Store, agent: &str, count: usize) {
    for i in 0..count {
        store
            .write(|tx| {
                let id = format!("seed:{agent}:{i}");
                let run = NewRun {
                    agent_id: agent,
                    ..super::super::store::tests::new_run(&id)
                };
                assert!(store::insert_run(tx, &run, None)?);
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn known_id_precedes_every_check_and_returns_the_original_payload() {
    let t = temp_store();
    let h = host("agent");
    let mut first = request("run", "agent");
    first["description"] = json!("first description");
    first["catalog"] = json!([entry("read")]);
    let (original, start) = stored(accept(&t.store, &h, &Clock::manual(NOW), &first));
    assert!(start.is_some());
    seed(&t.store, "agent", 1);
    let repeat = json!({"run_id": "run", "program": false, "catalog": null,
        "description": "x".repeat(5000), "scope": null, "limits": {"unknown": 0}});
    let Admission::Existing(run) = admit(
        &t.store,
        &h,
        &Clock::manual(NOW),
        Platform::Windows,
        &repeat,
    )
    .unwrap() else {
        panic!("a known id must bypass all checks without another start token");
    };
    assert_eq!(run, original);
    assert_eq!(run.description.as_deref(), Some("first description"));
    assert_eq!(h.scope_queries().len(), 1);
    assert_eq!(row_count(&t.store), 2);
}

#[test]
fn concurrent_first_admits_grant_one_start_token() {
    let t = Arc::new(temp_store());
    seed(&t.store, "agent", 1);
    let mock = Arc::new(host("agent"));
    let rendezvous = Arc::new((Mutex::new(0), Condvar::new()));
    let h = Arc::new(DescribeHost({
        let mock = mock.clone();
        move |owner: &Principal, scope_ref: &str| {
            // Both callers must finish their initial unknown-id read before
            // either can insert. A bounded wait also catches a misplaced lock
            // without leaving the test suite stuck at a barrier forever.
            let (arrivals, wake) = &*rendezvous;
            let mut arrived = arrivals.lock().unwrap();
            *arrived += 1;
            wake.notify_all();
            let (arrived, _) = wake
                .wait_timeout_while(arrived, Duration::from_secs(5), |n| *n < 2)
                .unwrap();
            assert_eq!(
                *arrived, 2,
                "both admissions must reach describe without a write lock"
            );
            mock.scope_describe(owner, scope_ref)
        }
    }));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let (t, h) = (t.clone(), h.clone());
        threads.push(std::thread::spawn(move || {
            admit(
                &t.store,
                &*h,
                &Clock::manual(NOW),
                Platform::Unix,
                &request("same", "agent"),
            )
            .unwrap()
        }));
    }
    let mut starts = 0;
    let mut existing = 0;
    for thread in threads {
        match thread.join().unwrap() {
            Admission::Admitted { start: Some(_), .. } => starts += 1,
            Admission::Existing(_) => existing += 1,
            other => panic!("unexpected concurrent admission {other:?}"),
        }
    }
    assert_eq!((starts, existing), (1, 1));
    assert_eq!(mock.scope_queries().len(), 2);
    assert_eq!(row_count(&t.store), 2);
}

#[test]
fn describe_can_write_while_admission_is_attesting() {
    let t = temp_store();
    let path = database_path(&t.store);
    let h = DescribeHost(move |_: &Principal, _: &str| {
        // A separate connection probes SQLite's actual write lock, with no
        // busy wait. Calling Store::write here could instead deadlock on the
        // store's connection mutex and hide the regression behind a timeout.
        let mut connection = rusqlite::Connection::open(&path).unwrap();
        connection.busy_timeout(Duration::ZERO).unwrap();
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("describe must be able to acquire the SQLite write lock");
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('describe_probe', 'written')",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
        Ok(description("agent"))
    });
    let (_, start) = stored(
        admit(
            &t.store,
            &h,
            &Clock::manual(NOW),
            Platform::Unix,
            &request("run", "agent"),
        )
        .unwrap(),
    );
    assert!(start.is_some());
    let written: String = t
        .store
        .read(|c| {
            Ok(c.query_row(
                "SELECT value FROM meta WHERE key = 'describe_probe'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(written, "written");
}

#[test]
fn known_id_needs_no_write_lock_or_describe() {
    let t = temp_store();
    let h = host("agent");
    let (original, _) = stored(accept(
        &t.store,
        &h,
        &Clock::manual(NOW),
        &request("run", "agent"),
    ));
    let path = database_path(&t.store);
    t.store
        .read(|c| {
            c.busy_timeout(Duration::ZERO)?;
            Ok(())
        })
        .unwrap();
    let mut connection = rusqlite::Connection::open(path).unwrap();
    let writer = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let Admission::Existing(run) = admit(
        &t.store,
        &h,
        &Clock::manual(NOW),
        Platform::Windows,
        &json!({"run_id":"run"}),
    )
    .unwrap() else {
        panic!("known id must return through a read-only lookup");
    };
    assert_eq!(run, original);
    assert_eq!(h.scope_queries().len(), 1);
    writer.rollback().unwrap();
}

#[test]
fn pruned_id_is_unknown_and_never_admitted_again() {
    let t = temp_store();
    let h = host("agent");
    let mut req = request("pruned", "agent");
    req["deadline_ms"] = json!(NOW);
    stored(accept(&t.store, &h, &Clock::manual(NOW), &req));
    super::super::retention::sweep(&t.store, NOW + 24 * 60 * 60 * 1000 + 1).unwrap();
    req["deadline_ms"] = json!(i64::MAX);
    refused(&t.store, &h, &req, "unknown_run");
    assert_eq!(h.scope_queries().len(), 1);
    assert_eq!(
        t.store.read(|c| store::lookup(c, "pruned")).unwrap(),
        Lookup::Pruned
    );
}

#[test]
fn injected_windows_is_refused_before_scope_or_request_checks() {
    let t = temp_store();
    let h = host("agent");
    let Admission::Refused(refusal) = admit(
        &t.store,
        &h,
        &Clock::manual(NOW),
        Platform::Windows,
        &json!({}),
    )
    .unwrap() else {
        panic!("Windows must refuse without a start token");
    };
    assert_eq!(refusal.code, "unsupported_platform");
    assert_eq!(row_count(&t.store), 0);
    assert!(h.scope_queries().is_empty());
}

#[test]
fn scope_shape_precedes_attestation_and_needs_no_flow_targets() {
    let t = temp_store();
    let h = host("agent");
    for scope in [
        None,
        Some(Value::Null),
        Some(json!({})),
        Some(
            json!({"owner": {"kind": "reserved", "module_id": "prefrontal-core"}, "ref": "scope:run", "epoch": "19"}),
        ),
        Some(
            json!({"owner": {"kind": "reserved", "module_id": "prefrontal-core"}, "ref": "scope:run", "epoch": 19, "extra": true}),
        ),
    ] {
        let mut req = request("run", "agent");
        if let Some(scope) = scope {
            req["scope"] = scope;
        } else {
            req.as_object_mut().unwrap().remove("scope");
        }
        req["program"] = json!(false);
        refused(&t.store, &h, &req, "no_scope");
    }
    assert!(h.scope_queries().is_empty());
    let (run, start) = stored(accept(
        &t.store,
        &h,
        &Clock::manual(NOW),
        &request("run", "agent"),
    ));
    assert_eq!(run.status, Status::Running);
    let start = start.unwrap();
    assert!(start.scope.targets.is_empty());
    assert_eq!(start.scope.selector.scope_ref, "scope:run");
    assert_eq!(start.scope.selector.epoch, 19);
}

#[test]
fn non_reserved_owner_and_blank_ref_are_refused_before_describe() {
    let t = temp_store();
    let h = host("agent");
    for scope in [
        json!({"owner": {"kind": "direct"}, "ref": "scope:run", "epoch": 19}),
        json!({"owner": {"kind": "reserved", "module_id": "prefrontal-core"}, "ref": " \t\n", "epoch": 19}),
    ] {
        let mut req = request("run", "agent");
        req["scope"] = scope;
        refused(&t.store, &h, &req, "no_scope");
    }
    assert!(h.scope_queries().is_empty());
}

#[test]
fn unavailable_describe_has_no_authority() {
    let t = temp_store();
    let h = MockHost::new();
    refused(&t.store, &h, &request("run", "agent"), "no_scope");
    assert_eq!(h.scope_queries().len(), 1);
}

#[test]
fn non_live_status_and_epoch_mismatch_precede_request_validation() {
    let t = temp_store();
    let h = host("agent");
    for answer in [
        {
            let mut d = description("agent");
            d.status = ScopeStatus::Ended;
            d
        },
        {
            let mut d = description("agent");
            d.scope_epoch = Some(20);
            d
        },
        {
            let mut d = description("agent");
            d.scope_epoch = None;
            d
        },
        {
            let mut d = description("agent");
            d.scope = None;
            d
        },
    ] {
        set_description(&h, answer);
        let mut req = request("run", "wrong agent");
        req["program"] = json!(false);
        refused(&t.store, &h, &req, "no_scope");
    }
}

#[test]
fn attested_agent_mismatch_precedes_request_limits_and_catalog() {
    let t = temp_store();
    let h = host("actual-agent");
    let mut req = request("run", "claimed-agent");
    req["program"] = json!(false);
    req["limits"] = json!({"unknown": 0});
    req["catalog"] = json!(false);
    refused(&t.store, &h, &req, "scope_mismatch");
    let mut d = description("actual-agent");
    d.scope.as_mut().unwrap().attributes.agent_id = None;
    set_description(&h, d);
    refused(
        &t.store,
        &h,
        &request("run", "claimed-agent"),
        "scope_mismatch",
    );
}

#[test]
fn required_request_fields_are_checked_after_attestation_before_limits() {
    let t = temp_store();
    let h = host("agent");
    for key in ["run_id", "agent_id", "program", "catalog", "deadline_ms"] {
        let mut req = request("run", "agent");
        req.as_object_mut().unwrap().remove(key);
        req["limits"] = json!({"unknown": 0});
        refused(&t.store, &h, &req, "invalid_request");
    }
    for (key, value) in [
        ("run_id", json!(42)),
        ("agent_id", json!(null)),
        ("program", json!([])),
        ("deadline_ms", json!(1.5)),
        ("deadline_ms", json!("1000")),
        ("deadline_ms", json!(u64::MAX)),
        ("description", json!(null)),
    ] {
        let mut req = request("run", "agent");
        req[key] = value;
        refused(&t.store, &h, &req, "invalid_request");
    }
    assert_eq!(h.scope_queries().len(), 12);
}

#[test]
fn script_byte_limit_is_inclusive() {
    let t = temp_store();
    let h = host("agent");
    let mut req = request("run", "agent");
    req["program"] = json!("x".repeat(MAX_SCRIPT_BYTES + 1));
    refused(&t.store, &h, &req, "invalid_request");
    req["program"] = json!("x".repeat(MAX_SCRIPT_BYTES));
    let (run, start) = stored(accept(&t.store, &h, &Clock::manual(NOW), &req));
    assert_eq!(run.program.len(), MAX_SCRIPT_BYTES);
    assert!(start.is_some());
}

#[test]
fn description_utf8_byte_limit_is_inclusive_and_preserves_the_text() {
    let t = temp_store();
    let h = host("agent");
    let mut req = request("run", "agent");
    let text = "é".repeat(512);
    req["description"] = json!(format!("{text}x"));
    refused(&t.store, &h, &req, "invalid_request");
    req["description"] = json!(text);
    let (run, _) = stored(accept(&t.store, &h, &Clock::manual(NOW), &req));
    assert_eq!(run.description.as_deref(), Some(text.as_str()));
    let Lookup::Found(found) = t.store.read(|c| store::lookup(c, "run")).unwrap() else {
        panic!("missing run");
    };
    assert_eq!(found.description, run.description);
}

#[test]
fn descriptions_refuse_each_line_break_character() {
    let t = temp_store();
    let h = host("agent");
    for newline in ['\n', '\r', '\u{2028}', '\u{2029}'] {
        let mut req = request("run", "agent");
        req["description"] = json!(format!("a{newline}b"));
        refused(&t.store, &h, &req, "invalid_request");
    }
}

#[test]
fn limits_refuse_unknown_keys_and_non_positive_non_integer_values() {
    let t = temp_store();
    let h = host("agent");
    for limits in [
        json!(null),
        json!([]),
        json!({"unknown": 1}),
        json!({"tool_calls": 0}),
        json!({"wall_ms": 0}),
        json!({"output_bytes": 0}),
        json!({"wall_ms": -1}),
        json!({"wall_ms": 1.5}),
        json!({"tool_calls": "1"}),
        json!({"wall_ms": u64::MAX}),
    ] {
        let mut req = request("run", "agent");
        req["limits"] = limits;
        req["catalog"] = json!(false);
        refused(&t.store, &h, &req, "invalid_limits");
    }
}

#[test]
fn tool_and_output_limits_accept_the_cap_and_refuse_cap_plus_one() {
    let t = temp_store();
    let h = host("agent");
    for limits in [json!({"tool_calls": 201}), json!({"output_bytes": 65537})] {
        let mut req = request("run", "agent");
        req["limits"] = limits;
        refused(&t.store, &h, &req, "invalid_limits");
    }
    let mut req = request("run", "agent");
    req["limits"] = json!({"tool_calls": 200, "output_bytes": 65536, "wall_ms": i64::MAX});
    let (run, start) = stored(accept(&t.store, &h, &Clock::manual(NOW), &req));
    let start = start.unwrap();
    assert_eq!(
        start.limits,
        Limits {
            wall_ms: Some(i64::MAX),
            ..Limits::default()
        }
    );
    assert_eq!(start.wall_deadline_ms, 1000);
    assert_eq!(
        serde_json::from_str::<Limits>(&run.limits).unwrap(),
        start.limits
    );
}

#[test]
fn catalog_array_and_exact_entry_keys_are_required() {
    let t = temp_store();
    let h = host("agent");
    for catalog in [
        json!(null),
        json!({}),
        json!([false]),
        json!([{"name":"read"}]),
        json!([{"name":"read", "module":"notes", "op":"list", "extra":true}]),
        {
            let mut e = entry("read");
            e["extra"] = json!(true);
            json!([e])
        },
    ] {
        let mut req = request("run", "agent");
        req["catalog"] = catalog;
        refused(&t.store, &h, &req, "invalid_catalog");
    }
}

#[test]
fn tool_names_are_ascii_identifiers_of_at_most_128_bytes() {
    let t = temp_store();
    let h = host("agent");
    for name in [
        "1abc".to_owned(),
        "".to_owned(),
        "a-b".to_owned(),
        "é".to_owned(),
        "x".repeat(129),
    ] {
        let mut req = request("run", "agent");
        req["catalog"] = json!([entry(&name)]);
        refused(&t.store, &h, &req, "invalid_catalog");
    }
    let mut req = request("run", "agent");
    req["catalog"] = json!([entry(&"x".repeat(128)), entry("_A0")]);
    let (_, start) = stored(accept(&t.store, &h, &Clock::manual(NOW), &req));
    assert_eq!(start.unwrap().tools.len(), 2);
}

#[test]
fn reserved_tool_names_are_refused_and_named_in_the_message() {
    let t = temp_store();
    let h = host("agent");
    for name in ["__proto__", "constructor", "prototype", "tools", "console"] {
        let mut req = request("run", "agent");
        req["catalog"] = json!([entry(name)]);
        let Admission::Refused(refusal) = accept(&t.store, &h, &Clock::manual(NOW), &req) else {
            panic!("reserved name {name} was admitted");
        };
        assert_eq!(refusal.code, "invalid_catalog");
        assert!(refusal.message.contains(name));
        assert_eq!(row_count(&t.store), 0);
    }
}

#[test]
fn duplicate_tool_names_are_refused() {
    let t = temp_store();
    let h = host("agent");
    let mut req = request("run", "agent");
    req["catalog"] = json!([entry("read"), entry("read")]);
    refused(&t.store, &h, &req, "invalid_catalog");
}

#[test]
fn module_and_op_are_nonempty_strings_with_a_128_byte_cap() {
    let t = temp_store();
    let h = host("agent");
    for field in ["module", "op"] {
        for value in [json!(""), json!("x".repeat(129)), json!(null)] {
            let mut e = entry("read");
            e[field] = value;
            let mut req = request("run", "agent");
            req["catalog"] = json!([e]);
            refused(&t.store, &h, &req, "invalid_catalog");
        }
    }
}

#[test]
fn schema_failures_and_external_refs_refuse_before_insertion() {
    let t = temp_store();
    let h = host("agent");
    for schema in [
        json!({"type":"uncompilable"}),
        json!({"$ref":"https://example.org/schema"}),
        json!({"$ref":"file:///etc/schema.json"}),
    ] {
        let mut e = entry("read");
        e["input_schema"] = schema;
        let mut req = request("run", "agent");
        req["catalog"] = json!([e]);
        refused(&t.store, &h, &req, "invalid_catalog");
    }
}

#[test]
fn same_document_schema_compiles_once_and_scope_targets_come_from_modules() {
    let t = temp_store();
    let h = host("agent");
    let mut e = entry("read");
    e["input_schema"] = json!({"$defs":{"input":{"type":"integer"}}, "$ref":"#/$defs/input"});
    let mut other = entry("other");
    other["module"] = json!("calendar");
    let mut req = request("run", "agent");
    req["catalog"] = json!([e, other, entry("third")]);
    let (run, start) = stored(accept(&t.store, &h, &Clock::manual(NOW), &req));
    let start = start.unwrap();
    assert!(start.tools["read"].input_schema.is_valid(&json!(42)));
    assert!(!start.tools["read"].input_schema.is_valid(&json!("42")));
    assert_eq!(
        start.scope.targets.into_iter().collect::<Vec<_>>(),
        vec!["calendar", "notes"]
    );
    assert_eq!(run.agent_id, "agent");
    assert_eq!(run.catalog_digest.len(), 64);
}

#[test]
fn agent_cap_is_two_attested_nonterminal_runs_in_the_transaction() {
    for at_cap in [2, 3] {
        let t = temp_store();
        let h = host("actual");
        seed(&t.store, "actual", at_cap);
        refused(&t.store, &h, &request("new", "actual"), "busy");
        refused(&t.store, &h, &request("new", "other"), "scope_mismatch");
    }
    let t = temp_store();
    let h = host("actual");
    seed(&t.store, "actual", 1);
    let (_, start) = stored(accept(
        &t.store,
        &h,
        &Clock::manual(NOW),
        &request("new", "actual"),
    ));
    assert!(start.is_some());
    assert_eq!(
        t.store
            .read(|c| store::running_counts(c, "actual"))
            .unwrap(),
        (2, 2)
    );
}

#[test]
fn basal_cap_is_sixteen_nonterminal_runs_in_the_transaction() {
    for at_cap in [16, 17] {
        let t = temp_store();
        let h = host("new-agent");
        for i in 0..at_cap {
            seed(&t.store, &format!("agent:{i}"), 1);
        }
        refused(&t.store, &h, &request("new", "new-agent"), "busy");
    }
    let t = temp_store();
    let h = host("new-agent");
    for i in 0..15 {
        seed(&t.store, &format!("agent:{i}"), 1);
    }
    stored(accept(
        &t.store,
        &h,
        &Clock::manual(NOW),
        &request("new", "new-agent"),
    ));
    assert_eq!(
        t.store
            .read(|c| store::running_counts(c, "new-agent"))
            .unwrap(),
        (1, 16)
    );
}

#[test]
fn concurrent_distinct_admits_cannot_overfill_the_agent_cap() {
    let t = Arc::new(temp_store());
    seed(&t.store, "agent", 1);
    let h = Arc::new(host("agent"));
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|i| {
            let (t, h, barrier) = (t.clone(), h.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                accept(
                    &t.store,
                    &h,
                    &Clock::manual(NOW),
                    &request(&format!("new:{i}"), "agent"),
                )
            })
        })
        .collect();
    let mut admitted = 0;
    let mut busy = 0;
    for thread in threads {
        match thread.join().unwrap() {
            Admission::Admitted { start: Some(_), .. } => admitted += 1,
            Admission::Refused(refusal) if refusal.code == "busy" => busy += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!((admitted, busy), (1, 1));
    assert_eq!(
        t.store.read(|c| store::running_counts(c, "agent")).unwrap(),
        (2, 2)
    );
}

#[test]
fn deadline_at_or_before_admission_inserts_terminal_without_a_start_token() {
    let t = temp_store();
    let h = host("agent");
    for deadline in [NOW - 1, NOW] {
        let mut req = request(&format!("expired:{deadline}"), "agent");
        req["deadline_ms"] = json!(deadline);
        let (run, start) = stored(accept(&t.store, &h, &Clock::manual(NOW), &req));
        assert!(
            start.is_none(),
            "expired run must not register a scope or spawn"
        );
        assert_eq!(run.status, Status::BudgetExhausted(Budget::Wall));
        assert_eq!(run.error.unwrap().code, "budget_exhausted:wall");
        assert_eq!(run.duration_ms, Some(0));
        assert_eq!(run.ended_at, Some(NOW));
        assert!(run.output.is_empty());
        assert!(
            t.store
                .read(|c| store::calls(c, &run.run_id))
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        t.store.read(|c| store::running_counts(c, "agent")).unwrap(),
        (0, 0)
    );
}

#[test]
fn wall_deadline_uses_the_injected_clock_and_saturating_addition() {
    let t = temp_store();
    let h = host("agent");
    let mut req = request("run", "agent");
    req["limits"] = json!({"wall_ms": 1});
    let (_, start) = stored(accept(&t.store, &h, &Clock::manual(NOW), &req));
    assert_eq!(start.unwrap().wall_deadline_ms, NOW + 1);
    let (_, start) = stored(accept(
        &t.store,
        &h,
        &Clock::manual(NOW),
        &request("default", "agent"),
    ));
    assert_eq!(start.unwrap().wall_deadline_ms, 1000);
}

#[test]
fn invalid_catalog_precedes_busy() {
    let t = temp_store();
    let h = host("agent");
    seed(&t.store, "agent", 2);
    let mut req = request("run", "agent");
    req["catalog"] = json!([entry("console")]);
    refused(&t.store, &h, &req, "invalid_catalog");
}
