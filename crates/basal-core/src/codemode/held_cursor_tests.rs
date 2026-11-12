use super::*;
use cortexkit_role_tool_provider::late_results::LateResultsReply;
use std::sync::atomic::AtomicUsize;

struct Provider {
    page: Value,
    settled: Arc<AtomicUsize>,
    reads: AtomicUsize,
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
        match name {
            "late_results" => {
                assert_eq!(arguments["since"], Value::Null);
                self.reads.fetch_add(1, Ordering::SeqCst);
                Ok(self.page.clone())
            }
            "late_results.ack" => {
                assert_eq!(
                    self.settled.load(Ordering::SeqCst),
                    2,
                    "both runs must take custody before ack"
                );
                Ok(json!({}))
            }
            _ => panic!("unexpected role request"),
        }
    }
    fn catalog(&self) -> std::result::Result<Value, WireError> {
        panic!("no catalog")
    }
    fn management(&self, _: &str, _: &str, _: Value) -> std::result::Result<Value, WireError> {
        panic!("no management")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> std::result::Result<Value, WireError> {
        panic!("no tool send")
    }
}

#[test]
fn custodian_page_reaches_both_runs_before_ack_and_rechecks_held_eligibility() {
    let custody = Custody::default();
    let settled = Arc::new(AtomicUsize::new(0));
    let mut entries = Vec::new();
    let mut receivers = Vec::new();
    for run in ["a", "b"] {
        let (events, receiver) = mpsc::channel();
        let call = HostCall {
            position: 0,
            kind: CallKind::Tool {
                name: "browser_use".into(),
            },
            args: JsonText::null(),
        };
        let scope = ScopeIdentity {
            owner: "reserved:core".into(),
            scope_ref: run.into(),
            scope_epoch: 7,
        };
        custody.register(
            "provider",
            Target {
                run: run.into(),
                scope,
                call,
                events,
                held: false,
                options: basal_host::transport::RunToolOptions {
                    identity: subc_protocol::BindIdentity::new("/", "basal", "test"),
                    reply_timeout: Duration::from_secs(1),
                    preset: None,
                },
            },
        );
        custody.mark_held("provider", run, 0, true);
        entries.push(
            LateEntry::new(
                "result",
                "reserved:core",
                run,
                7,
                "reserved:basal",
                store::call_key(run, 0),
                format!("e-{run}"),
                100,
            )
            .with_result(json!(run)),
        );
        let settled = settled.clone();
        receivers.push(thread::spawn(move || {
            let Event::LateAnswer { entry, ack, .. } =
                receiver.recv_timeout(Duration::from_secs(5)).unwrap()
            else {
                panic!("expected final entry")
            };
            assert_eq!(entry.unwrap().result, Some(json!(run)));
            settled.fetch_add(1, Ordering::SeqCst);
            ack.send(()).unwrap();
        }));
    }
    let provider = Provider {
        page: serde_json::to_value(LateResultsReply {
            entries,
            cursor: Cursor {
                provider_incarnation: "i".into(),
                seq: 2,
            },
            more: false,
        })
        .unwrap(),
        settled,
        reads: AtomicUsize::new(0),
    };
    let options = basal_host::transport::RunToolOptions {
        identity: subc_protocol::BindIdentity::new("/", "basal", "test"),
        reply_timeout: Duration::from_secs(1),
        preset: None,
    };
    assert!(
        !custody
            .pull(&provider, "codemode:a", "provider", &options)
            .unwrap()
    );
    for receiver in receivers {
        receiver.join().unwrap();
    }
    custody.remove("provider", "a", 0);
    assert!(!custody.has_held("codemode:a", "provider"));
    assert!(custody.has_held("codemode:b", "provider"));
    // A pull scheduled by the first run before its final entry arrived must
    // recheck eligibility after obtaining the shared cursor.
    assert!(
        !custody
            .pull(&provider, "codemode:a", "provider", &options)
            .unwrap()
    );
    assert_eq!(provider.reads.load(Ordering::SeqCst), 1);
    custody.end("b");
    assert!(!custody.has_held("codemode:b", "provider"));
}
