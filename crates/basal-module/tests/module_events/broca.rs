//! Broca's `run.ended` on a real JetStream: the reader commits a wake for
//! the pending model call it names before acknowledging it, acknowledges
//! every other Broca event without recording anything, and a redelivery
//! after a commit whose acknowledgement was lost adds nothing.
use super::*;
use serde_json::Value;

// Broca's captured run.ended (test-vectors/broca-run-events), published
// with its own event key, digest and headers.
const ENDED_BUS: &str =
    include_str!("../../../../test-vectors/broca-run-events/run_ended.bus.json");
const ENDED_SUBJECT: &str = "ck.account.event.broca.run.ended.v1";

fn wait_for(rt: &Runtime, send_id: &str, run_id: &str) {
    rt.store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO broca_waits (send_id, broca_run_id, check_at) VALUES (?1, ?2, 0)",
                [send_id, run_id],
            )?;
            Ok(())
        })
        .unwrap();
}

fn wakes(rt: &Runtime) -> Vec<(String, String, String)> {
    rt.store()
        .read(|c| {
            Ok(
                c.prepare("SELECT send_id, event_key, kind FROM broca_wakes ORDER BY send_id")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<rusqlite::Result<_>>()?,
            )
        })
        .unwrap()
}

#[tokio::test]
async fn broca_run_ended_commits_a_wake_before_its_ack_and_other_events_are_only_acked() {
    let server = Server::start().await;
    let path = server.dir.join("basal.db");
    let rt = runtime(&path);
    let bus: Value = serde_json::from_str(ENDED_BUS).unwrap();
    let run_id = bus["headers"]["run_id"].as_str().unwrap();
    let event_key = bus["message_id"].as_str().unwrap();
    wait_for(&rt, "send-1", run_id);
    wait_for(&rt, "send-2", "run-other");
    let headers: cortexkit_bus_trait::Headers = bus["headers"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect();
    let stream = server.bus.stream(STREAM);
    let receipt = stream
        .publish(
            ENDED_SUBJECT,
            event_key,
            bus["digest_sha256_of_jcs"]
                .as_str()
                .unwrap()
                .parse::<ContentDigest>()
                .unwrap(),
            headers,
        )
        .await
        .unwrap();
    assert!(!receipt.duplicate());
    // Another session's run ending, and a progress event for the same run.
    let mut elsewhere = cortexkit_bus_trait::Headers::new();
    elsewhere.insert("run_id".into(), "run-elsewhere".into());
    server
        .publish_with_headers(1, ENDED_SUBJECT, elsewhere)
        .await;
    let mut progress = cortexkit_bus_trait::Headers::new();
    progress.insert("run_id".into(), run_id.into());
    server
        .publish_with_headers(2, "ck.account.event.broca.run.progress.v1", progress)
        .await;

    let consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 3);
    // Commit without acknowledging, as if the process died between them.
    let report = commit_batch(rt.clone(), &messages, false).await.unwrap();
    assert_eq!(report.model_wakes, 1);
    assert_eq!(
        wakes(&rt),
        [(
            "send-1".to_owned(),
            event_key.to_owned(),
            "ended".to_owned()
        )],
        "the wake is durable before the notice is acknowledged"
    );
    drop(messages);
    drop(consumer);
    drop(rt);

    let rt = runtime(&path);
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    // Each unacknowledged notice comes back once its own ack wait expires, so
    // on a slow runner one pull can return only part of them. Keep pulling
    // until all three are back; the deadline only stops a hang.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut redelivery = Vec::new();
    while redelivery.len() < 3 {
        assert!(
            std::time::Instant::now() < deadline,
            "only {} of 3 notices redelivered",
            redelivery.len()
        );
        redelivery.extend(pull(&consumer).await.unwrap());
    }
    assert_eq!(redelivery.len(), 3);
    assert!(redelivery[0].info().unwrap().delivered >= 2);
    commit_batch(rt.clone(), &redelivery, true).await.unwrap();
    assert_eq!(wakes(&rt).len(), 1, "a redelivery adds no second wake");
    let info = consumer.info().await.unwrap();
    assert_eq!(info.num_ack_pending, 0);
    assert_eq!(info.num_pending, 0);
}
