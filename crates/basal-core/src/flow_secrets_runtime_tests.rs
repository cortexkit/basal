use crate::{
    ChannelError, Config, Durability, InstallGate, InstallRequest, NoHooks, Runtime, Store,
    WorkerChannel,
};
use basal_host::{CallClass, CallRequest, Dispatched, Host, TransportError};
use basal_proto::*;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

const KEY: &str = "secret-never-to-script";
const ENCODED: &str = "c2VjcmV0LW5ldmVyLXRvLXNjcmlwdA";

struct Carrier(AtomicUsize);
impl Host for Carrier {
    fn secret_use(
        &self,
        request: &CallRequest,
        code_hash: &str,
        _: Value,
    ) -> Result<Value, basal_host::transport::WireError> {
        assert_eq!(request.flow_version, Some(1));
        assert_eq!(code_hash.len(), 64);
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(
            json!({"status":"released","kind":"agent_api_key","key_b64url":ENCODED,"location":"body:token"}),
        )
    }
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> Result<Dispatched, TransportError> {
        panic!("no op forwarding")
    }
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.5
    }
    fn attach(&self, _: Arc<dyn basal_host::CompletionSink>) {}
}

struct Script {
    welcome: Welcome,
    activation: Option<u64>,
    issued: bool,
    awaiting: bool,
    cut_delivery: bool,
    result: Option<JsonText>,
}
impl Script {
    fn new(cut_delivery: bool) -> Self {
        Self {
            welcome: Welcome {
                protocol_version: PROTOCOL_VERSION,
                engine: "secret-scripted".into(),
                prelude_hash: PreludeHash([0; 32]),
                codemode_prelude_hash: PreludeHash([0; 32]),
                confinement: Confinement::None,
            },
            activation: None,
            issued: false,
            awaiting: false,
            cut_delivery,
            result: None,
        }
    }
}
impl WorkerChannel for Script {
    fn welcome(&self) -> &Welcome {
        &self.welcome
    }
    fn send(&mut self, message: &ParentMessage) -> Result<(), ChannelError> {
        match message {
            ParentMessage::Activate(request) => {
                self.activation = Some(request.activation_id);
                if let Some(recorded) = request.prefix.first() {
                    self.issued = true;
                    self.result = Some(
                        recorded
                            .outcome
                            .as_ref()
                            .expect("recorded delivery")
                            .value
                            .clone(),
                    );
                }
            }
            ParentMessage::Deliver(outcome) => {
                self.result = Some(outcome.value.clone());
                self.awaiting = false;
                if self.cut_delivery {
                    return Err(ChannelError::Closed);
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn recv(&mut self, _: Duration) -> Result<WorkerMessage, ChannelError> {
        let activation_id = self.activation.ok_or(ChannelError::Closed)?;
        if self.awaiting {
            return Ok(WorkerMessage::Blocked { awaiting: vec![0] });
        }
        if !self.issued {
            self.issued = true;
            self.awaiting = true;
            return Ok(WorkerMessage::HostCall(HostCall {
                position: 0,
                kind: CallKind::Primitive(Primitive::NetFetch),
                args: JsonText::new(
                    json!({"url":"https://x.test/repos/a","options":{"secret":"key"}}).to_string(),
                )
                .unwrap(),
            }));
        }
        Ok(WorkerMessage::Finished {
            activation_id,
            result: ActivationResult::Completed {
                value: self
                    .result
                    .clone()
                    .expect("a delivered or replayed outcome"),
            },
        })
    }
    fn receiver(&mut self) -> Box<dyn crate::channel::WorkerReceiver> {
        panic!("uses recv")
    }
    fn kill(&mut self) {}
}

struct Logs(Arc<Mutex<String>>);
impl tracing::field::Visit for Logs {
    fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        write!(self.0.lock().unwrap(), "{value:?}").unwrap();
    }
}
impl tracing::Subscriber for Logs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        event.record(&mut Logs(self.0.clone()));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn flow_secrets_material_stays_out_of_journal_logs_script_errors_and_replay_never_releases_again() {
    let logs = Arc::new(Mutex::new(String::new()));
    tracing::subscriber::set_global_default(Logs(logs.clone())).unwrap();
    let path = std::env::temp_dir().join(format!("basal-secret-journal-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    let store = Arc::new(Store::open(path.join("store.sqlite"), Durability::throwaway()).unwrap());
    let carrier = Arc::new(Carrier(AtomicUsize::new(0)));
    let host =
        Arc::new(basal_host::builtins::BuiltinHost::default().with_secret_host(carrier.clone()));
    let rt = Runtime::new(
        store,
        host,
        Arc::new(basal_host::MockCatalog::standard()),
        Arc::new(NoHooks),
        None,
        Config {
            install_gate: InstallGate::Off,
            ..Config::default()
        },
    );
    let manifest = json!({"id":"secret-journal","version":1,"purpose":"Journal security regression",
        "trigger":{"schedule":{"interval":"1h"}},"net":{"fetch":[{"host":"x.test","methods":["GET"]}]},
        "secrets":[{"name":"key","target":{"scheme":"https","host":"x.test","path":"/repos/a"},"methods":["GET"]}]}).to_string();
    let installed = rt
        .install(&InstallRequest {
            script: "return 1;".into(),
            manifest,
            author: "operator".into(),
            loop_override: false,
        })
        .unwrap();
    rt.approve("secret-journal", 1, &installed.code_hash, "test:card")
        .unwrap();
    let run = rt
        .admit_trigger("secret-journal", "one", JsonText::null())
        .unwrap()
        .run_id()
        .unwrap()
        .to_owned();
    let mut first = Script::new(true);
    rt.activate(&run, &mut first).unwrap();
    assert_eq!(carrier.0.load(Ordering::SeqCst), 1);
    let rows = rt.calls(&run).unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let frozen: Value = serde_json::from_str(row.request.as_ref().unwrap().as_str()).unwrap();
    assert_eq!(
        frozen["credential"],
        json!({"name":"key","value":"[secret]"})
    );
    assert_eq!(
        frozen["flow_code_hash"],
        crate::ids::hex(&installed.code_hash)
    );
    let recorded = row.outcome.as_ref().unwrap().value.clone();
    assert_eq!(
        serde_json::from_str::<Value>(recorded.as_str()).unwrap()["code"],
        "secret_location_unsupported"
    );
    let mut replay = Script::new(false);
    assert!(matches!(
        rt.activate(&run, &mut replay).unwrap(),
        crate::ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(
        carrier.0.load(Ordering::SeqCst),
        1,
        "recorded replay must not reach the vault"
    );
    assert_eq!(replay.result, Some(recorded.clone()));
    for text in [
        row.args.as_str(),
        row.request.as_ref().unwrap().as_str(),
        recorded.as_str(),
        first.result.as_ref().unwrap().as_str(),
        replay.result.as_ref().unwrap().as_str(),
        &logs.lock().unwrap(),
    ] {
        assert!(!text.contains(KEY));
        assert!(!text.contains(ENCODED));
    }
    drop(rt);
    std::fs::remove_dir_all(path).unwrap();
}
