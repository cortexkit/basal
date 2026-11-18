//! Captures the real agent-data service's responses and ERROR frames.

use std::fs;
use std::path::Path;
use std::time::Duration;

use basal_core::cards::Decision;
use cortexkit_role_agent_data::Op;
use cortexkit_role_agent_data::errors::{ErrorBody, RefusalCode, allowed_refusals};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subc_client_rs::test_support::{BindIdentity, FrameType, ModuleHarness, RouteTarget};
use subc_client_rs::{RouteBindRequest, RouteHandle};
use subc_protocol::Principal;

use super::{AGENT, BACK, CUT, Computer, FakeDoor, REF_KIND, seal};
use crate::serve::BasalHandler;

const EMPTY_CUT: &str = "dddddddddddddddddddddddddddddddd";
const REFUSAL_CUT: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const FLOW: &str = "f-vector";

struct Service {
    harness: ModuleHarness,
    route: RouteHandle,
    exchanges: Vec<Value>,
}

impl Service {
    async fn start(computer: &Computer) -> Self {
        let handler = BasalHandler::ready_test(computer.module.clone());
        let mut harness = ModuleHarness::start(|_| handler, []).await;
        let route = Self::bind(&mut harness, 1, "entorhinal").await;
        Self {
            harness,
            route,
            exchanges: Vec::new(),
        }
    }

    async fn bind(harness: &mut ModuleHarness, channel: u16, principal: &str) -> RouteHandle {
        harness
            .bind_route(
                RouteBindRequest::new(
                    RouteHandle::detached(channel, 1),
                    RouteTarget::InternalService {
                        module_id: "basal".into(),
                        service_id: cortexkit_role_agent_data::SERVICE_ID.into(),
                    },
                    BindIdentity::new("test", "agent-data-vectors"),
                )
                .with_principal(Principal::Reserved {
                    module_id: principal.into(),
                }),
            )
            .await
    }

    async fn exchange(
        &mut self,
        route: RouteHandle,
        name: &str,
        op: Op,
        cut: &str,
    ) -> Result<Value, ErrorBody> {
        let refs = json!([{"ref_kind": REF_KIND, "ref": AGENT}]);
        let params = match op {
            Op::Describe => json!({}),
            Op::Busy => json!({"refs": refs}),
            Op::Release => json!({"refs": refs, "cutId": cut, "holder": "holder-test-receiver"}),
            _ => json!({"refs": refs, "cutId": cut}),
        };
        let request = json!({"method": op.method(), "params": params});
        let corr = self
            .harness
            .send_request(route, serde_json::to_vec(&request).unwrap())
            .await;
        let frame = self.harness.read_reply(route, corr).await;
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        match frame.header.ty {
            FrameType::Response => {
                let result = &body["result"];
                assert!(result.is_object(), "{name}: missing result object: {body}");
                self.exchanges
                    .push(json!({"name": name, "request": request, "reply": body}));
                Ok(result.clone())
            }
            FrameType::Error => {
                let error: ErrorBody = serde_json::from_value(body.clone()).unwrap();
                assert!(
                    allowed_refusals(op).contains(&error.code),
                    "{name}: {} is not allowed for {}",
                    error.code.as_str(),
                    op.method(),
                );
                error.typed_detail().unwrap();
                self.exchanges
                    .push(json!({"name": name, "request": request, "error": body}));
                Err(error)
            }
            ty => panic!("{name}: expected RESPONSE or ERROR, got {ty:?}"),
        }
    }

    async fn ok(&mut self, name: &str, op: Op, cut: &str) -> Value {
        self.exchange(self.route, name, op, cut)
            .await
            .unwrap_or_else(|error| panic!("{name}: {error:?}"))
    }

    async fn finish(mut self) -> Vec<Value> {
        self.harness.shutdown().await;
        self.exchanges
    }
}

fn live_run(computer: &Computer) {
    computer.exec(
        "INSERT INTO run_snapshots(flow_id,flow_version,code_hash,script,manifest)
           SELECT flow_id,version,code_hash,script,manifest FROM installs WHERE flow_id='f-vector';
         INSERT INTO live_runs(run_id,flow_id,flow_version,snapshot_id,trigger_id,trigger_kind,trigger,attempt,admitted_at,admit_seq,admit_epoch,admit_segment,admit_offset,deadline_ms,self_input,params_input,grant_guards,end_instance)
           SELECT 'run-vector',flow_id,flow_version,snapshot_id,'t-vector','schedule','{}',1,20,2,0,0,0,600000,'null','null','null',0 FROM run_snapshots WHERE flow_id='f-vector';",
    );
}

fn assert_busy(error: ErrorBody, reason: &str) {
    assert_eq!(error.code, RefusalCode::Busy);
    assert_eq!(error.detail.unwrap()["refs"][0]["reason_code"], reason);
}

async fn flow_move() -> (Vec<Value>, Vec<Value>) {
    let door = FakeDoor::new();
    let source = Computer::new("vectors-flows-source", &door);
    let receiver = Computer::new("vectors-flows-receiver", &door);
    source.flow(FLOW);
    source.exec(
        "INSERT INTO event_receipts(flow_id,subject,event_key,notice,state,received_at)
           VALUES ('f-vector','synthetic.event','event-vector','{}','backlog',5)",
    );
    let mut sender = Service::start(&source).await;
    let mut target = Service::start(&receiver).await;

    sender.ok("describe", Op::Describe, CUT).await;
    assert_eq!(
        sender.ok("busy-with-flows", Op::Busy, CUT).await,
        json!({"refs": [{"state": "idle"}]}),
    );
    let prepared = sender.ok("prepare-with-flows", Op::Prepare, CUT).await;
    assert_eq!(prepared["items"].as_array().unwrap().len(), 1);
    sender.ok("export-with-flows", Op::Export, CUT).await;
    seal(&door, CUT, &prepared);

    assert_eq!(
        target.ok("diverged-at-cut", Op::Diverged, CUT).await,
        json!({"refs": [{"state": "at_cut"}]}),
    );
    let imported = target.ok("import-with-drops", Op::Import, CUT).await;
    assert_eq!(imported["items"][0]["dropped"], json!({"queued_event": 1}));
    target.ok("activate-with-flows", Op::Activate, CUT).await;
    assert_eq!(
        sender.ok("release-enforced", Op::Release, CUT).await,
        json!({"enforced": true}),
    );
    (sender.finish().await, target.finish().await)
}

async fn empty_move() -> (Vec<Value>, Vec<Value>) {
    let door = FakeDoor::new();
    let source = Computer::new("vectors-empty-source", &door);
    let receiver = Computer::new("vectors-empty-receiver", &door);
    let mut sender = Service::start(&source).await;
    let mut target = Service::start(&receiver).await;

    assert_eq!(
        sender.ok("busy-without-flows", Op::Busy, EMPTY_CUT).await,
        json!({"refs": [{"state": "idle"}]}),
    );
    let prepared = sender.ok("prepare-empty", Op::Prepare, EMPTY_CUT).await;
    assert_eq!(prepared, json!({"items": [], "external": []}));
    assert_eq!(
        sender.ok("export-empty", Op::Export, EMPTY_CUT).await,
        json!({"items": []}),
    );
    seal(&door, EMPTY_CUT, &prepared);

    assert_eq!(
        target.ok("import-empty", Op::Import, EMPTY_CUT).await,
        json!({"items": []}),
    );
    assert_eq!(
        target.ok("activate-empty", Op::Activate, EMPTY_CUT).await,
        json!({})
    );
    assert!(receiver.sql("SELECT flow_id FROM flows").is_empty());
    (sender.finish().await, target.finish().await)
}

async fn source_refusals() -> Vec<Value> {
    let door = FakeDoor::new();
    let source = Computer::new("vectors-source-refusals", &door);
    let mut sender = Service::start(&source).await;
    let code_hash = source.install_draft(FLOW);
    let card = source
        .module
        .rt
        .record_card(FLOW, 1, &code_hash, AGENT, "{}")
        .unwrap();
    let error = sender
        .exchange(
            sender.route,
            "prepare-approval-open",
            Op::Prepare,
            REFUSAL_CUT,
        )
        .await
        .unwrap_err();
    assert_busy(error, "approval_open");

    source
        .module
        .rt
        .decide_card(&card.card_id, Decision::Approve, "core:card")
        .unwrap();
    live_run(&source);
    let error = sender
        .exchange(sender.route, "prepare-run-active", Op::Prepare, REFUSAL_CUT)
        .await
        .unwrap_err();
    assert_busy(error, "run_active");
    source.exec("DELETE FROM live_runs WHERE run_id='run-vector'");

    sender
        .ok("prepare-before-early-release", Op::Prepare, REFUSAL_CUT)
        .await;
    let error = sender
        .exchange(
            sender.route,
            "release-before-export",
            Op::Release,
            REFUSAL_CUT,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, RefusalCode::InvalidState);
    assert_eq!(error.detail.unwrap(), json!({"step": "prepared"}));

    let other_route = Service::bind(&mut sender.harness, 2, "broca").await;
    let error = sender
        .exchange(
            other_route,
            "describe-wrong-caller",
            Op::Describe,
            REFUSAL_CUT,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, RefusalCode::CallerForbidden);
    sender
        .finish()
        .await
        .into_iter()
        .filter(|case| case.get("error").is_some())
        .collect()
}

async fn clean_import_and_receiver_refusals() -> (Vec<Value>, Vec<Value>) {
    let door = FakeDoor::new();
    let source = Computer::new("vectors-clean-source", &door);
    let receiver = Computer::new("vectors-clean-receiver", &door);
    let clash = Computer::new("vectors-clash-receiver", &door);
    source.flow(FLOW);
    let mut sender = Service::start(&source).await;
    let mut target = Service::start(&receiver).await;
    let mut conflicting = Service::start(&clash).await;

    let prepared = sender.ok("prepare-clean", Op::Prepare, BACK).await;
    sender.ok("export-clean", Op::Export, BACK).await;
    seal(&door, BACK, &prepared);
    sender.finish().await;

    let imported = target.ok("import-without-drops", Op::Import, BACK).await;
    let items = imported["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].get("dropped").is_none());
    let clean = std::mem::take(&mut target.exchanges);

    // Import stages the incoming flow; a local flow can still start a run before activation.
    receiver.flow(FLOW);
    live_run(&receiver);
    let error = target
        .exchange(target.route, "activate-run-active", Op::Activate, BACK)
        .await
        .unwrap_err();
    assert_busy(error, "run_active");

    clash.exec("INSERT INTO flows(flow_id,owner,created_at) VALUES ('f-vector','agent_other',1)");
    let error = conflicting
        .exchange(conflicting.route, "import-flow-id-clash", Op::Import, BACK)
        .await
        .unwrap_err();
    assert_eq!(error.code, RefusalCode::LocalAdvanced);
    assert!(error.message.contains(FLOW) && error.message.contains("agent_other"));
    let mut refusals = target.finish().await;
    refusals.extend(conflicting.finish().await);
    (clean, refusals)
}

async fn collect_vectors() -> [(&'static str, Vec<Value>); 7] {
    let (source_flows, receiver_flows) = flow_move().await;
    let (source_empty, receiver_empty) = empty_move().await;
    let (receiver_clean, receiver_refusals) = clean_import_and_receiver_refusals().await;
    [
        ("receiver-clean.json", receiver_clean),
        ("receiver-empty.json", receiver_empty),
        ("receiver-flows.json", receiver_flows),
        ("receiver-refusals.json", receiver_refusals),
        ("source-empty.json", source_empty),
        ("source-flows.json", source_flows),
        ("source-refusals.json", source_refusals().await),
    ]
}

#[tokio::test]
async fn basal_agent_data_vectors_match_service() {
    let vectors = tokio::time::timeout(Duration::from_secs(30), collect_vectors())
        .await
        .expect("agent-data service did not finish capturing vectors");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/basal-agent-data-v1");
    let write = std::env::var("BASAL_WRITE_VECTORS").as_deref() == Ok("1");
    let check = |name, contents: &str| {
        let path = root.join(name);
        if write {
            fs::write(&path, contents).unwrap();
        }
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display())),
            contents,
            "{name} differs from the service; see the vector README for regeneration",
        );
    };

    let readme = fs::read(root.join("README.md")).unwrap();
    let mut sums = format!("{:x}  README.md\n", Sha256::digest(readme));
    for (name, exchanges) in vectors {
        let contents = format!("{}\n", serde_json::to_string_pretty(&exchanges).unwrap());
        check(name, &contents);
        sums.push_str(&format!("{:x}  {name}\n", Sha256::digest(contents)));
    }
    check("SHA256SUMS", &sums);
}
