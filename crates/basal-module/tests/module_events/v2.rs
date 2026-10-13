use super::*;
use basal_module::events::decode;
use std::collections::BTreeMap;

const V2_SUBJECT: &str = "ck.account.event.plexus.pull_request_review.v2";

fn catalog() -> MockCatalog {
    let catalog = MockCatalog::standard();
    catalog.set_event(
        "plexus",
        "pull_request_review",
        2,
        basal_host::EventDecl {
            origin: basal_host::EventOrigin::External,
            body: basal_host::EventBody::Inline,
        },
    );
    catalog
}

async fn publish(server: &Server, subject: &str, id: &str, key: Option<&str>) {
    let headers = key
        .map(|key| BTreeMap::from([("event_key".into(), key.into())]))
        .unwrap_or_default();
    let receipt = server
        .bus
        .stream(STREAM)
        .publish(subject, id, ContentDigest::of_bytes(b"{}"), headers)
        .await
        .unwrap();
    assert!(!receipt.duplicate());
}

#[tokio::test]
async fn v1_and_v2_decode_using_distinct_transport_ids() {
    let server = Server::start().await;
    let catalog = catalog();
    let key = format!("{:064x}", 7);
    publish(&server, SUBJECT, &key, None).await;
    publish(&server, V2_SUBJECT, &format!("{key}.v2"), Some(&key)).await;
    let messages = pull(&bind(&server.bus, STREAM).await.unwrap())
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    let v1 = decode(&messages[0], &catalog).unwrap();
    assert_eq!(v1.event_key, key);
    assert_eq!(v1.identity().unwrap(), ("plexus", "pull_request_review", 1));
    assert!(v1.headers.is_empty());
    let v2 = decode(&messages[1], &catalog).unwrap();
    assert_eq!(v2.event_key, key);
    assert_eq!(v2.headers["event_key"], key);
    assert_eq!(v2.identity().unwrap(), ("plexus", "pull_request_review", 2));
    assert_eq!(v2.digest, ContentDigest::of_bytes(b"{}").to_string());
}

async fn malformed(subject: &str, id: &str, key: Option<&str>, reason: &str) {
    let server = Server::start().await;
    let rt = runtime_with_catalog(&server.dir.join("malformed.db"), catalog());
    approve(&rt, "v1");
    approve_version(&rt, "v2", 2);
    publish(&server, subject, id, key).await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert!(
        decode(&messages[0], rt.catalog())
            .unwrap_err()
            .contains(reason)
    );
    assert_eq!(
        commit_batch(rt.clone(), &messages, true)
            .await
            .unwrap()
            .matched,
        0
    );
    assert_eq!(run_count(&rt), 0);
    let receipts: i64 = rt
        .store()
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event_receipts", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(receipts, 0);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}

#[tokio::test]
async fn v2_missing_event_key_is_malformed_and_acked() {
    malformed(
        V2_SUBJECT,
        &format!("{:064x}.v2", 1),
        None,
        "event_key absent",
    )
    .await;
}

#[tokio::test]
async fn v2_mismatched_transport_id_is_malformed_and_acked() {
    let key = format!("{:064x}", 2);
    malformed(V2_SUBJECT, &key, Some(&key), "transport id").await;
}

#[tokio::test]
async fn undeclared_version_is_malformed_and_acked() {
    let key = format!("{:064x}", 3);
    malformed(
        "ck.account.event.plexus.pull_request_review.v3",
        &format!("{key}.v3"),
        Some(&key),
        "undeclared event version",
    )
    .await;
}

#[tokio::test]
async fn suffixless_subject_is_malformed_and_acked() {
    malformed(
        SUBJECT.trim_end_matches(".v1"),
        &format!("{:064x}", 8),
        None,
        "invalid event subject",
    )
    .await;
}

#[tokio::test]
async fn declared_v3_uses_header_key_and_refuses_mismatched_id() {
    let server = Server::start().await;
    let catalog = catalog();
    catalog.set_event(
        "plexus",
        "pull_request_review",
        3,
        basal_host::EventDecl {
            origin: basal_host::EventOrigin::External,
            body: basal_host::EventBody::Inline,
        },
    );
    let rt = runtime_with_catalog(&server.dir.join("v3.db"), catalog);
    approve_version(&rt, "v3", 3);
    let subject = "ck.account.event.plexus.pull_request_review.v3";
    let key = format!("{:064x}", 10);
    publish(&server, subject, &format!("{key}.v3"), Some(&key)).await;
    let other_key = format!("{:064x}", 11);
    publish(
        &server,
        subject,
        &format!("{other_key}.v2"),
        Some(&other_key),
    )
    .await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 2);
    let notice = decode(&messages[0], rt.catalog()).unwrap();
    assert_eq!(
        notice.identity().unwrap(),
        ("plexus", "pull_request_review", 3)
    );
    assert_eq!(notice.event_key, key);
    assert!(
        decode(&messages[1], rt.catalog())
            .unwrap_err()
            .contains("transport id")
    );
    let report = commit_batch(rt.clone(), &messages, true).await.unwrap();
    assert_eq!((report.matched, report.admitted), (1, 1));
    assert_eq!(run_count(&rt), 1);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}

fn approve_version(rt: &Runtime, id: &str, version: u32) {
    let installed = rt.install(&InstallRequest {
        script: "return 1;".into(),
        manifest: json!({"id":id,"version":1,"purpose":"Consume versioned events",
            "trigger":{"events":[{"module":"plexus","name":"pull_request_review","version":version}]}}).to_string(),
        author: "local:unverified".into(),
        loop_override: false,
    }).unwrap();
    rt.approve(id, 1, &installed.code_hash, "test").unwrap();
}

#[tokio::test]
async fn v1_and_v2_route_separately_and_dedupe_per_version() {
    let server = Server::start().await;
    let rt = runtime_with_catalog(&server.dir.join("versions.db"), catalog());
    approve_version(&rt, "v1", 1);
    approve_version(&rt, "v2", 2);
    let key = format!("{:064x}", 9);
    publish(&server, SUBJECT, &key, None).await;
    publish(&server, V2_SUBJECT, &format!("{key}.v2"), Some(&key)).await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 2);
    for (message, expected_flow) in messages.iter().zip(["v1", "v2"]) {
        let report = commit_batch(rt.clone(), std::slice::from_ref(message), false)
            .await
            .unwrap();
        assert_eq!(
            (report.matched, report.admitted, report.duplicate),
            (1, 1, 0)
        );
        let flow: String = rt
            .store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT flow_id FROM event_receipts WHERE subject=?1",
                    [message.subject.as_str()],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(flow, expected_flow);
    }
    assert_eq!(
        run_count(&rt),
        2,
        "the same bare key admits once per version"
    );
    drop(messages);
    let redelivery = pull(&consumer).await.unwrap();
    assert_eq!(redelivery.len(), 2);
    assert!(redelivery.iter().all(|m| m.info().unwrap().delivered >= 2));
    let report = commit_batch(rt.clone(), &redelivery, true).await.unwrap();
    assert_eq!(
        (report.matched, report.admitted, report.duplicate),
        (2, 0, 2)
    );
    assert_eq!(run_count(&rt), 2);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}
