use super::{STREAM, Server, bind, common, pull, runtime_with_catalog};
use basal_core::Runtime;
use basal_module::{caller::Caller, events::publish_outbox};
use cortexkit_bus_trait::{ContentDigest, Stream};
use serde_json::{Value, json};

const OWNER: &str = "agent_47120287c700722b";

fn register(rt: &Runtime, schedule: Option<Value>, script: &str) {
    let mut m = common::events_manifest("watch");
    m["params"] = json!({"note":{"type":"string","max_bytes":4096}});
    m["template"] = json!({"max_per_agent":4,"max_lifetime":"1d"});
    m["sinks"][0]["agent"] = json!("$self");
    if let Some(schedule) = schedule {
        m["params"]["time"] = json!({"type":"timestamp","max_ahead":"1h"});
        m["trigger"] = json!({"schedule":schedule});
    }
    rt.register_package(script, &m.to_string()).unwrap();
}
fn create(rt: &Runtime, values: Value, lifetime: &str) -> String {
    rt.instantiate("watch", 1, OWNER, &values, lifetime)
        .unwrap()["flow_id"]
        .as_str()
        .unwrap()
        .into()
}
fn runtime(path: &std::path::Path) -> Runtime {
    let catalog = basal_host::MockCatalog::standard();
    catalog.add_named_agent(OWNER, "Owner");
    runtime_with_catalog(path, catalog)
}

#[tokio::test]
async fn committed_outbox_survives_crash_before_publish() {
    let server = Server::start().await;
    let path = server.dir.join("outbox.db");
    let rt = runtime(&path);
    register(&rt, None, "return 1;");
    let id = create(&rt, json!({"note":"durable"}), "1h");
    assert_eq!(rt.pending_template_events(32).unwrap().len(), 1);
    drop(rt);
    let rt = runtime(&path);
    assert_eq!(rt.live_instances().unwrap()["instances"][0]["flow_id"], id);
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        1
    );
    assert!(rt.pending_template_events(32).unwrap().is_empty());
    let consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0]
            .headers
            .as_ref()
            .unwrap()
            .get_last("Nats-Msg-Id")
            .unwrap()
            .as_str(),
        format!("{id}:1")
    );
}

#[tokio::test]
async fn duplicate_publish_receipt_settles_outbox_after_ack_crash() {
    let server = Server::start().await;
    let path = server.dir.join("duplicate.db");
    let rt = runtime(&path);
    register(&rt, None, "return 1;");
    create(&rt, json!({"note":"duplicate"}), "1h");
    let event = rt.pending_template_events(32).unwrap().pop().unwrap();
    let subject = format!("ck.account.event.basal.{}.v1", event.event_name);
    let receipt = server
        .bus
        .stream(STREAM)
        .publish(
            &subject,
            &event.event_key(),
            ContentDigest::of_bytes(event.body.as_bytes()),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(!receipt.duplicate());
    // The server acknowledged, but a crash prevented the local published mark.
    drop(rt);
    let rt = runtime(&path);
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        1
    );
    assert!(rt.pending_template_events(32).unwrap().is_empty());
    let stream = async_nats::jetstream::new(server.bus.client())
        .get_stream(STREAM)
        .await
        .unwrap();
    assert_eq!(stream.cached_info().state.messages, 1);
    assert_eq!(
        rt.template_event_body(&event.event_key()).unwrap().unwrap()["published_at"],
        1000
    );
}

#[tokio::test]
async fn unavailable_bus_retries_with_durable_backoff_and_loses_nothing() {
    let server = Server::start().await;
    let path = server.dir.join("retry.db");
    let rt = runtime(&path);
    register(&rt, None, "return 1;");
    let id = create(&rt, json!({"note":"retry"}), "1h");
    let js = async_nats::jetstream::new(server.bus.client());
    js.delete_stream(STREAM).await.unwrap();
    assert!(publish_outbox(&rt, &server.bus, "account").await.is_err());
    assert!(rt.pending_template_events(32).unwrap().is_empty());
    drop(rt);
    let rt = runtime(&path);
    rt.config().clock.set(1249);
    assert!(rt.pending_template_events(32).unwrap().is_empty());
    rt.config().clock.set(1250);
    assert_eq!(
        rt.pending_template_events(32).unwrap()[0].event_key(),
        format!("{id}:1")
    );
    assert!(publish_outbox(&rt, &server.bus, "account").await.is_err());
    rt.config().clock.set(1749);
    assert!(rt.pending_template_events(32).unwrap().is_empty());
    rt.config().clock.set(1750);
    js.create_stream(async_nats::jetstream::stream::Config {
        name: STREAM.into(),
        subjects: vec!["ck.account.event.>".into()],
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        1
    );
    assert!(rt.pending_template_events(32).unwrap().is_empty());
}

#[tokio::test]
async fn self_end_and_one_shot_publish_ended() {
    let server = Server::start().await;
    let f = common::fixture("lifecycle-self-end", common::Options::with_worker());
    f.catalog.add_named_agent(OWNER, "Owner");
    register(&f.module.rt, None, "await instance.end(); return true;");
    let id = create(&f.module.rt, json!({"note":"self-end"}), "1h");
    f.module
        .rt
        .admit_trigger(&id, "fire", basal_proto::JsonText::null())
        .unwrap();
    f.module.engine.run_until_idle(100).unwrap();
    assert!(
        f.module.rt.live_instances().unwrap()["instances"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        publish_outbox(&f.module.rt, &server.bus, "account")
            .await
            .unwrap(),
        2
    );
    let g = common::fixture("lifecycle-one-shot", common::Options::with_worker());
    g.catalog.add_named_agent(OWNER, "Owner");
    register(
        &g.module.rt,
        Some(json!({"at":"$params.time"})),
        "throw new Error('one-shot failed');",
    );
    create(
        &g.module.rt,
        json!({"note":"one-shot","time":"2026-05-01T00:00:01Z"}),
        "1h",
    );
    g.clock.set(common::T0 + 1000);
    g.module.engine.run_until_idle(100).unwrap();
    assert!(
        g.module.rt.live_instances().unwrap()["instances"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        publish_outbox(&g.module.rt, &server.bus, "account")
            .await
            .unwrap(),
        2
    );
    let messages = pull(&bind(&server.bus, STREAM).await.unwrap())
        .await
        .unwrap();
    let ended = messages
        .iter()
        .filter(|m| m.subject.ends_with(".flow_instance_ended.v1"))
        .collect::<Vec<_>>();
    assert_eq!(ended.len(), 2);
    for message in ended {
        assert_eq!(
            message
                .headers
                .as_ref()
                .unwrap()
                .get_last("seq")
                .unwrap()
                .as_str(),
            "2"
        );
    }
}

#[tokio::test]
async fn captured_lifecycle_notices_bodies_and_snapshot_match_vectors() {
    let server = Server::start().await;
    let f = common::fixture("lifecycle-vectors", common::Options::default());
    f.catalog.add_named_agent(OWNER, "Owner");
    register(&f.module.rt, None, "return 1;");
    let id = create(&f.module.rt, json!({"note":"watch Δ"}), "1m");
    create(&f.module.rt, json!({"note":"watch Δ"}), "2m");
    f.clock.set(common::T0 + 120_000);
    f.module.rt.scheduler().unwrap().tick().unwrap();
    let live_request = json!({});
    let live_reply = f
        .module
        .handle(&Caller::Core, "flow.instance.live", live_request.clone())
        .unwrap();
    f.module.rt.end_instance(&id).unwrap();
    assert_eq!(
        publish_outbox(&f.module.rt, &server.bus, "account")
            .await
            .unwrap(),
        4
    );
    let consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 4);
    let mut captures = Vec::new();
    for message in messages {
        assert!(message.payload.is_empty());
        let notice = basal_module::events::decode(&message, f.module.rt.catalog()).unwrap();
        let event_name = message.subject.split('.').nth(4).unwrap();
        let request = json!({"event_key":notice.event_key,"event_name":event_name});
        let reply = f
            .module
            .handle(&Caller::Core, "events_get", request.clone())
            .unwrap();
        let mut versioned = request.clone();
        versioned["version"] = json!(1);
        assert_eq!(
            f.module
                .handle(&Caller::Core, "events_get", versioned.clone())
                .unwrap(),
            reply
        );
        versioned["version"] = json!(2);
        assert_eq!(
            f.module
                .handle(&Caller::Core, "events_get", versioned)
                .unwrap_err()
                .code,
            "event_not_visible"
        );
        assert_eq!(notice.digest, reply["digest"].as_str().unwrap());
        let body = reply["body"].as_str().unwrap();
        assert_eq!(
            notice.digest,
            ContentDigest::of_bytes(body.as_bytes()).to_string()
        );
        let headers = message
            .headers
            .as_ref()
            .unwrap()
            .iter()
            .map(|(name, values)| (name.to_string(), values.last().unwrap().to_string()))
            .collect::<std::collections::BTreeMap<_, _>>();
        captures.push(json!({"subject":message.subject.as_str(),"headers":headers,"payload":"","events_get_request":request.to_string(),"events_get_reply":reply.to_string()}));
        message.double_ack().await.unwrap();
    }
    let capture = json!({"events":captures,"live":{"request":live_request.to_string(),"reply":live_reply.to_string()}});
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/templates/lifecycle.json");
    let bytes = format!("{}\n", serde_json::to_string_pretty(&capture).unwrap());
    if std::env::var_os("BASAL_UPDATE_TEMPLATE_VECTORS").is_some() {
        std::fs::write(&path, &bytes).unwrap();
        panic!("vectors rewritten, rerun without the variable");
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), bytes);
}

#[test]
fn body_lookup_hides_unknown_keys_and_non_core_callers_identically() {
    let f = common::fixture("lifecycle-body-auth", common::Options::default());
    f.catalog.add_named_agent(OWNER, "Owner");
    register(&f.module.rt, None, "return 1;");
    let id = create(&f.module.rt, json!({"note":"private"}), "1h");
    let request = json!({"event_key":format!("{id}:1"),"event_name":"flow_instance_created"});
    let unknown = f
        .module
        .handle(
            &Caller::Core,
            "events_get",
            json!({"event_key":"unknown:1","event_name":"flow_instance_created"}),
        )
        .unwrap_err();
    assert_eq!(unknown.code, "event_not_visible");
    assert_eq!(unknown.message, "event not visible");
    for caller in [
        Caller::Operator,
        Caller::Local,
        common::agent(OWNER),
        Caller::Other("module".into()),
    ] {
        assert_eq!(
            f.module
                .handle(&caller, "events_get", request.clone())
                .unwrap_err(),
            unknown
        );
        assert_eq!(
            f.module
                .handle(
                    &caller,
                    "events_get",
                    json!({"event_key":format!("{id}:1"),"event_name":"flow_instance_ended"})
                )
                .unwrap_err(),
            unknown
        );
    }
    let reply = f
        .module
        .handle(&Caller::Core, "events_get", request)
        .unwrap();
    assert_eq!(reply["published_at"], Value::Null);
    assert_eq!(
        serde_json::from_str::<Value>(reply["body"].as_str().unwrap()).unwrap()["agent_id"],
        OWNER
    );
}

#[test]
fn core_body_lookup_reports_stored_event_name_mismatch() {
    let f = common::fixture("lifecycle-event-name", common::Options::default());
    f.catalog.add_named_agent(OWNER, "Owner");
    register(&f.module.rt, None, "return 1;");
    let id = create(&f.module.rt, json!({"note":"name check"}), "1h");
    let error = f
        .module
        .handle(
            &Caller::Core,
            "events_get",
            json!({"event_key":format!("{id}:1"),"event_name":"flow_instance_ended"}),
        )
        .unwrap_err();
    assert_eq!(error.code, "event_name_mismatch");
    assert_eq!(
        error.message,
        "event_name mismatch: expected flow_instance_created, got flow_instance_ended"
    );
}

#[tokio::test]
async fn body_retention_starts_at_ack_and_never_prunes_unpublished_rows() {
    let server = Server::start().await;
    let rt = runtime(&server.dir.join("retention.db"));
    register(&rt, None, "return 1;");
    let id = create(&rt, json!({"note":"retained"}), "1h");
    let event_key = format!("{id}:1");
    rt.config()
        .clock
        .set(1000 + basal_core::template_events::BODY_RETENTION_MS + 1);
    rt.scheduler().unwrap().tick().unwrap();
    assert!(rt.template_event_body(&event_key).unwrap().is_some());
    assert_eq!(
        publish_outbox(&rt, &server.bus, "account").await.unwrap(),
        2
    );
    let published = rt.config().clock.now_ms();
    rt.config()
        .clock
        .set(published + basal_core::template_events::BODY_RETENTION_MS);
    rt.scheduler().unwrap().tick().unwrap();
    assert!(rt.template_event_body(&event_key).unwrap().is_some());
    rt.config()
        .clock
        .set(published + basal_core::template_events::BODY_RETENTION_MS + 1);
    rt.scheduler().unwrap().tick().unwrap();
    assert!(rt.template_event_body(&event_key).unwrap().is_none());
}

#[test]
fn manifest_declares_all_lifecycle_events_and_query_operations() {
    let m = serde_json::to_value(basal_module::manifest::manifest()).unwrap();
    let events = m["events"].as_array().unwrap();
    assert_eq!(events.len(), 4);
    for name in [
        "flow_instance_created",
        "flow_instance_extended",
        "flow_instance_ended",
        "flow_instance_expired",
    ] {
        let event = events.iter().find(|e| e["name"] == name).unwrap();
        assert_eq!(event["version"], 1);
        assert_eq!(event["headers"], json!(["flow_id", "agent", "seq"]));
    }
    for name in ["events_get", "flow.instance.live"] {
        let op = basal_module::manifest::OPERATIONS
            .iter()
            .find(|op| op.0 == name)
            .unwrap();
        assert_eq!(
            op.1,
            subc_protocol::manifest::ManagementOperationKind::Query
        );
    }
}
