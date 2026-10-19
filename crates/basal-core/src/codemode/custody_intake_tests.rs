use super::*;
use std::sync::atomic::AtomicUsize;

type ReplyFn = dyn Fn(&str, Value) -> std::result::Result<Value, WireError> + Send + Sync;
struct Provider {
    reply: Box<ReplyFn>,
}
impl Transport for Provider {
    fn role_for_run(
        &self,
        _: &str,
        _: &str,
        name: &str,
        arguments: Value,
        _: &basal_host::transport::RunToolOptions,
    ) -> std::result::Result<Value, WireError> {
        (self.reply)(name, arguments)
    }
    fn catalog(&self) -> std::result::Result<Value, WireError> {
        panic!("no catalog")
    }
    fn management(&self, _: &str, _: &str, _: Value) -> std::result::Result<Value, WireError> {
        panic!("no management")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> std::result::Result<Value, WireError> {
        panic!("no action resend")
    }
}
fn options() -> basal_host::transport::RunToolOptions {
    basal_host::transport::RunToolOptions {
        identity: subc_protocol::BindIdentity::new("/", "basal", "test"),
        reply_timeout: Duration::from_secs(1),
    }
}
fn target(custody: &Custody, run: &str) -> mpsc::Receiver<Event> {
    let (events, receiver) = mpsc::channel();
    custody.register(
        "provider",
        Target {
            run: run.into(),
            scope: ScopeIdentity {
                owner: "reserved:core".into(),
                scope_ref: run.into(),
                scope_epoch: 7,
            },
            call: HostCall {
                position: 0,
                kind: CallKind::Tool {
                    name: "read".into(),
                },
                args: JsonText::null(),
            },
            events,
            options: options(),
            held: false,
        },
    );
    custody.mark_held("provider", run, 0, true);
    receiver
}
fn entry(run: &str, value: &str) -> Value {
    serde_json::to_value(
        LateEntry::new(
            "result",
            "reserved:core",
            run,
            7,
            "reserved:basal",
            store::call_key(run, 0),
            format!("e-{value}"),
            100,
        )
        .with_result(json!(value)),
    )
    .unwrap()
}
fn page(entries: Vec<Value>) -> Value {
    json!({"entries":entries,"cursor":{"provider_incarnation":"i1","seq":10},"more":false})
}

fn skip_entry(bad: Value) {
    let custody = Arc::new(Custody::default());
    let receiver = target(&custody, "a");
    let acked = Arc::new(AtomicUsize::new(0));
    let count = acked.clone();
    let provider = Provider {
        reply: Box::new(move |name, _| match name {
            "late_results" => Ok(page(vec![bad.clone(), entry("a", "good")])),
            "late_results.ack" => {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(json!({}))
            }
            _ => panic!("unexpected op"),
        }),
    };
    let taking = custody.clone();
    let driver = thread::spawn(move || {
        let Event::LateAnswer { entry, ack, .. } =
            receiver.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("expected final")
        };
        assert_eq!(
            entry.result,
            Some(json!("good")),
            "bad identity entries must not be delivered"
        );
        taking.remove("provider", "a", 0);
        ack.send(()).unwrap();
    });
    assert!(
        !custody
            .pull(&provider, "codemode:a", "provider", &options())
            .unwrap()
    );
    driver.join().unwrap();
    assert_eq!(
        acked.load(Ordering::SeqCst),
        1,
        "the cursor advances past the skipped entry"
    );
}
#[test]
fn foreign_scope_entry_is_skipped_and_the_page_is_acked() {
    let mut bad = entry("a", "wrong-scope");
    bad["ref"] = json!("other");
    skip_entry(bad);
}
#[test]
fn foreign_custodian_entry_is_skipped_and_the_page_is_acked() {
    let mut bad = entry("a", "wrong-custodian");
    bad["custodian"] = json!("reserved:other");
    skip_entry(bad);
}
#[test]
fn malformed_entry_does_not_poison_the_following_valid_entry() {
    skip_entry(json!({"kind":"result","result":"bad"}));
}

#[test]
fn another_ending_driver_cannot_fail_a_live_runs_intake() {
    let custody = Arc::new(Custody::default());
    let a = target(&custody, "a");
    let b = target(&custody, "b");
    let acked = Arc::new(AtomicUsize::new(0));
    let count = acked.clone();
    let provider = Provider {
        reply: Box::new(move |name, _| match name {
            "late_results" => Ok(page(vec![entry("b", "ending"), entry("a", "live")])),
            "late_results.ack" => {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(json!({}))
            }
            _ => panic!("unexpected op"),
        }),
    };
    let ending = custody.clone();
    let b_driver = thread::spawn(move || {
        let Event::LateAnswer { ack, .. } = b.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected b")
        };
        ending.end("b");
        drop(ack);
    });
    let live = custody.clone();
    let a_driver = thread::spawn(move || {
        let Event::LateAnswer { ack, entry, .. } = a.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("expected a")
        };
        assert_eq!(entry.result, Some(json!("live")));
        live.remove("provider", "a", 0);
        ack.send(()).unwrap();
    });
    assert!(
        custody
            .pull(&provider, "codemode:a", "provider", &options())
            .is_ok(),
        "another driver's dropped ack is not a provider failure"
    );
    b_driver.join().unwrap();
    a_driver.join().unwrap();
    assert_eq!(acked.load(Ordering::SeqCst), 1);
}

#[test]
fn one_provider_reuses_one_polling_thread_for_all_held_runs() {
    let custody = Arc::new(Custody::default());
    let a = target(&custody, "a");
    let b = target(&custody, "b");
    let threads = Arc::new(Mutex::new(BTreeSet::new()));
    let seen = threads.clone();
    let reads = Arc::new(AtomicUsize::new(0));
    let polls = reads.clone();
    let (ack, done) = mpsc::channel();
    let provider = Arc::new(Provider {
        reply: Box::new(move |name, _| {
            seen.lock()
                .unwrap()
                .insert(format!("{:?}", thread::current().id()));
            match name {
                "late_results" => {
                    let count = polls.fetch_add(1, Ordering::SeqCst);
                    Ok(if count < 2 {
                        page(vec![])
                    } else {
                        page(vec![entry("a", "a"), entry("b", "b")])
                    })
                }
                "late_results.ack" => {
                    ack.send(()).unwrap();
                    Ok(json!({}))
                }
                _ => panic!("unexpected op"),
            }
        }),
    });
    let mut drivers = Vec::new();
    for (run, receiver) in [("a", a), ("b", b)] {
        let custody = custody.clone();
        drivers.push(thread::spawn(move || {
            loop {
                if let Event::LateAnswer { ack, .. } =
                    receiver.recv_timeout(Duration::from_secs(5)).unwrap()
                {
                    custody.remove("provider", run, 0);
                    ack.send(()).unwrap();
                    break;
                }
            }
        }));
    }
    custody.ensure_worker("provider", provider.clone()).unwrap();
    custody.ensure_worker("provider", provider).unwrap();
    done.recv_timeout(Duration::from_secs(5)).unwrap();
    for driver in drivers {
        driver.join().unwrap();
    }
    custody.shutdown();
    assert_eq!(reads.load(Ordering::SeqCst), 3);
    assert_eq!(
        threads.lock().unwrap().len(),
        1,
        "the same worker performs every poll and ack for both runs"
    );
}

#[test]
fn an_awake_provider_worker_with_no_held_targets_makes_zero_requests() {
    let custody = Arc::new(Custody::default());
    let _receiver = target(&custody, "a");
    custody.mark_held("provider", "a", 0, false);
    let reads = Arc::new(AtomicUsize::new(0));
    let count = reads.clone();
    let (traffic, observed) = mpsc::channel();
    let provider = Arc::new(Provider {
        reply: Box::new(move |name, _| {
            count.fetch_add(1, Ordering::SeqCst);
            let _ = traffic.send(());
            assert_eq!(name, "late_results");
            Ok(page(vec![]))
        }),
    });
    custody.ensure_worker("provider", provider).unwrap();
    let _ = observed.recv_timeout(Duration::from_millis(100));
    custody.shutdown();
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "awake is not permission to poll without a held target"
    );
}
