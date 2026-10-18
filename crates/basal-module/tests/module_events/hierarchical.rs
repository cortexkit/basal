use super::*;
use basal_module::events::decode;
use std::collections::BTreeMap;

#[tokio::test]
async fn hierarchical_names_install_decode_admit_and_dedupe_by_full_subject() {
    let server = Server::start().await;
    let catalog = MockCatalog::standard();
    let cases = [
        (
            "started",
            "run.started",
            "ck.account.event.broca.run.started.v1",
        ),
        (
            "step",
            "run.task.step.started",
            "ck.account.event.broca.run.task.step.started.v1",
        ),
    ];
    for (_, name, _) in cases {
        catalog.set_event(
            "broca",
            name,
            1,
            basal_host::EventDecl {
                headers: vec!["repo".into()],
                origin: basal_host::EventOrigin::External,
                body: basal_host::EventBody::Inline,
            },
        );
    }
    let rt = runtime_with_catalog(&server.dir.join("hierarchical.db"), catalog);
    for (id, name, _) in cases {
        let installed = rt
            .install(&InstallRequest {
                script: "return trigger.event.body;".into(),
                manifest: json!({"id":id,"version":1,"purpose":"Receive hierarchical events",
                "trigger":{"events":[{"module":"broca","name":name,"version":1,
                    "match":{"repo":"cortexkit/*"}}]}})
                .to_string(),
                author: "local:unverified".into(),
                loop_override: false,
            })
            .unwrap();
        rt.approve(id, 1, &installed.code_hash, "test").unwrap();
    }
    for (n, (_, _, subject)) in cases.iter().enumerate() {
        server
            .publish_with_headers(
                n as u64,
                subject,
                BTreeMap::from([("repo".into(), "cortexkit/basal".into())]),
            )
            .await;
    }
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 2);
    for (message, (flow, name, subject)) in messages.iter().zip(cases) {
        let notice = decode(message, rt.catalog()).unwrap();
        assert_eq!(notice.identity().unwrap(), ("broca", name, 1));
        let report = commit_batch(rt.clone(), std::slice::from_ref(message), false)
            .await
            .unwrap();
        assert_eq!((report.matched, report.admitted), (1, 1));
        let receipt_flow: String = rt
            .store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT flow_id FROM event_receipts WHERE subject=?1",
                    [subject],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(receipt_flow, flow);
    }
    assert_eq!(run_count(&rt), 2);
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
