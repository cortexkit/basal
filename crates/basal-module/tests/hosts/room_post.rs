use super::*;

const ROOT: &str = "../../test-vectors/flow-room-post-v1";
const CAPABILITY: &str = include_str!("../../../../test-vectors/flow-room-post-v1/capability.txt");
const ROOM: &str = "rm_fleet";

fn path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(ROOT)
        .join(name)
}

fn vector(name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(path(name)).unwrap()).unwrap()
}

/// The pinned entry with this name, read from a list of `{name, …}` objects.
fn pinned(list: &str, name: &str) -> Value {
    vector(list)
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("{name} is not pinned in {list}"))
        .clone()
}

fn published(name: &str) -> Value {
    vector("vectors.json")
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap_or_else(|| panic!("{name} is not a published vector"))
        .clone()
}

fn room_manifest() -> Value {
    let mut manifest = json!({"id":"host-flow","version":1,"purpose":"Post a daily summary",
        "audience":{"kind":"agent","id":"ag_synapse"},
        "trigger":{"schedule":{"interval":"1h"}}});
    manifest["sinks"] = vector("manifest-grant.json")["sinks"].clone();
    manifest
}

/// Core's answer to `agent.list`, restricted to the one agent the fixtures
/// know, so a mention of any other id has no stable id to resolve to.
fn only_ag_synapse(fake: &Fake) {
    let mut catalog = fake.catalog.lock().unwrap();
    catalog["agents"] = json!([{"agent_id":"ag_synapse","name":"SYNAPSE"}]);
}

/// Runs one approved flow to the end, returning its run id and what its
/// script returned.
fn run_once(f: &common::Fixture, tag: &str) -> (String, Value) {
    let run = common::admit(f, "host-flow", tag);
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded, "{:?}", done.error_detail);
    let value = serde_json::from_str(done.result.as_deref().unwrap()).unwrap();
    (run, value)
}

#[test]
fn room_post_vectors_are_byte_pinned() {
    use sha2::{Digest, Sha256};
    let sums = std::fs::read_to_string(path("SHA256SUMS")).unwrap();
    let mut names = Vec::new();
    for line in sums.lines() {
        let (hash, name) = line.split_once("  ").unwrap();
        let bytes = std::fs::read(path(name)).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), hash, "{name}");
        names.push(name.to_owned());
    }
    // The published contract first, byte for byte, then basal's additions.
    assert_eq!(
        names,
        [
            "README.md",
            "vectors.json",
            "CONTRACT.md",
            "capability.txt",
            "manifest-grant.json",
            "refusals.json",
            "replies.json",
            "requests.json",
        ]
    );
    assert_eq!(CAPABILITY.trim(), basal_host::core_host::FLOW_ROOM_POST_V1);
    // Every published case has a pinned request and reply, in the same order.
    let published: Vec<String> = vector("vectors.json")
        .as_array()
        .unwrap()
        .iter()
        .map(|case| case["name"].as_str().unwrap().to_owned())
        .collect();
    for list in ["requests.json", "replies.json", "refusals.json"] {
        let named: Vec<String> = vector(list)
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().or(entry["code"].as_str()).unwrap().to_owned())
            .collect();
        if list != "refusals.json" {
            assert_eq!(named, published, "{list}");
        }
        assert!(!named.is_empty(), "{list}");
    }
}

#[test]
fn room_post_wire_matches_every_published_vector() {
    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let host = CoreHost::new(fake.clone());
    for case in vector("vectors.json").as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let params = pinned("requests.json", name)["params"].clone();
        let reply = pinned("replies.json", name)["reply"].clone();
        let outcome = |value: Value| {
            if let Some(error) = value.get("error") {
                Err(WireError::Refused {
                    code: error["code"].as_str().unwrap().into(),
                    message: "core's words are not part of the contract".into(),
                })
            } else {
                Ok(value["result"].clone())
            }
        };
        // The storage failure is Basal's one retryable room refusal; its
        // dispatch is covered by the refusal test, so the wire test only
        // needs one answer per case to compare bytes against.
        fake.enqueue("sink.room.post", vec![outcome(reply.clone())]);
        let mut request = call(Primitive::SinkRoomPost, case["request"]["params"].clone());
        request.idempotency_key = params["receipt_key"].as_str().unwrap().into();
        let dispatched = host.dispatch(&request);
        assert_eq!(
            fake.calls("sink.room.post").last().unwrap()["params"]
                .to_string()
                .as_bytes(),
            params.to_string().as_bytes(),
            "{name}: request bytes"
        );
        if let Some(error) = reply.get("error")
            && error["code"] == "sink_store_failed"
        {
            let Err(basal_host::TransportError::Refused(refusal)) = dispatched else {
                panic!("{name}: a storage failure is retried")
            };
            assert_eq!(refusal.reason.as_str(), "sink_store_failed");
            continue;
        }
        let Dispatched::Completed(outcome) = dispatched.unwrap() else {
            panic!("{name}: completed")
        };
        match reply.get("result") {
            Some(result) => {
                assert_eq!(outcome.settlement, Settlement::Fulfilled, "{name}");
                let read: Value = serde_json::from_str(outcome.value.as_str()).unwrap();
                assert_eq!(read, *result, "{name}: reply");
            }
            None => {
                assert_eq!(outcome.settlement, Settlement::Rejected, "{name}");
                assert_eq!(
                    serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
                    reply["error"]["code"],
                    "{name}"
                );
            }
        }
    }
    assert_eq!(
        host.classify(&CallKind::Primitive(Primitive::SinkRoomPost)),
        basal_host::CallClass::Mutation {
            honours_idempotency_keys: true
        }
    );
    assert_eq!(
        basal_host::op_label(&CallKind::Primitive(Primitive::SinkRoomPost)),
        "prefrontal-core.sink.room.post"
    );
}

#[test]
fn room_post_dispatch_gate_refuses_without_the_core_capability() {
    let fake = Fake::new();
    fake.enqueue(
        "sink.room.post",
        vec![Ok(pinned("replies.json", "success")["reply"]["result"].clone())],
    );
    let host = CoreHost::new(fake.clone());
    let Dispatched::Completed(outcome) = host
        .dispatch(&call(
            Primitive::SinkRoomPost,
            published("success")["request"]["params"].clone(),
        ))
        .unwrap()
    else {
        panic!("completed")
    };
    assert_eq!(outcome.settlement, Settlement::Rejected);
    assert_eq!(
        serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
        "room_post_unsupported"
    );
    assert!(fake.calls("sink.room.post").is_empty());
}

#[test]
fn room_post_refusals_are_final_except_a_storage_failure() {
    let fake = Fake::new();
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let host = CoreHost::new(fake.clone());
    let params = published("room_post_rate_limited")["request"]["params"].clone();
    for pinned in vector("refusals.json").as_array().unwrap() {
        let code = pinned["code"].as_str().unwrap();
        fake.enqueue(
            "sink.room.post",
            vec![Err(WireError::RefusedDetails {
                code: code.into(),
                message: "no".into(),
                detail: published("room_post_rate_limited")["reply"]["error"]["detail"].clone(),
            })],
        );
        let outcome = host.dispatch(&call(Primitive::SinkRoomPost, params.clone()));
        match pinned["effect"].as_str().unwrap() {
            "retry" => {
                let Err(basal_host::TransportError::Refused(refusal)) = outcome else {
                    panic!("{code} must be retried, not settled")
                };
                assert!(!refusal.terminal());
                assert_eq!(refusal.reason.as_str(), code);
                assert_eq!(refusal.action, "sink.room.post");
            }
            "final" => {
                let Dispatched::Completed(outcome) = outcome.unwrap() else {
                    panic!("{code} must be final")
                };
                assert_eq!(outcome.settlement, Settlement::Rejected, "{code}");
                let read: Value = serde_json::from_str(outcome.value.as_str()).unwrap();
                assert_eq!(read["code"], code);
                if code == "room_post_rate_limited" {
                    assert_eq!(read["detail"]["retry_after_ms"], 3_600_000);
                }
            }
            other => panic!("unknown effect {other}"),
        }
    }
    for reply in [
        json!({}),
        json!({"room_id":"rm_fleet","seq":3,"replayed":false,"extra":1}),
        json!({"room_id":"rm_fleet","seq":0,"replayed":false}),
        json!({"room_id":"#fleet","seq":3,"replayed":false}),
        json!({"room_id":"rm_fleet","seq":3,"replayed":"no"}),
        json!({"room_id":"rm_fleet","seq":3,"replayed":false,"extra":1}),
    ] {
        fake.enqueue("sink.room.post", vec![Ok(reply)]);
        assert!(matches!(
            host.dispatch(&call(Primitive::SinkRoomPost, params.clone())),
            Err(basal_host::TransportError::Unavailable {
                sent: basal_host::Sent::Maybe(_),
                ..
            })
        ));
    }
}

#[test]
fn room_grants_decode_and_install_only_for_an_agent_audience_with_stable_ids() {
    use basal_core::manifest::Manifest;
    let manifest = room_manifest();
    let parsed = Manifest::parse(&manifest.to_string()).unwrap();
    assert_eq!(parsed.room_sinks().count(), 1);
    assert_eq!(
        parsed.room_mentions(ROOM),
        Some(&["ag_other".to_owned(), "ag_test".to_owned()][..])
    );
    assert_eq!(parsed.room_mentions("rm_other"), None);
    assert_eq!(parsed.digest_sinks().count(), 0);
    assert!(!parsed.wait_sink());
    assert_eq!(serde_json::to_value(&parsed).unwrap()["sinks"], manifest["sinks"]);

    // The grant is refused at install unless the flow is authored by exactly
    // one agent, named by its stable id.
    for (what, patch, expected) in [
        (
            "no audience",
            json!({"audience":null}),
            "needs the flow's own agent as its audience",
        ),
        (
            "global audience",
            json!({"audience":{"kind":"global"}}),
            "needs the flow's own agent as its audience",
        ),
        (
            "workspace audience",
            json!({"audience":{"kind":"workspace","id":"ws-1"}}),
            "needs the flow's own agent as its audience",
        ),
        (
            "display name",
            json!({"audience":{"kind":"agent","id":"SYNAPSE"}}),
            r#"is a name; use the agent's stable id \"ag_synapse\""#,
        ),
        (
            "unsafe room id",
            json!({"sinks":[{"kind":"room","room_id":"#fleet-notices"}]}),
            "room id",
        ),
        (
            "placeholder mention",
            json!({"sinks":[{"kind":"room","room_id":"rm_fleet","mentions":["$audience"]}]}),
            "agent id",
        ),
        (
            "duplicate room",
            json!({"sinks":[{"kind":"room","room_id":"rm_fleet"},{"kind":"room","room_id":"rm_fleet"}]}),
            "listed twice",
        ),
        (
            "unknown room field",
            json!({"sinks":[{"kind":"room","room_id":"rm_fleet","title":"Notices"}]}),
            "unknown field",
        ),
    ] {
        let fake = Fake::new();
        only_ag_synapse(&fake);
        let f = worker_fixture(
            fake.clone(),
            Arc::new(CoreConsent::new(fake.clone())),
            "room-install",
        );
        let mut m = manifest.clone();
        for (key, value) in patch.as_object().unwrap() {
            m[key] = value.clone();
        }
        let refused = format!(
            "{:?}",
            f.module
                .handle(
                    &Caller::Operator,
                    "flow.install",
                    json!({"script":"return 1;", "manifest":m.to_string()}),
                )
                .unwrap_err()
        );
        assert!(
            refused.contains(expected),
            "{what}: refusal {refused} does not mention {expected}"
        );
        assert!(
            fake.calls("flow.install_request").is_empty(),
            "{what} reached core"
        );
    }
    // A post with no room grant at all cannot be installed, and a room grant
    // needs no op line of its own.
    let mut missing = manifest.clone();
    missing["sinks"] = json!([]);
    missing["ops"] = json!([{"module":"prefrontal-core","op":"sink.room.post"}]);
    assert!(Manifest::parse(&missing.to_string()).is_err());
}

#[test]
fn room_script_sends_the_pinned_request_and_never_picks_its_own_identity() {
    let fake = Fake::new();
    only_ag_synapse(&fake);
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        "room-script",
    );
    common::install(
        &f,
        &Caller::Operator,
        "return await sink.room.post('rm_fleet', 'Boat spend: $12\\nDaily total', {mentions:['ag_other']});",
        &room_manifest(),
    );
    approve(&fake, &f.module);
    fake.enqueue(
        "sink.room.post",
        vec![Ok(pinned("replies.json", "success")["reply"]["result"].clone())],
    );
    let (run, result) = run_once(&f, "room-script");
    assert_eq!(result, pinned("replies.json", "success")["reply"]["result"]);

    let hash = fake.calls("flow.install_request")[0]["params"]["flow_install"]["code_hash"].clone();
    let row = &f.module.rt.calls(&run).unwrap()[0];
    let mut expected = published("success")["request"]["params"].clone();
    expected["stamp"]["flow_id"] = json!("host-flow");
    expected["stamp"]["code_hash"] = hash.clone();
    expected["receipt_key"] = json!(row.idempotency_key);
    let sent = fake.calls("sink.room.post");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["params"].to_string().as_bytes(),
        expected.to_string().as_bytes()
    );
    // The frozen intent carries the approved identity and leaves the receipt
    // key to the journal, so a replay reuses both.
    let mut frozen = expected.clone();
    frozen.as_object_mut().unwrap().remove("receipt_key");
    assert_eq!(row.request.as_ref().unwrap().as_str(), &frozen.to_string());
    assert_eq!(row.class, basal_core::StoredClass::KeyedMutation);
}

#[test]
fn room_script_refusals_never_reach_core() {
    let cases = [
        (
            "room_not_granted",
            "return await sink.room.post('rm_other','hi').catch(e => e.data.code);",
        ),
        (
            "room_all_mention_refused",
            "return await sink.room.post('rm_fleet','Hello @all').catch(e => e.data.code);",
        ),
        (
            "room_mention_not_granted",
            "return await sink.room.post('rm_fleet','hi',{mentions:['ag_unapproved']}).catch(e => e.data.code);",
        ),
        (
            "room_mention_not_granted",
            "return await sink.room.post('rm_fleet','hi',{mentions:['SYNAPSE']}).catch(e => e.data.code);",
        ),
        (
            "sink_item_invalid",
            "return await sink.room.post('rm_fleet','').catch(e => e.data.code);",
        ),
        (
            "sink_item_invalid",
            "return await sink.room.post('rm_fleet','line\\nbreak\\u0000').catch(e => e.data.code);",
        ),
        (
            "sink_item_invalid",
            "return await sink.room.post('rm_fleet','x'.repeat(2001)).catch(e => e.data.code);",
        ),
    ];
    for (code, script) in cases {
        let fake = Fake::new();
        only_ag_synapse(&fake);
        set_core_capability(&fake, CAPABILITY.trim(), true);
        let f = worker_fixture(
            fake.clone(),
            Arc::new(CoreConsent::new(fake.clone())),
            "room-refused",
        );
        common::install(&f, &Caller::Operator, script, &room_manifest());
        approve(&fake, &f.module);
        let (run, result) = run_once(&f, "room-refused");
        assert_eq!(result, json!(code), "{script}");
        assert!(fake.calls("sink.room.post").is_empty(), "{script}");
        let row = &f.module.rt.calls(&run).unwrap()[0];
        assert_eq!(row.attempts, 0);
        assert_eq!(row.dispatch, basal_core::DispatchState::None);
    }
}

#[test]
fn room_script_gate_and_missing_grant_are_journaled_unsent() {
    for (grant, capability, code) in [
        (true, false, "room_post_unsupported"),
        (false, true, "room_not_granted"),
    ] {
        let fake = Fake::new();
        only_ag_synapse(&fake);
        set_core_capability(&fake, CAPABILITY.trim(), capability);
        let f = worker_fixture(
            fake.clone(),
            Arc::new(CoreConsent::new(fake.clone())),
            "room-unsent",
        );
        let mut m = room_manifest();
        if !grant {
            m["sinks"] = json!([]);
        }
        common::install(
            &f,
            &Caller::Operator,
            "return await sink.room.post('rm_fleet','hi').catch(e => e.data.code);",
            &m,
        );
        approve(&fake, &f.module);
        let run = common::admit(&f, "host-flow", "room-unsent");
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(
            f.module.rt.run(&run).unwrap().result.as_deref(),
            Some(json!(code).to_string().as_str())
        );
        assert!(fake.calls("sink.room.post").is_empty());
        let row = &f.module.rt.calls(&run).unwrap()[0];
        assert_eq!(row.attempts, 0);
        assert_eq!(row.dispatch, basal_core::DispatchState::None);
    }
}

#[test]
fn room_authorization_accepts_only_the_granted_shape() {
    let manifest = basal_core::manifest::Manifest::parse(&room_manifest().to_string()).unwrap();
    let check = |args: Value| {
        basal_core::authorize::check(
            &manifest,
            &basal_core::authorize::ShellDenylist::default(),
            &SubcCatalog::new(Fake::new()),
            &CallKind::Primitive(Primitive::SinkRoomPost),
            &args,
        )
        .map_err(|refusal| refusal.code)
    };
    let allowed = json!({"room_id":"rm_fleet","text":"Boat spend: $12\nDaily total",
        "mentions":["ag_other"]});
    assert_eq!(check(allowed.clone()), Ok(()));
    let mut unmentioned = allowed.clone();
    unmentioned["mentions"] = Value::Null;
    assert_eq!(check(unmentioned), Ok(()));
    for (args, code) in [
        (
            json!({"room_id":"rm_fleet","text":"hi","mentions":null,"stamp":{"flow_id":"x"}}),
            "sink_item_invalid",
        ),
        (
            json!({"room_id":"rm_fleet","text":"hi","mentions":["ag_other"],"receipt_key":"mine"}),
            "sink_item_invalid",
        ),
        (json!({"room_id":"rm_fleet"}), "sink_item_invalid"),
        (
            json!({"room_id":"rm_fleet","text":"hi","mentions":"ag_other"}),
            "sink_item_invalid",
        ),
        (
            json!({"room_id":"rm_fleet","text":"hi","mentions":[7]}),
            "sink_item_invalid",
        ),
        (json!({"room_id":"rm_other","text":"hi"}), "room_not_granted"),
        (
            json!({"room_id":"rm_fleet","text":"@ALL hands"}),
            "room_all_mention_refused",
        ),
    ] {
        assert_eq!(check(args.clone()), Err(code.to_owned()), "{args}");
    }
}

#[test]
fn room_store_refusal_defers_and_reuses_the_frozen_receipt() {
    let fake = Fake::new();
    only_ag_synapse(&fake);
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        "room-retry",
    );
    common::install(
        &f,
        &Caller::Operator,
        "return await sink.room.post('rm_fleet','hi');",
        &room_manifest(),
    );
    approve(&fake, &f.module);
    fake.enqueue(
        "sink.room.post",
        vec![
            Err(WireError::Refused {
                code: "sink_store_failed".into(),
                message: "temporary".into(),
            }),
            Ok(pinned("replies.json", "receipt_replay")["reply"]["result"].clone()),
        ],
    );
    let run = common::admit(&f, "host-flow", "room-retry");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(
        f.module.rt.calls(&run).unwrap()[0].dispatch,
        basal_core::DispatchState::Deferred
    );
    let retry: i64 = f
        .module
        .rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT retry_not_before FROM journal WHERE run_id=?1",
                [&run],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    f.clock.set(retry);
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
    let sent = fake.calls("sink.room.post");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].to_string(), sent[1].to_string());
}

#[test]
fn room_rate_limit_is_final_and_never_resent() {
    let fake = Fake::new();
    only_ag_synapse(&fake);
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        "room-limited",
    );
    common::install(
        &f,
        &Caller::Operator,
        "return await sink.room.post('rm_fleet','hi').catch(e => [e.data.code, e.data.detail.retry_after_ms]);",
        &room_manifest(),
    );
    approve(&fake, &f.module);
    let rate_limited = published("room_post_rate_limited")["reply"]["error"].clone();
    fake.enqueue(
        "sink.room.post",
        vec![Err(WireError::RefusedDetails {
            code: rate_limited["code"].as_str().unwrap().into(),
            message: "no".into(),
            detail: rate_limited["detail"].clone(),
        })],
    );
    let (run, result) = run_once(&f, "room-limited");
    assert_eq!(result, json!(["room_post_rate_limited", 3_600_000]));
    // The refused post is final: nothing schedules it again, not even long
    // after core said the room would accept one.
    let row = &f.module.rt.calls(&run).unwrap()[0];
    assert_eq!(row.attempts, 1);
    assert_eq!(row.dispatch, basal_core::DispatchState::Sent);
    let queued = fake.replies.lock().unwrap().get("sink.room.post").cloned();
    assert!(
        queued.as_ref().is_none_or(|queue| queue.is_empty()),
        "the refusal left {queued:?} to resend"
    );
}

#[test]
fn room_dry_run_captures_the_post_and_names_the_room() {
    let fake = Fake::new();
    only_ag_synapse(&fake);
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        "room-dry",
    );
    let m = room_manifest();
    common::install(
        &f,
        &Caller::Operator,
        "await sink.room.post('rm_fleet','boat',{mentions:['ag_other']}).catch(()=>null);\n\
         await sink.room.post('rm_fleet','quiet').catch(()=>null);",
        &m,
    );
    let parsed = basal_core::manifest::Manifest::parse(&m.to_string()).unwrap();
    for mode in ["capture", "live"] {
        let result = f
            .module
            .handle(
                &Caller::Operator,
                "flow.dry_run",
                json!({"flow_id":"host-flow","mode":mode,"trigger":{"kind":"test"}}),
            )
            .unwrap();
        let summary = &result["summary"];
        let calls = summary["runs"][0]["calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        for call in calls {
            assert_eq!(call["kind"], "sink.room.post");
            assert_eq!(call["action"], "captured");
            assert_eq!(call["outcome"]["rejected"], "captured");
        }
        let text = basal_core::dry_run_text::render(&parsed, summary);
        assert_eq!(
            text.would,
            [
                "Post to the room 'rm_fleet' mentioning 'ag_other'",
                "Post to the room 'rm_fleet'"
            ]
        );
        assert!(text.problems.is_empty(), "{text:?}");
        assert!(fake.calls("sink.room.post").is_empty());
    }
}

#[test]
fn room_dry_run_follows_the_live_catalog() {
    let mut manifest = room_manifest();
    manifest["audience"] = json!({"kind":"agent","id":"ag_synapse"});
    let fake = Fake::new();
    only_ag_synapse(&fake);
    set_core_capability(&fake, CAPABILITY.trim(), true);
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        "room-dry-gate",
    );
    common::install(
        &f,
        &Caller::Operator,
        "await sink.room.post('rm_fleet','hi').catch(()=>null);",
        &manifest,
    );
    let parsed = basal_core::manifest::Manifest::parse(&manifest.to_string()).unwrap();
    // Install cards and dry runs read the catalog each time, so a core that
    // has not yet grown the capability shows the refusal the live run gives.
    for supported in [false, true, false] {
        set_core_capability(&fake, CAPABILITY.trim(), supported);
        for mode in ["capture", "live"] {
            let result = f
                .module
                .handle(
                    &Caller::Operator,
                    "flow.dry_run",
                    json!({"flow_id":"host-flow","mode":mode,"trigger":{"kind":"test"}}),
                )
                .unwrap();
            let summary = &result["summary"];
            let call = &summary["runs"][0]["calls"][0];
            assert_eq!(call["kind"], "sink.room.post");
            if supported {
                assert_eq!(call["action"], "captured", "{call:#}");
            } else {
                assert_eq!(call["action"], "refused", "{call:#}");
                assert_eq!(call["outcome"]["rejected"], "room_post_unsupported");
            }
            let text = basal_core::dry_run_text::render(&parsed, summary);
            assert_eq!(text.problems.is_empty(), supported, "{text:?}");
        }
    }
    assert!(fake.calls("sink.room.post").is_empty());
}

/// A run cut between journaling the post and recording core's answer must
/// re-send exactly the frozen bytes under the same receipt.
#[test]
fn room_journals_before_send_and_replays_the_identical_intent_after_a_cut() {
    use basal_core::{Clock, Config, Durability};
    use basal_module::{
        dryrun::DryRunConfig,
        engine::EngineConfig,
        module::{Module, ModuleConfig},
        pool::{PoolConfig, ProcessSpawner},
    };
    use basal_testkit::harness::{Point, Probe};
    for (point, sent_before_cut) in [
        ("CallCommitted { position: 0 }#1", 0),
        ("HostAnswered { position: 0 }#1", 1),
    ] {
        let dir = common::scratch("room-cut");
        let clock = Clock::manual(common::T0);
        let pool = PoolConfig::new(
            basal_testkit::channel::worker_binary(),
            basal_module::process::WorkerLaunch::Plain,
        );
        let config = ModuleConfig {
            store_path: dir.join("basal.db"),
            durability: Durability::throwaway(),
            runtime: Config {
                clock: clock.clone(),
                install_gate: basal_core::InstallGate::Off,
                ..Default::default()
            },
            pool: pool.clone(),
            engine: EngineConfig::default(),
            dry_run: DryRunConfig::new(dir.join("dry")),
        };
        let fake = Fake::new();
        only_ag_synapse(&fake);
        set_core_capability(&fake, CAPABILITY.trim(), true);
        let consent = Arc::new(CoreConsent::new(fake.clone()));
        let probe = Arc::new(Probe::crash_at(Point::parse(point).unwrap()));
        let mut dependencies = hosts(fake.clone(), consent.clone());
        dependencies.hooks = probe.clone();
        let module = Module::start(
            config.clone(),
            dependencies,
            Arc::new(ProcessSpawner::new(&pool)),
        )
        .unwrap();
        module
            .handle(
                &Caller::Operator,
                "flow.install",
                json!({
                    "script":"return await sink.room.post('rm_fleet','Boat spend: $12\\nDaily total',{mentions:['ag_other']});",
                    "manifest": room_manifest().to_string(),
                }),
            )
            .unwrap();
        approve(&fake, &module);
        fake.enqueue(
            "sink.room.post",
            vec![
                Ok(pinned("replies.json", "success")["reply"]["result"].clone()),
                Ok(pinned("replies.json", "receipt_replay")["reply"]["result"].clone()),
            ],
        );
        let run = module
            .rt
            .admit_trigger(
                "host-flow",
                "cut",
                JsonText::new(json!({"kind":"test"}).to_string()).unwrap(),
            )
            .unwrap()
            .run_id()
            .unwrap()
            .to_owned();
        assert!(module.engine.run_until_idle(50).is_err());
        module.rt.quiesce();
        module.pool.stop();
        assert!(probe.fired());
        assert_eq!(fake.calls("sink.room.post").len(), sent_before_cut);
        consent.attach(Arc::new(Decisions::default()));
        drop(module);
        clock.set(common::T0 + 10_000);
        let module = Module::start(
            config,
            hosts(fake.clone(), consent),
            Arc::new(ProcessSpawner::new(&pool)),
        )
        .unwrap();
        let row = module.rt.calls(&run).unwrap().remove(0);
        let frozen = row
            .request
            .expect("the post must be durable before transport can send");
        let value: Value = serde_json::from_str(frozen.as_str()).unwrap();
        assert_eq!(value["room_id"], ROOM);
        assert_eq!(value["text"], "Boat spend: $12\nDaily total");
        assert_eq!(value["mentions"], json!(["ag_other"]));
        assert_eq!(value["stamp"]["flow_id"], "host-flow");
        assert_eq!(value["stamp"]["flow_version"], 1);
        assert_eq!(row.class, basal_core::StoredClass::KeyedMutation);
        module.engine.run_until_idle(50).unwrap();
        let done = module.rt.run(&run).unwrap();
        assert_eq!(done.state, RunState::Succeeded);
        assert_eq!(
            serde_json::from_str::<Value>(done.result.as_ref().unwrap()).unwrap(),
            pinned("replies.json", if sent_before_cut == 0 { "success" } else { "receipt_replay" })
                ["reply"]["result"]
        );
        let sent = fake.calls("sink.room.post");
        assert_eq!(sent.len(), sent_before_cut + 1);
        let mut expected = value;
        expected["receipt_key"] = json!(row.idempotency_key);
        for call in &sent {
            assert_eq!(
                call["params"].to_string().as_bytes(),
                expected.to_string().as_bytes()
            );
        }
        assert_eq!(
            module.rt.calls(&run).unwrap()[0].request.as_ref(),
            Some(&frozen)
        );
        module.rt.quiesce();
        module.pool.stop();
        drop(module);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
