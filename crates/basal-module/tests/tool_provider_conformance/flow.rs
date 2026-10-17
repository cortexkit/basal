use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

fn read_reply(bytes: &[u8]) -> Value {
    json!({"bytes_base64":STANDARD.encode(bytes),"size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))})
}

fn stamp() -> RouteStamp {
    let mut stamp = principals().stamp("reserved:prefrontal-core");
    let scope = stamp.scope.as_mut().unwrap();
    scope.scope_ref = "flow-read/test".into();
    scope.scope_epoch = 1;
    stamp
}

async fn call(route: &Route, arguments: Value) -> Value {
    let reply = response(route, json!({"name":"flow","arguments":arguments})).await;
    let parsed: cortexkit_role_tool_provider::result::ToolResult =
        serde_json::from_value(reply.clone()).unwrap();
    cortexkit_role_tool_provider::result::validate(&parsed).unwrap();
    assert_eq!(reply["content"][0]["type"], "text");
    assert!(reply["structuredContent"].is_object());
    reply
}

async fn refusal(route: &Route, arguments: Value) -> subc_protocol_v032::ErrorBody {
    let exchange = route
        .request(json!({"name":"flow","arguments":arguments}))
        .await
        .unwrap();
    match cortexkit_role_tool_provider_conformance::single_terminal(&exchange).unwrap() {
        ObservedFrame::Error(error) => error.clone(),
        other => panic!("expected flow refusal, got {other:?}"),
    }
}

async fn content_route(subject: &Subject, live: &Live, stamp: &RouteStamp) -> Route {
    subject
        .bind_route(live, stamp, None, None, Some(vec!["content/v1".into()]))
        .await
        .unwrap()
}

fn files(live: &Live) -> (String, String) {
    let script = "return {exact: 'from head'};\n".to_owned();
    let mut manifest = common::events_manifest("head-flow");
    manifest["sinks"][0]["agent"] = json!("agent");
    let manifest = format!(" {}\n", manifest);
    let mut files = live.core.files.lock().unwrap();
    files.insert("head.js".into(), Ok(read_reply(script.as_bytes())));
    files.insert("head.json".into(), Ok(read_reply(manifest.as_bytes())));
    (script, manifest)
}

fn install() -> Value {
    json!({"action":"install","script_path":"head.js","manifest_path":"head.json"})
}

async fn management(subject: &Subject, live: &Live, stamp: &RouteStamp) -> Route {
    subject
        .bind_surface(
            live,
            stamp,
            None,
            None,
            None,
            RouteTarget::ManagementSurface {
                module_id: "basal".into(),
            },
        )
        .await
        .unwrap()
}

#[test]
fn flow_catalog_has_exact_capability_tag() {
    let catalog = basal_module::tool::catalog(json!({
        "params":{"capabilities":["basal:flow.manage/v1"]}
    }))
    .unwrap();
    let flow = catalog["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "flow")
        .unwrap();
    assert_eq!(flow["capabilities"], json!(["basal:flow.manage/v1"]));
    assert_eq!(flow["result_format"], "content/v1");
    assert!(
        flow["description"]
            .as_str()
            .unwrap()
            .contains("newest approved")
    );
    let manifest = serde_json::to_value(basal_module::manifest::manifest()).unwrap();
    assert!(
        manifest["provides"][0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "flow")
    );
    assert!(
        !manifest["capabilities"]["provides"]
            .as_array()
            .unwrap()
            .contains(&json!("basal:flow.manage/v1"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_reads_exact_head_bytes_on_stamped_scope_and_deduplicates() {
    let root = common::scratch("flow-exact");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let (script, manifest) = files(&live);
    let route = content_route(&subject, &live, &stamp()).await;
    let request = json!({"name":"flow","arguments":install(),"call_key":"install-once"});
    let reply = response(&route, request.clone()).await;
    let result = &reply["structuredContent"];
    assert_eq!(result["flow_id"], "head-flow");
    let expected = basal_core::ids::hex(&basal_core::ids::code_hash(&script, &manifest));
    assert_eq!(result["code_hash"], expected);
    let text = reply["content"][0]["text"].as_str().unwrap();
    for field in ["card_id", "code_hash"] {
        assert!(text.contains(result[field].as_str().unwrap()));
    }
    assert!(text.contains("Warnings:") && text.contains("Dry run:"));
    assert_eq!(response(&route, request.clone()).await, reply);
    let reads = live.core.reads.lock().unwrap().clone();
    assert_eq!(reads.len(), 2);
    assert_eq!(
        reads.iter().map(|(_, p)| p.as_str()).collect::<Vec<_>>(),
        ["head.js", "head.json"]
    );
    for (scope, _) in reads {
        assert_eq!(scope.owner, principal("reserved:prefrontal-core"));
        assert_eq!(scope.scope_ref, "flow-read/test");
        assert_eq!(scope.scope_epoch, Some(1));
    }
    drop(route);
    drop(live);
    subject.join_stopped().await;
    let live = subject.restart(&root).await.unwrap();
    let route = content_route(&subject, &live, &stamp()).await;
    assert_eq!(response(&route, request).await, reply);
    assert!(live.core.reads.lock().unwrap().is_empty());
    drop(live);
    subject.join_stopped().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_scope_guard_refuses_without_reading() {
    let root = common::scratch("flow-scope");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let mut missing = stamp();
    missing.scope = None;
    let mut wrong_owner = stamp();
    wrong_owner.scope.as_mut().unwrap().owner = "reserved:aft".into();
    let mut wrong_ref = stamp();
    wrong_ref.scope.as_mut().unwrap().scope_ref = "agent-scope".into();
    for stamp in [missing, wrong_owner, wrong_ref] {
        let route = content_route(&subject, &live, &stamp).await;
        assert_eq!(
            refusal(&route, install()).await.code,
            "flow_read_scope_missing"
        );
    }
    assert!(live.core.reads.lock().unwrap().is_empty());
    drop(live);
    subject.join_stopped().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_digest_guard_rechecks_returned_bytes() {
    let root = common::scratch("flow-digest");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    files(&live);
    let mut corrupt = read_reply(b"return 'tampered';");
    corrupt["sha256"] = json!("0".repeat(64));
    live.core
        .files
        .lock()
        .unwrap()
        .insert("head.js".into(), Ok(corrupt));
    let route = content_route(&subject, &live, &stamp()).await;
    assert_eq!(
        refusal(&route, install()).await.code,
        "flow_read_digest_mismatch"
    );
    assert_eq!(live.core.reads.lock().unwrap().len(), 1);
    let list = call(&route, json!({"action":"list"})).await;
    assert_eq!(list["structuredContent"]["flows"], json!([]));
    drop(live);
    subject.join_stopped().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_passes_all_aft_refusals_through_unchanged() {
    let root = common::scratch("flow-aft-errors");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let route = content_route(&subject, &live, &stamp()).await;
    for code in [
        "file_read_exact_access_denied",
        "file_read_exact_not_found",
        "file_read_exact_not_regular",
        "file_read_exact_too_large",
        "file_read_exact_io_error",
        "file_read_exact_bad_request",
        "file_read_exact_route_not_admitted",
    ] {
        for check in [
            "principal",
            "owner",
            "kind",
            "scope_ref",
            "parent",
            "expiry",
            "operation",
        ] {
            let message = if code == "file_read_exact_route_not_admitted" {
                format!("Route not admitted: {check}")
            } else {
                format!("{code} from AFT")
            };
            let detail = json!({"check":check});
            live.core.files.lock().unwrap().insert(
                "head.js".into(),
                Err(WireError::RefusedDetails {
                    code: code.into(),
                    message: message.clone(),
                    detail: detail.clone(),
                }),
            );
            let error = refusal(&route, install()).await;
            assert_eq!(error.code, code);
            assert_eq!(error.message, message);
            assert_eq!(error.detail, Some(detail));
        }
    }
    drop(live);
    subject.join_stopped().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_enforces_script_and_manifest_limits_and_size_consistency() {
    let root = common::scratch("flow-limits");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let route = content_route(&subject, &live, &stamp()).await;
    for (path, limit) in [("head.js", 1_048_576), ("head.json", 65_536)] {
        files(&live);
        live.core
            .files
            .lock()
            .unwrap()
            .insert(path.into(), Ok(read_reply(&vec![b' '; limit + 1])));
        assert_eq!(
            refusal(&route, install()).await.code,
            "file_read_exact_too_large"
        );
        let mut lying = read_reply(&vec![b' '; limit + 1]);
        lying["size"] = json!(1);
        live.core
            .files
            .lock()
            .unwrap()
            .insert(path.into(), Ok(lying));
        assert_eq!(
            refusal(&route, install()).await.code,
            "file_read_exact_too_large"
        );
    }
    files(&live);
    let mut inconsistent = read_reply(b"return 42;");
    inconsistent["size"] = json!(1);
    live.core
        .files
        .lock()
        .unwrap()
        .insert("head.js".into(), Ok(inconsistent));
    assert_eq!(
        refusal(&route, install()).await.code,
        "flow_read_bad_response"
    );
    drop(live);
    subject.join_stopped().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flow_caller_guard_uses_stamp_not_arguments() {
    let root = common::scratch("flow-caller");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let owner = management(&subject, &live, &stamp()).await;
    let mut manifest = common::events_manifest("owned");
    manifest["sinks"][0]["agent"] = json!("agent");
    response(&owner, json!({"method":"flow.install","params":{"script":"return 1;","manifest":manifest.to_string()}})).await;
    let route = content_route(&subject, &live, &stamp()).await;
    let reply = call(&route, json!({"action":"list"})).await;
    assert_eq!(reply["structuredContent"]["flows"][0]["flow_id"], "owned");
    assert_eq!(
        refusal(&route, json!({"action":"list","agent_id":"victim"}))
            .await
            .code,
        "invalid_request"
    );
    let other = subject
        .bind_route(
            &live,
            &stamp(),
            None,
            Some(json!({"agent_id":"victim"})),
            Some(vec!["content/v1".into()]),
        )
        .await
        .unwrap();
    assert_eq!(
        call(&other, json!({"action":"list"})).await["structuredContent"]["flows"],
        json!([])
    );
    assert_eq!(
        refusal(&other, json!({"action":"disable","flow_id":"owned"}))
            .await
            .code,
        "not_permitted"
    );
    assert_eq!(
        refusal(&other, json!({"action":"enable","flow_id":"owned"}))
            .await
            .code,
        "not_permitted"
    );
    assert_eq!(
        refusal(&other, json!({"action":"end","flow_id":"owned"}))
            .await
            .code,
        "not_permitted"
    );
    assert_eq!(
        refusal(
            &other,
            json!({"action":"dry_run","flow_id":"owned","trigger":{}})
        )
        .await
        .code,
        "not_permitted"
    );
    let key = json!({"name":"flow","arguments":{"action":"list"},"call_key":"same-carrier"});
    response(&route, key.clone()).await;
    let exchange = other.request(key).await.unwrap();
    assert!(
        matches!(cortexkit_role_tool_provider_conformance::single_terminal(&exchange).unwrap(), ObservedFrame::Error(e) if e.code == "invalid_request")
    );
    drop(live);
    subject.join_stopped().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_flow_actions_reuse_ops_and_return_content_v1() {
    let root = common::scratch("flow-actions");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    files(&live);
    let route = content_route(&subject, &live, &stamp()).await;
    let installed = call(&route, install()).await;
    assert!(live.core.consent.decide(
        installed["structuredContent"]["card_id"].as_str().unwrap(),
        basal_host::CardDecision::Approve,
        "operator"
    ));
    let dry = call(
        &route,
        json!({"action":"dry_run","flow_id":"head-flow","version":1,"trigger":{"sample":true}}),
    )
    .await;
    assert_eq!(dry["structuredContent"]["flow_id"], "head-flow");
    let disabled = call(
        &route,
        json!({"action":"disable","flow_id":"head-flow","reason":"paused"}),
    )
    .await;
    assert_eq!(
        disabled["structuredContent"],
        json!({"flow_id":"head-flow","state":"disabled","changed":true})
    );
    let enabled = call(&route, json!({"action":"enable","flow_id":"head-flow"})).await;
    assert_eq!(
        enabled["structuredContent"],
        json!({"flow_id":"head-flow","state":"enabled","changed":true})
    );
    let list = call(&route, json!({"action":"list","flow_ids":["head-flow"]})).await;
    let agent_ops = management(&subject, &live, &stamp()).await;
    assert_eq!(
        list["structuredContent"],
        response(
            &agent_ops,
            json!({"method":"flow.list","params":{"flow_ids":["head-flow"]}})
        )
        .await["result"]
    );
    let text = list["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("head-flow: enabled; approved version 1; last run null"));
    let mut template = common::events_manifest("watch");
    template["sinks"][0]["agent"] = json!("$self");
    template["params"] = json!({"note":{"type":"string","max_bytes":100}});
    template["template"] = json!({"max_per_agent":2,"max_lifetime":"1d"});
    let mut operator = stamp();
    operator.scope = None;
    operator.principal = "reserved:callosum".into();
    let operator = management(&subject, &live, &operator).await;
    for version in [1, 2] {
        template["version"] = json!(version);
        response(&operator, json!({"method":"package.register","params":{"script":"return 1;","manifest":template.to_string()}})).await;
    }
    live.core.approval_capability.store(true, Ordering::SeqCst);
    live.core
        .approved
        .lock()
        .unwrap()
        .insert("watch".into(), Some(1));
    let instantiated = call(
        &route,
        json!({"action":"instantiate","template":"watch","params":{"note":"a"},"expires_in":"1h"}),
    )
    .await;
    assert_eq!(
        instantiated["structuredContent"]["version"], 1,
        "registered v2 has not been approved by core"
    );
    let id = instantiated["structuredContent"]["flow_id"]
        .as_str()
        .unwrap();
    let ended = call(&route, json!({"action":"end","flow_id":id})).await;
    assert_eq!(ended["structuredContent"]["flow_id"], id);
    live.core.approval_capability.store(false, Ordering::SeqCst);
    let explicit = call(&route, json!({"action":"instantiate","template":"watch","version":2,"params":{"note":"b"},"expires_in":"1h"})).await;
    assert_eq!(explicit["structuredContent"]["version"], 2);
    assert_eq!(*live.core.approval_queries.lock().unwrap(), ["watch"]);
    drop(live);
    subject.join_stopped().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instantiate_requires_capability_and_handles_no_approved_version() {
    let root = common::scratch("flow-version");
    let subject = Subject::default();
    let live = subject.spawn(&root).await.unwrap();
    let route = content_route(&subject, &live, &stamp()).await;
    let args = json!({"action":"instantiate","template":"unknown","params":{},"expires_in":"1h"});
    let error = refusal(&route, args.clone()).await;
    assert_eq!(error.code, "version_required");
    assert_eq!(
        error.message,
        "Pass version; this core can't report approved template versions yet"
    );
    assert!(live.core.approval_queries.lock().unwrap().is_empty());
    live.core.approval_capability.store(true, Ordering::SeqCst);
    assert_eq!(refusal(&route, args.clone()).await.code, "package_unknown");
    live.core
        .approved
        .lock()
        .unwrap()
        .insert("known-unapproved".into(), None);
    let mut known = args;
    known["template"] = json!("known-unapproved");
    assert_eq!(refusal(&route, known).await.code, "package_unknown");
    drop(live);
    subject.join_stopped().await;
    std::fs::remove_dir_all(root).unwrap();
}
