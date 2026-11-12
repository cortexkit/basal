//! Admission of codemode runs asked for by a worker session (a *worker run*)
//! next to runs asked for by a head (an *agent run*).

use super::*;
use basal_host::mock::MockHost;
use basal_host::run_scope::Scope;
use basal_host::transport::{Transport, WireError};
use serde_json::json;
use std::sync::Mutex;

const HEAD: &str = "SYNAPSE";

fn core() -> Principal {
    Principal::Reserved {
        module_id: "prefrontal-core".into(),
    }
}

/// prefrontal-core's run-scope ops. Every open gets run scope `rs-<run_id>`
/// at epoch 7; each request is kept so a test can read what basal sent.
#[derive(Default)]
struct Core {
    opens: Mutex<Vec<Value>>,
}

impl Transport for Core {
    fn catalog(&self) -> std::result::Result<Value, WireError> {
        Ok(json!({"modules":[]}))
    }
    fn management(
        &self,
        _: &str,
        op: &str,
        params: Value,
    ) -> std::result::Result<Value, WireError> {
        match op {
            "codemode.run_scope.open" => {
                self.opens.lock().unwrap().push(params.clone());
                Ok(
                    json!({"scope":{"ref":format!("rs-{}", params["run_id"].as_str().unwrap()),"epoch":7},
                    "expires_at_ms":params["expires_at_ms"],"catalog":[]}),
                )
            }
            "codemode.run_scope.close" => Ok(json!({"closed":true,"already_closed":false})),
            other => panic!("unexpected management op {other}"),
        }
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> std::result::Result<Value, WireError> {
        panic!("admission does not call tools")
    }
}

/// Describe run `run`'s scope with `attributes` and, when given, a parent
/// `(ref, epoch)` owned by core.
fn describe(host: &MockHost, run: &str, attributes: Value, parent: Option<(&str, u64)>) {
    let mut scope = json!({"owner":core(),"ref":format!("rs-{run}"),"scope_epoch":7,
        "kind":"ephemeral","owner_authorized":true,"attributes":attributes});
    if let Some((reference, epoch)) = parent {
        scope["parent"] = json!({"owner":core(),"ref":reference,"scope_epoch":epoch});
    }
    host.set_scope_description(
        core(),
        &format!("rs-{run}"),
        Ok(serde_json::from_value(json!({
        "status":"live","scope_epoch":7,"daemon_incarnation":"daemon","owner_synced":true,
        "owner_configured":true,"scope":scope}))
        .unwrap()),
    );
}

/// A worker run's stamp as core and the daemon make it: the run, the head the
/// worker works for, and the worker's own scope as parent.
fn worker_stamp(host: &MockHost, run: &str, worker: &str) {
    describe(
        host,
        run,
        json!({"run_id":run,"on_behalf_of_agent":HEAD}),
        Some((worker, 3)),
    );
}

struct Fixture {
    store: super::super::store::tests::TempStore,
    host: MockHost,
    core: Core,
    clock: Clock,
    bind: subc_protocol::BindIdentity,
}

impl Fixture {
    fn new() -> Self {
        Self {
            store: super::super::store::tests::temp_store(),
            host: MockHost::default(),
            core: Core::default(),
            clock: Clock::manual(100),
            bind: subc_protocol::BindIdentity::new("/work", "test", "session"),
        }
    }

    fn admit(&self, run: &str, caller: RunCaller<'_>, invoking: &str) -> Admission {
        admit_tool(
            &self.store.store,
            &self.host,
            &self.core,
            &self.clock,
            Platform::current(),
            &ToolInvocation {
                run_id: run,
                caller,
                invoking_scope: Scope {
                    reference: invoking.into(),
                    epoch: 3,
                },
                carrier: "direct",
                call_key: None,
                record_scope: "null",
                custodian: "reserved:prefrontal-core",
                bind_identity: &self.bind,
            },
            &json!({"code":"return 42;"}),
        )
        .unwrap()
    }

    /// `(worker_scope, on_behalf_of_agent, agent_id)` as stored for `run`.
    fn recorded(&self, run: &str) -> (Option<String>, Option<String>, String) {
        self.store
            .store
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT worker_scope,on_behalf_of_agent,agent_id FROM codemode_runs WHERE run_id=?1",
                    [run],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .unwrap()
    }
}

/// The run started, with a start token.
fn started(admission: Admission) {
    match admission {
        Admission::Admitted {
            run,
            start: Some(_),
        } => assert_eq!(run.status, store::Status::Running),
        other => panic!("expected a started run, got {other:?}"),
    }
}

/// The run was admitted and then failed with `code` before it started.
fn failed(admission: Admission, code: &str) {
    match admission {
        Admission::Admitted { run, start: None } => {
            assert_eq!(run.status, store::Status::Failed);
            assert_eq!(run.error.as_ref().unwrap().code, code, "{:?}", run.error);
        }
        other => panic!("expected a failed run, got {other:?}"),
    }
}

#[test]
fn a_worker_run_opens_without_an_agent_id_and_records_its_head_for_audit() {
    let f = Fixture::new();
    worker_stamp(&f.host, "w1", "task:1");
    started(f.admit("w1", RunCaller::Worker, "task:1"));
    let opens = f.core.opens.lock().unwrap();
    assert_eq!(opens.len(), 1);
    assert!(opens[0].get("agent_id").is_none(), "{}", opens[0]);
    assert_eq!(
        opens[0]["invoking_scope"],
        json!({"ref":"task:1","epoch":3})
    );
    assert_eq!(opens[0]["run_id"], "w1");
    drop(opens);
    // The head is attribution only: it is never the run's agent.
    assert_eq!(
        f.recorded("w1"),
        (Some("task:1".into()), Some(HEAD.into()), String::new())
    );
}

#[test]
fn an_agent_run_is_unchanged() {
    let f = Fixture::new();
    describe(&f.host, "a1", json!({"run_id":"a1","agent_id":HEAD}), None);
    started(f.admit("a1", RunCaller::Agent(HEAD), "head:1"));
    let opens = f.core.opens.lock().unwrap();
    assert_eq!(opens[0]["agent_id"], HEAD);
    let mut keys: Vec<_> = opens[0].as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["agent_id", "expires_at_ms", "invoking_scope", "run_id"]
    );
    drop(opens);
    assert_eq!(f.recorded("a1"), (None, None, HEAD.to_owned()));
}

#[test]
fn a_worker_run_scope_must_be_a_worker_runs_shape() {
    let cases = [
        // An agent identity on a worker run, alone or beside the head.
        (json!({"run_id":"w","agent_id":HEAD}), Some(("task:1", 3))),
        (
            json!({"run_id":"w","agent_id":HEAD,"on_behalf_of_agent":HEAD}),
            Some(("task:1", 3)),
        ),
        // No head named at all.
        (json!({"run_id":"w"}), Some(("task:1", 3))),
        // Another run's scope.
        (
            json!({"run_id":"other","on_behalf_of_agent":HEAD}),
            Some(("task:1", 3)),
        ),
        // Not a child of the worker that asked, or of its current epoch.
        (json!({"run_id":"w","on_behalf_of_agent":HEAD}), None),
        (
            json!({"run_id":"w","on_behalf_of_agent":HEAD}),
            Some(("task:2", 3)),
        ),
        (
            json!({"run_id":"w","on_behalf_of_agent":HEAD}),
            Some(("task:1", 4)),
        ),
    ];
    for (attributes, parent) in cases {
        let f = Fixture::new();
        describe(&f.host, "w", attributes.clone(), parent);
        failed(f.admit("w", RunCaller::Worker, "task:1"), "scope_mismatch");
        assert_eq!(f.recorded("w").1, None, "{attributes} {parent:?}");
    }
}

#[test]
fn an_agent_run_scope_must_not_have_a_worker_runs_shape() {
    for attributes in [
        json!({"run_id":"a","on_behalf_of_agent":HEAD}),
        json!({"run_id":"a","agent_id":HEAD,"on_behalf_of_agent":HEAD}),
    ] {
        let f = Fixture::new();
        describe(&f.host, "a", attributes.clone(), Some(("head:1", 3)));
        failed(
            f.admit("a", RunCaller::Agent(HEAD), "head:1"),
            "scope_mismatch",
        );
    }
}

#[test]
fn core_refusing_a_worker_run_reaches_the_caller_with_cores_code() {
    struct Refusing;
    impl Transport for Refusing {
        fn catalog(&self) -> std::result::Result<Value, WireError> {
            Ok(json!({"modules":[]}))
        }
        fn management(&self, _: &str, _: &str, _: Value) -> std::result::Result<Value, WireError> {
            Err(WireError::Refused {
                code: "worker_runs_unsupported".into(),
                message: "core does not open worker runs yet".into(),
            })
        }
        fn tool(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> std::result::Result<Value, WireError> {
            panic!("admission does not call tools")
        }
    }
    let f = Fixture::new();
    let admission = admit_tool(
        &f.store.store,
        &f.host,
        &Refusing,
        &f.clock,
        Platform::current(),
        &ToolInvocation {
            run_id: "w",
            caller: RunCaller::Worker,
            invoking_scope: Scope {
                reference: "task:1".into(),
                epoch: 3,
            },
            carrier: "direct",
            call_key: None,
            record_scope: "null",
            custodian: "reserved:prefrontal-core",
            bind_identity: &f.bind,
        },
        &json!({"code":"return 42;"}),
    )
    .unwrap();
    failed(admission, "worker_runs_unsupported");
}

#[test]
fn the_per_caller_cap_keys_on_the_workers_own_scope_not_its_head() {
    let f = Fixture::new();
    for (run, worker) in [
        ("a1", "task:A"),
        ("a2", "task:A"),
        ("b1", "task:B"),
        ("b2", "task:B"),
    ] {
        worker_stamp(&f.host, run, worker);
        started(f.admit(run, RunCaller::Worker, worker));
    }
    // Both workers work for the same head and each has its own two runs.
    for (run, worker) in [("a3", "task:A"), ("b3", "task:B")] {
        worker_stamp(&f.host, run, worker);
        match f.admit(run, RunCaller::Worker, worker) {
            Admission::Refused(refusal) => assert_eq!(refusal.code, "busy"),
            other => panic!("expected busy, got {other:?}"),
        }
    }
    // The head's own cap is untouched by its workers' runs.
    describe(&f.host, "h1", json!({"run_id":"h1","agent_id":HEAD}), None);
    started(f.admit("h1", RunCaller::Agent(HEAD), "head:1"));
    let counts = |owner: store::RunOwner<'static>| {
        f.store
            .store
            .read(|conn| store::running_counts(conn, owner))
            .unwrap()
    };
    assert_eq!(counts(store::RunOwner::Worker("task:A")), (2, 5));
    assert_eq!(counts(store::RunOwner::Agent(HEAD)), (1, 5));
    // An agent whose id is empty never counts a worker run as its own.
    assert_eq!(counts(store::RunOwner::Agent("")), (0, 5));
}

#[test]
fn the_overall_cap_counts_worker_and_agent_runs_together() {
    let f = Fixture::new();
    for n in 0..MAX_BASAL_RUNS {
        let (run, worker) = (format!("w{n}"), format!("task:{n}"));
        worker_stamp(&f.host, &run, &worker);
        started(f.admit(&run, RunCaller::Worker, &worker));
    }
    describe(&f.host, "h", json!({"run_id":"h","agent_id":HEAD}), None);
    match f.admit("h", RunCaller::Agent(HEAD), "head:1") {
        Admission::Refused(refusal) => assert_eq!(refusal.code, "busy"),
        other => panic!("expected busy, got {other:?}"),
    }
}
