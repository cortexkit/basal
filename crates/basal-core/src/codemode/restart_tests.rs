use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;

const CHILD: &str = "codemode::supervisor::tests::restart::crash_child";
const CRASH_EXIT: i32 = 86;

fn construct(f: &Fixture) -> Result<Supervisor> {
    Supervisor::new(
        f.host.store.clone(),
        f.host.clone(),
        f.catalog.clone(),
        f.source.clone(),
        f.clock.clone(),
        PreludeHash::of("parent's codemode prelude"),
        ShellDenylist::default(),
    )
}

#[test]
fn construction_recovers_every_running_row_before_returning() {
    let f = Fixture::new();
    f.host
        .store
        .write(|tx| {
            for id in ["r", "before-spawn", "terminal"] {
                store::insert_run(tx, &new_run(id), None)?;
            }
            store::insert_call(tx, "r", 0, "read", 4, CallStart::Intent { at: 100 })?;
            store::record_outcome(tx, "r", 0, Outcome::Ok, None, 103)?;
            store::insert_call(tx, "r", 1, "read", 4, CallStart::Intent { at: 100 })?;
            store::insert_call(tx, "r", 2, "read", 4, CallStart::Queued)?;
            store::commit_terminal(
                tx,
                "terminal",
                &super::super::super::store::tests::completed(),
                104,
            )?;
            Ok(())
        })
        .unwrap();
    f.clock.set(110);
    let supervisor = construct(&f).unwrap();
    for id in ["r", "before-spawn"] {
        let result = supervisor.result(id).unwrap().unwrap();
        assert_eq!(result["status"], "interrupted", "{id}");
        assert_eq!(result["error"]["code"], "basal_restarted", "{id}");
        assert_eq!(result["duration_ms"], 10);
    }
    let result = supervisor.result("r").unwrap().unwrap();
    assert_eq!(result["calls"][0]["outcome"], "ok");
    assert_eq!(result["calls"][0]["duration_ms"], 3);
    assert_eq!(result["calls"][1]["outcome"], "outcome_unknown");
    assert_eq!(result["calls"][1]["code"], "no_outcome");
    assert_eq!(result["calls"][2]["outcome"], "cancelled");
    let terminal = supervisor.result("terminal").unwrap().unwrap();
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["value"], 42);
    assert_eq!(terminal["output"], "a 1\n");
    assert_eq!(terminal["duration_ms"], 4);
    assert_eq!(
        f.host
            .store
            .read(|c| store::running_counts(c, "agent"))
            .unwrap(),
        (0, 0)
    );
    assert_eq!(*f.host.events.lock().unwrap(), ["release", "release"]);
    assert!(
        f.source.0.lock().unwrap().is_some(),
        "recovery cannot acquire a worker"
    );
    assert!(f.attempts.try_recv().is_err());
    let again = construct(&f).unwrap();
    assert_eq!(again.result("r").unwrap().unwrap(), result);
    assert_eq!(
        f.host.events.lock().unwrap().len(),
        2,
        "terminal runs are not released twice"
    );
}

#[test]
fn construction_refuses_to_serve_when_recovery_cannot_settle() {
    let f = Fixture::new();
    f.host
        .store
        .write(|tx| {
            store::insert_run(tx, &new_run("r"), None)?;
            tx.execute_batch("DROP TABLE codemode_calls")?;
            Ok(())
        })
        .unwrap();
    assert!(
        construct(&f).is_err(),
        "a supervisor must not escape failed recovery"
    );
    assert!(
        f.host.events.lock().unwrap().is_empty(),
        "failed settlement cannot release authority"
    );
    assert!(f.source.0.lock().unwrap().is_some());
}

// The provider stays in the parent test process while the supervisor process
// exits without unwinding. Its observations therefore survive both openings of
// SQLite and cannot be erased by startup recovery.
struct ProviderServer {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<Value>>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl ProviderServer {
    fn new(answer: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observations = seen.clone();
        let thread = thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                if request == "stop" {
                    break;
                }
                observations.lock().unwrap().push(request);
                stream.write_all(b"received\n").unwrap();
                if answer {
                    stream.write_all(b"{\"answer\":42}\n").unwrap();
                } else {
                    held.push(stream);
                }
            }
        });
        Self {
            address,
            seen,
            thread: Some(thread),
        }
    }
}
impl Drop for ProviderServer {
    fn drop(&mut self) {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream.write_all(b"\"stop\"\n").unwrap();
        self.thread.take().unwrap().join().unwrap();
    }
}

fn provider_client(
    address: SocketAddr,
    crash_after_send: bool,
    sent: Arc<AtomicUsize>,
) -> Arc<Provider> {
    Arc::new(move |flow, input, key| {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        writeln!(stream, "{}", json!({"flow":flow,"input":input,"key":key})).unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "received\n");
        sent.fetch_add(1, Ordering::SeqCst);
        if crash_after_send {
            std::process::exit(CRASH_EXIT);
        }
        line.clear();
        reader.read_line(&mut line).unwrap();
        Ok(serde_json::from_str(&line).unwrap())
    })
}

#[test]
fn crash_child() {
    let Ok(seam) = std::env::var("BASAL_RESTART_SEAM") else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os("BASAL_RESTART_DIR").unwrap());
    let address: SocketAddr = std::env::var("BASAL_RESTART_PROVIDER")
        .unwrap()
        .parse()
        .unwrap();
    let sends = Arc::new(AtomicUsize::new(0));
    let f = Fixture::at_with_provider(
        dir,
        Some(provider_client(address, seam == "send", sends.clone())),
    );
    if seam == "before-spawn" {
        use super::super::super::admission::{self, Admission, Platform};
        let host = basal_host::mock::MockHost::new();
        let owner = subc_protocol::Principal::Reserved {
            module_id: "core".into(),
        };
        host.set_scope_description(
            owner,
            "scope-r",
            Ok(serde_json::from_value(json!({
                "status":"live", "scope_epoch":7, "daemon_incarnation":"daemon:test",
                "owner_synced":true, "owner_configured":true,
                "scope":{"owner":{"kind":"reserved","module_id":"core"},"ref":"scope-r",
                    "scope_epoch":7,"kind":"head","attributes":{"agent_id":"agent","run_id":"r"},
                    "owner_authorized":true}
            }))
            .unwrap()),
        );
        let Admission::Admitted { start: Some(_), .. } = admission::admit(
            &f.host.store,
            &host,
            &f.clock,
            Platform::Unix,
            &json!({"run_id":"r","agent_id":"agent","program":"return 1", "catalog":[],
                "deadline_ms":10000,"scope":{"owner":{"kind":"reserved","module_id":"core"},"ref":"scope-r","epoch":7}}),
        )
        .unwrap() else {
            panic!("expected a committed admission before spawn")
        };
        std::process::exit(CRASH_EXIT);
    }
    if seam == "intent" {
        *f.supervisor.0.before_entry.lock().unwrap() =
            Some(Arc::new(|| std::process::exit(CRASH_EXIT)));
    }
    if seam == "answer" {
        *f.supervisor.0.before_record.lock().unwrap() =
            Some(Arc::new(|| std::process::exit(CRASH_EXIT)));
    }
    // The standard fixture classifies notes/read as Query. Recovery must not
    // use that classification as permission to replay an unanswered attempt.
    assert_eq!(
        f.catalog.op("notes", "read").unwrap().kind,
        Some(OpKind::Query)
    );
    f.standard();
    if seam == "queued" {
        for position in 0..8 {
            f.issue(position, "read", json!({}));
        }
        let until = Instant::now() + TIMEOUT;
        while sends.load(Ordering::SeqCst) != 8 {
            assert!(
                Instant::now() < until,
                "provider did not receive all eight attempts"
            );
            thread::yield_now();
        }
        f.issue(8, "read", json!({}));
        f.wait_calls(9);
        assert_eq!(f.calls()[8].intent_at, None);
        std::process::exit(CRASH_EXIT);
    }
    f.issue(0, "read", json!({"query":"one"}));
    if seam == "recorded" {
        let delivery = f.delivery();
        assert_eq!(delivery.settlement, Settlement::Fulfilled);
        assert_eq!(f.calls()[0].outcome, Outcome::Ok);
        std::process::exit(CRASH_EXIT);
    }
    // If the crash hook does not terminate the subprocess, fail instead of
    // waiting forever.
    let _ = f.parent.recv_timeout(TIMEOUT);
    panic!("crash seam {seam} did not exit");
}

fn restart_case(seam: &str, sends: usize, outcomes: &[&str]) {
    let dir = std::env::temp_dir().join(format!(
        "basal-restart-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let provider = ProviderServer::new(matches!(seam, "answer" | "recorded"));
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD, "--nocapture"])
        .env("BASAL_RESTART_SEAM", seam)
        .env("BASAL_RESTART_DIR", &dir)
        .env("BASAL_RESTART_PROVIDER", provider.address.to_string())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(CRASH_EXIT),
        "{seam}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // These assertions observe the committed state before any recovery code
    // runs, so a crash hook accidentally placed after recording cannot pass.
    {
        let store = Store::open(dir.join("core.db"), Durability { fullfsync: false }).unwrap();
        let Lookup::Found(run) = store.read(|c| store::lookup(c, "r")).unwrap() else {
            panic!()
        };
        assert_eq!(run.status, Status::Running);
        let calls = store.read(|c| store::calls(c, "r")).unwrap();
        assert_eq!(calls.len(), outcomes.len());
        for (index, call) in calls.iter().enumerate() {
            assert_eq!(
                call.outcome,
                if seam == "recorded" {
                    Outcome::Ok
                } else {
                    Outcome::Pending
                }
            );
            assert_eq!(call.intent_at.is_some(), !(seam == "queued" && index == 8));
            assert_eq!(
                call.entered_at.is_some(),
                seam != "intent" && !(seam == "queued" && index == 8)
            );
        }
    }
    let f = Fixture::at_with_provider(
        dir,
        Some(provider_client(
            provider.address,
            false,
            Arc::new(AtomicUsize::new(0)),
        )),
    );
    let result = f.supervisor.result("r").unwrap().unwrap();
    assert_eq!(result["status"], "interrupted");
    assert_eq!(result["error"]["code"], "basal_restarted");
    assert_eq!(result["calls"].as_array().unwrap().len(), outcomes.len());
    for (index, outcome) in outcomes.iter().enumerate() {
        let call = &result["calls"][index];
        assert_eq!(call["outcome"], *outcome, "{seam} call {index}");
        if *outcome == "outcome_unknown" {
            assert_eq!(call["code"], "no_outcome");
        } else {
            assert!(call.get("code").is_none());
        }
        assert_eq!(
            call.get("duration_ms").is_some(),
            seam != "intent" && *outcome != "cancelled"
        );
    }
    assert_eq!(
        f.host
            .store
            .read(|c| store::running_counts(c, "agent"))
            .unwrap(),
        (0, 0)
    );
    assert!(f.source.0.lock().unwrap().is_some());
    assert!(f.attempts.try_recv().is_err(), "startup cannot dispatch");
    let again = construct(&f).unwrap();
    assert_eq!(
        again.result("r").unwrap().unwrap(),
        result,
        "a second restart must be inert"
    );
    let observations = provider.seen.lock().unwrap();
    assert_eq!(
        observations.len(),
        sends,
        "{seam}: provider attempts across both restarts"
    );
    let mut keys = std::collections::BTreeSet::new();
    for observation in observations.iter() {
        assert_eq!(observation["flow"], "codemode:r");
        assert!(
            keys.insert(observation["key"].as_str().unwrap()),
            "provider saw a duplicate call"
        );
    }
    for position in 0..sends {
        assert!(keys.contains(store::call_key("r", position as u64).as_str()));
    }
}

#[test]
fn crash_after_admission_before_spawn_is_interrupted_at_construction() {
    restart_case("before-spawn", 0, &[]);
}
#[test]
fn crash_after_intent_is_unknown_and_never_sent_on_restart() {
    restart_case("intent", 0, &["outcome_unknown"]);
}
#[test]
fn crash_after_query_send_is_unknown_and_provider_sees_one_attempt() {
    restart_case("send", 1, &["outcome_unknown"]);
}
#[test]
fn crash_after_answer_before_record_is_unknown_without_resend() {
    restart_case("answer", 1, &["outcome_unknown"]);
}
#[test]
fn crash_after_queue_commit_cancels_only_the_unsent_call() {
    restart_case(
        "queued",
        8,
        &[
            "outcome_unknown",
            "outcome_unknown",
            "outcome_unknown",
            "outcome_unknown",
            "outcome_unknown",
            "outcome_unknown",
            "outcome_unknown",
            "outcome_unknown",
            "cancelled",
        ],
    );
}
#[test]
fn recorded_ok_survives_process_restart_in_result() {
    restart_case("recorded", 1, &["ok"]);
}
