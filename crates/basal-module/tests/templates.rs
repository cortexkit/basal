mod common;

use basal_core::{Admission, RunState};
use basal_module::caller::Caller;
use basal_proto::JsonText;
use common::{Fixture, Options, agent, events_manifest, fixture};
use serde_json::{Value, json};

const OWNER: &str = "agent_47120287c700722b";
const OTHER: &str = "agent_d1e7002e2c9b9f43";
fn setup(tag: &str, worker: bool) -> Fixture {
    let f = fixture(
        tag,
        if worker {
            Options::with_worker()
        } else {
            Options::default()
        },
    );
    f.catalog.add_named_agent(OWNER, "Owner");
    f.catalog.add_named_agent(OTHER, "Other");
    f
}
fn manifest() -> Value {
    let mut m = events_manifest("watch");
    m["sinks"][0]["agent"] = json!("$self");
    m["params"] = json!({"note":{"type":"string","max_bytes":4096}});
    m["template"] = json!({"max_per_agent":2,"max_live":3,"max_lifetime":"1d"});
    m
}
fn register(f: &Fixture, m: &Value, script: &str) -> Value {
    f.module
        .handle(
            &Caller::Operator,
            "package.register",
            json!({"script":script,"manifest":m.to_string()}),
        )
        .unwrap()
}
fn create(
    f: &Fixture,
    owner: &str,
    values: Value,
    version: u32,
    lifetime: &str,
) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(
        &agent(owner),
        "flow.instantiate",
        json!({"template":"watch","version":version,"params":values,"expires_in":lifetime}),
    )
}
fn id(reply: &Value) -> &str {
    reply["flow_id"].as_str().unwrap()
}
fn admitted(f: &Fixture, id: &str, key: &str) -> String {
    f.module
        .rt
        .admit_trigger(id, key, JsonText::null())
        .unwrap()
        .run_id()
        .unwrap()
        .to_owned()
}
fn no_admission(f: &Fixture, id: &str, key: &str) {
    assert_eq!(
        f.module
            .rt
            .admit_trigger(id, key, JsonText::null())
            .unwrap(),
        Admission::NotApproved
    );
}

#[test]
fn live_snapshot_is_core_only_and_keeps_expired_not_ended_instances() {
    let f = setup("live-snapshot", false);
    register(&f, &manifest(), "return 1;");
    let a = create(&f, OWNER, json!({"note":"expired"}), 1, "1m").unwrap();
    let b = create(&f, OTHER, json!({"note":"ended"}), 1, "1h").unwrap();
    f.module.rt.end_instance(id(&b)).unwrap();
    f.clock.set(a["expires_at"].as_i64().unwrap());
    f.module.rt.scheduler().unwrap().tick().unwrap();
    for caller in [
        Caller::Operator,
        Caller::Local,
        agent(OWNER),
        agent(OTHER),
        Caller::Other("module".into()),
    ] {
        assert_eq!(
            f.module
                .handle(&caller, "flow.instance.live", json!({}))
                .unwrap_err()
                .code,
            "not_permitted"
        );
    }
    let reply = f
        .module
        .handle(&Caller::Core, "flow.instance.live", json!({}))
        .unwrap();
    assert_eq!(
        reply,
        json!({"instances":[{"flow_id":id(&a),"template":"watch","version":1,"agent_id":OWNER,"expires_at":a["expires_at"],"seq":2}]})
    );
    assert_eq!(
        f.module
            .handle(
                &Caller::Core,
                "flow.instance.live",
                json!({"agent_id":OWNER})
            )
            .unwrap_err()
            .code,
        "invalid_params"
    );
}

#[test]
fn lifecycle_sequence_changes_only_for_committed_changes_across_kinds() {
    let f = setup("lifecycle-sequence", false);
    let m = manifest();
    register(&f, &m, "return 1;");
    let a = create(&f, OWNER, json!({"note":"a"}), 1, "1m").unwrap();
    create(&f, OWNER, json!({"note":"a"}), 1, "1m").unwrap();
    let mut m2 = m;
    m2["version"] = json!(2);
    register(&f, &m2, "return 2;");
    create(&f, OWNER, json!({"note":"a"}), 2, "1m").unwrap();
    let extended = create(&f, OWNER, json!({"note":"a"}), 2, "2m").unwrap();
    f.clock.set(extended["expires_at"].as_i64().unwrap());
    f.module.rt.scheduler().unwrap().tick().unwrap();
    f.module.rt.end_instance(id(&a)).unwrap();
    f.module.rt.end_instance(id(&a)).unwrap();
    create(&f, OWNER, json!({"note":"a"}), 2, "1m").unwrap();
    let events = f.module.rt.pending_template_events(32).unwrap();
    assert_eq!(
        events
            .iter()
            .map(|e| (e.seq, e.event_name.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (1, "flow_instance_created"),
            (2, "flow_instance_extended"),
            (3, "flow_instance_extended"),
            (4, "flow_instance_expired"),
            (5, "flow_instance_ended"),
            (6, "flow_instance_created")
        ]
    );
    assert_eq!(
        f.module.rt.live_instances().unwrap()["instances"][0]["seq"],
        6
    );
}

#[test]
fn expiry_wakes_without_schedule_at_deadline_and_carries_canonical_params() {
    let f = setup("expiry-notice", false);
    register(&f, &manifest(), "return 1;");
    let a = create(&f, OWNER, json!({"note":"private watch"}), 1, "1m").unwrap();
    let at = a["expires_at"].as_i64().unwrap();
    // The engine's first maintenance pass establishes its retention timer.
    f.module.engine.pass().unwrap();
    assert_eq!(f.module.rt.next_wake_at(None).unwrap(), Some(at));
    f.clock.set(at - 1);
    f.module.rt.scheduler().unwrap().tick().unwrap();
    assert_eq!(f.module.rt.pending_template_events(32).unwrap().len(), 1);
    f.clock.set(at);
    f.module.rt.scheduler().unwrap().tick().unwrap();
    let events = f.module.rt.pending_template_events(32).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].event_name, "flow_instance_expired");
    let body: Value = serde_json::from_str(&events[1].body).unwrap();
    assert_eq!(body["params"], json!({"note":"private watch"}));
    assert_eq!(body["expires_at"], at);
    f.module.rt.scheduler().unwrap().tick().unwrap();
    assert_eq!(f.module.rt.pending_template_events(32).unwrap().len(), 2);
}

#[test]
fn lifecycle_change_rolls_back_if_outbox_insert_fails() {
    let f = setup("outbox-atomic", false);
    register(&f, &manifest(), "return 1;");
    f.module.rt.store().write(|tx| {
        tx.execute_batch("CREATE TRIGGER reject_event BEFORE INSERT ON template_event_outbox BEGIN SELECT RAISE(ABORT,'forced outbox failure'); END;")?;
        Ok(())
    }).unwrap();
    assert!(
        f.module
            .rt
            .instantiate("watch", 1, OWNER, &json!({"note":"atomic"}), "1h")
            .is_err()
    );
    assert!(
        f.module.rt.live_instances().unwrap()["instances"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute_batch("DROP TRIGGER reject_event")?;
            Ok(())
        })
        .unwrap();
    let a = create(&f, OWNER, json!({"note":"atomic"}), 1, "1h").unwrap();
    f.module.rt.store().write(|tx| {
        tx.execute_batch("CREATE TRIGGER reject_event BEFORE INSERT ON template_event_outbox BEGIN SELECT RAISE(ABORT,'forced outbox failure'); END;")?; Ok(())
    }).unwrap();
    assert!(f.module.rt.end_instance(id(&a)).is_err());
    assert_eq!(
        f.module.rt.live_instances().unwrap()["instances"][0]["seq"],
        1
    );
    assert!(
        f.module
            .rt
            .instantiate("watch", 1, OWNER, &json!({"note":"atomic"}), "2h")
            .is_err()
    );
    f.clock.set(a["expires_at"].as_i64().unwrap());
    assert!(f.module.rt.scheduler().unwrap().tick().is_err());
    assert_eq!(
        f.module.rt.live_instances().unwrap()["instances"][0]["seq"],
        1
    );
}

#[test]
fn instantiate_caller_version_and_reply_are_stamped_without_cards() {
    let f = setup("template-caller", false);
    let m = manifest();
    let registered = register(&f, &m, "return 1;");
    for caller in [Caller::Core, Caller::Local, Caller::Other("unknown".into())] {
        let e = f
            .module
            .handle(
                &caller,
                "flow.instantiate",
                json!({"template":"watch","version":1,"params":{"note":"a"},"expires_in":"1h"}),
            )
            .unwrap_err();
        assert_eq!(
            e.code,
            if caller == Caller::Local {
                "operator_attestation_required"
            } else {
                "not_permitted"
            }
        );
    }
    assert_eq!(
        f.module
            .handle(
                &agent(OWNER),
                "flow.instantiate",
                json!({"template":"watch","params":{"note":"a"},"expires_in":"1h"})
            )
            .unwrap_err()
            .code,
        "invalid_params"
    );
    assert_eq!(f.module.handle(&agent(OWNER),"flow.instantiate",json!({"template":"watch","version":1,"agent_id":OTHER,"params":{"note":"a"},"expires_in":"1h"})).unwrap_err().code,"not_permitted");
    assert_eq!(
        create(&f, OWNER, json!({"note":"a"}), 99, "1h")
            .unwrap_err()
            .code,
        "package_unknown"
    );
    let created=f.module.handle(&Caller::Operator,"flow.instantiate",json!({"template":"watch","version":1,"agent_id":OTHER,"params":{"note":"a"},"expires_in":"1h"})).unwrap();
    assert_eq!(created["agent_id"], OTHER);
    assert_eq!(created["template"], "watch");
    assert_eq!(created["version"], 1);
    assert_eq!(created["params"], json!({"note":"a"}));
    assert_eq!(created["new"], true);
    assert_eq!(created["expires_at"], f.clock.now_ms() + 3_600_000);
    assert!(f.consent.undelivered().is_empty());
    assert!(f.module.rt.cards(id(&created)).unwrap().is_empty());
    let stored = f.module.rt.get_package("watch", 1).unwrap();
    assert_eq!(stored["manifest"], m.to_string());
    assert_eq!(stored["code_hash"], registered["code_hash"]);
}

#[test]
fn package_template_exclusion_and_ordinary_install_dry_run_refusals() {
    let f = setup("template-exclusion", false);
    let m = manifest();
    register(&f, &m, "return 1;");
    assert_eq!(
        f.module
            .handle(
                &Caller::Core,
                "flow.instance.ensure",
                json!({"package":"watch","version":1,"agent_id":OWNER,"generation":1})
            )
            .unwrap_err()
            .code,
        "package_has_params"
    );
    for field in ["params", "template"] {
        let mut ordinary = events_manifest("ordinary");
        ordinary[field] = m[field].clone();
        assert_eq!(
            f.module
                .handle(
                    &Caller::Operator,
                    "flow.install",
                    json!({"script":"return 1;","manifest":ordinary.to_string()})
                )
                .unwrap_err()
                .code,
            "params_require_package"
        );
        let request = basal_core::InstallRequest {
            script: "return 1;".into(),
            manifest: ordinary.to_string(),
            author: "operator".into(),
            loop_override: false,
        };
        assert!(matches!(
            f.module.rt.install(&request),
            Err(basal_core::InstallError::Manifest(
                basal_core::ManifestError::ParamsRequirePackage { .. }
            ))
        ));
        ordinary[field] = Value::Null;
        assert_eq!(
            f.module
                .handle(
                    &Caller::Operator,
                    "flow.install",
                    json!({"script":"return 1;","manifest":ordinary.to_string()})
                )
                .unwrap_err()
                .code,
            "params_require_package"
        );
    }
    let mut persona = m.clone();
    persona.as_object_mut().unwrap().remove("params");
    persona.as_object_mut().unwrap().remove("template");
    persona["version"] = json!(2);
    register(&f, &persona, "return 1;");
    assert_eq!(
        create(&f, OWNER, json!({}), 2, "1h").unwrap_err().code,
        "package_not_template"
    );
    assert_eq!(
        f.module
            .handle(
                &Caller::Operator,
                "flow.dry_run",
                json!({"flow_id":"watch","version":1})
            )
            .unwrap_err()
            .code,
        "template_values_required"
    );
}

#[test]
fn idempotent_creation_moves_version_extends_never_shortens_and_keeps_state() {
    let f = setup("template-idempotency", false);
    let m = manifest();
    register(&f, &m, "return 1;");
    let first = create(&f, OWNER, json!({"note":"a"}), 1, "1h").unwrap();
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES (?1,'key','7',4,1)",
                [id(&first)],
            )?;
            Ok(())
        })
        .unwrap();
    f.module
        .handle(&agent(OWNER), "flow.disable", json!({"flow_id":id(&first)}))
        .unwrap();
    let short = create(&f, OWNER, json!({"note":"a"}), 1, "1m").unwrap();
    assert_eq!(short["new"], false);
    assert_eq!(short["expires_at"], first["expires_at"]);
    let mut next = m.clone();
    next["version"] = json!(2);
    register(&f, &next, "return 2;");
    let moved = create(&f, OWNER, json!({"note":"a"}), 2, "2h").unwrap();
    assert_eq!(moved["flow_id"], first["flow_id"]);
    assert_eq!(moved["expires_at"], f.clock.now_ms() + 7_200_000);
    assert_eq!(moved["new"], false);
    assert_eq!(
        f.module
            .rt
            .flow(id(&moved))
            .unwrap()
            .unwrap()
            .approved_version,
        Some(2)
    );
    assert!(!f.module.rt.flow(id(&moved)).unwrap().unwrap().enabled);
    f.module
        .rt
        .store()
        .read(|conn| {
            assert_eq!(
                conn.query_row("SELECT value FROM kv WHERE flow_id=?1", [id(&moved)], |r| r
                    .get::<_, String>(0))?,
                "7"
            );
            Ok(())
        })
        .unwrap();
    let mut bounded = next;
    bounded["version"] = json!(3);
    bounded["params"]["note"] = json!({"type":"enum","values":["b"]});
    register(&f, &bounded, "return 3;");
    assert_eq!(
        create(&f, OWNER, json!({"note":"a"}), 3, "3h")
            .unwrap_err()
            .code,
        "param_out_of_bounds"
    );
    assert_eq!(
        create(&f, OWNER, json!({"note":"a"}), 2, "1m").unwrap()["expires_at"],
        moved["expires_at"]
    );
    f.module
        .handle(&agent(OWNER), "flow.enable", json!({"flow_id":id(&moved)}))
        .unwrap();
    assert!(f.module.rt.flow(id(&moved)).unwrap().unwrap().enabled);
}

#[test]
fn lifetime_limit_refuses_without_creating() {
    let f = setup("template-lifetime", false);
    register(&f, &manifest(), "return 1;");
    assert_eq!(
        create(&f, OWNER, json!({"note":"a"}), 1, "2d")
            .unwrap_err()
            .code,
        "lifetime_too_long"
    );
    assert_eq!(
        create(&f, OWNER, json!({"note":"a"}), 1, "0s")
            .unwrap_err()
            .code,
        "invalid_params"
    );
    assert_eq!(
        f.module
            .handle(&Caller::Operator, "flow.list", json!({}))
            .unwrap()["flows"],
        json!([])
    );
    assert!(create(&f, OWNER, json!({"note":"a"}), 1, "1d").is_ok());
}

fn count(f: &Fixture, table: &str) -> i64 {
    f.module
        .rt
        .store()
        .read(|c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?))
        .unwrap()
}

fn registry_failures() -> [basal_host::transport::WireError; 3] {
    use basal_host::transport::WireError;
    [
        WireError::NeverSent("core restarting".into()),
        WireError::TimedOut("agent.list timed out".into()),
        WireError::Unreadable("agent.list reply undecodable".into()),
    ]
}

#[test]
fn instantiate_refuses_a_failed_agent_registry_read_as_retryable_and_writes_nothing() {
    for failure in registry_failures() {
        let f = setup("template-registry-down", false);
        register(&f, &manifest(), "return 1;");
        f.catalog.fail_agent_registry(Some(failure.clone()));
        let as_agent = create(&f, OWNER, json!({"note":"a"}), 1, "1h").unwrap_err();
        let as_operator = f
            .module
            .handle(
                &Caller::Operator,
                "flow.instantiate",
                json!({"template":"watch","version":1,"agent_id":OWNER,"params":{"note":"a"},"expires_in":"1h"}),
            )
            .unwrap_err();
        for error in [as_agent, as_operator] {
            assert_eq!(error.code, "agent_registry_unavailable", "{failure:?}");
            assert_eq!(error.detail, Some(json!({"retryable": true})));
        }
        assert_eq!(count(&f, "flows"), 0);
        assert_eq!(count(&f, "template_instances"), 0);
        assert!(f.module.rt.pending_template_events(32).unwrap().is_empty());
        // The refusal was not a verdict: the same call succeeds once the
        // registry answers again.
        f.catalog.fail_agent_registry(None);
        let created = create(&f, OWNER, json!({"note":"a"}), 1, "1h").unwrap();
        assert_eq!(created["new"], true);
    }
}

#[test]
fn instantiate_refuses_an_agent_absent_from_an_answered_registry_as_unknown() {
    let f = setup("template-registry-absent", false);
    register(&f, &manifest(), "return 1;");
    // Neither an unlisted id nor a display name standing in for a stable id
    // is a known agent.
    for missing in ["agent_0000000000000000", "Owner"] {
        let error = create(&f, missing, json!({"note":"a"}), 1, "1h").unwrap_err();
        assert_eq!(error.code, "agent_unknown", "{missing}");
        assert_eq!(error.detail, None);
    }
    assert_eq!(count(&f, "flows"), 0);
    assert_eq!(count(&f, "template_instances"), 0);
}

#[test]
fn template_dry_run_refuses_a_failed_agent_registry_read_as_retryable() {
    for failure in registry_failures() {
        let f = setup("template-dry-registry-down", false);
        register(&f, &manifest(), "return 1;");
        f.catalog.fail_agent_registry(Some(failure.clone()));
        let error = dry(
            &f,
            &agent(OWNER),
            json!({"flow_id":"watch","params":{"note":"a"},"trigger":{}}),
        )
        .unwrap_err();
        assert_eq!(error.code, "agent_registry_unavailable", "{failure:?}");
        assert_eq!(error.detail, Some(json!({"retryable": true})));
        assert_eq!(count(&f, "flows"), 0);
        assert_eq!(count(&f, "runs"), 0);
    }
}

#[test]
fn template_dry_run_refuses_an_agent_absent_from_an_answered_registry_as_unknown() {
    let f = setup("template-dry-registry-absent", false);
    register(&f, &manifest(), "return 1;");
    for missing in ["agent_0000000000000000", "Owner"] {
        let error = dry(
            &f,
            &agent(missing),
            json!({"flow_id":"watch","params":{"note":"a"},"trigger":{}}),
        )
        .unwrap_err();
        assert_eq!(error.code, "agent_unknown", "{missing}");
        assert_eq!(error.detail, None);
    }
    assert_eq!(count(&f, "flows"), 0);
}

#[test]
fn instance_limits_cover_agent_and_template_and_exclude_ended_expired() {
    let f = setup("template-limits", false);
    register(&f, &manifest(), "return 1;");
    let a = create(&f, OWNER, json!({"note":"a"}), 1, "1h").unwrap();
    let b = create(&f, OWNER, json!({"note":"b"}), 1, "1h").unwrap();
    let e = create(&f, OWNER, json!({"note":"c"}), 1, "1h").unwrap_err();
    assert_eq!(e.code, "instance_limit_reached");
    assert_eq!(e.detail, Some(json!({"limit":2,"scope":"agent"})));
    assert_eq!(
        create(&f, OWNER, json!({"note":"a"}), 1, "2h").unwrap()["new"],
        false
    );
    create(&f, OTHER, json!({"note":"a"}), 1, "1h").unwrap();
    let e = create(&f, OTHER, json!({"note":"b"}), 1, "1h").unwrap_err();
    assert_eq!(e.code, "instance_limit_reached");
    assert_eq!(e.detail, Some(json!({"limit":3,"scope":"template"})));
    f.module
        .handle(
            &agent(OWNER),
            "flow.instance.end",
            json!({"flow_id":id(&b)}),
        )
        .unwrap();
    create(&f, OTHER, json!({"note":"b"}), 1, "1h").unwrap();
    f.clock.advance(3_600_000);
    create(&f, OTHER, json!({"note":"c"}), 1, "1h").unwrap();
    assert!(create(&f, OWNER, json!({"note":"a"}), 1, "1h").is_ok());
    assert_eq!(
        f.module.rt.flow(id(&a)).unwrap().unwrap().approved_version,
        Some(1)
    );
}

#[test]
fn end_is_owner_scoped_repeatable_kept_and_reopened_by_same_values() {
    let f = setup("template-end", false);
    register(&f, &manifest(), "return 1;");
    let created = create(&f, OWNER, json!({"note":"a"}), 1, "1h").unwrap();
    for caller in [agent(OTHER), Caller::Core, Caller::Local] {
        assert_eq!(
            f.module
                .handle(
                    &caller,
                    "flow.instance.end",
                    json!({"flow_id":id(&created)})
                )
                .unwrap_err()
                .code,
            "not_permitted"
        );
    }
    let request = json!({"flow_id":id(&created)});
    assert_eq!(
        f.module
            .handle(&agent(OWNER), "flow.instance.end", request.clone())
            .unwrap(),
        json!({"flow_id":id(&created),"ended":true})
    );
    assert_eq!(
        f.module
            .handle(&Caller::Operator, "flow.instance.end", request)
            .unwrap()["ended"],
        false
    );
    no_admission(&f, id(&created), "after-end");
    f.module
        .handle(
            &Caller::Operator,
            "flow.enable",
            json!({"flow_id":id(&created)}),
        )
        .unwrap();
    no_admission(&f, id(&created), "after-enable");
    assert!(f.module.rt.flow(id(&created)).unwrap().is_some());
    assert_eq!(
        create(&f, OWNER, json!({"note":"a"}), 1, "2h").unwrap()["new"],
        false
    );
    admitted(&f, id(&created), "reopened");
    assert_eq!(
        f.module
            .handle(
                &Caller::Operator,
                "flow.instance.end",
                json!({"flow_id":"absent"})
            )
            .unwrap_err()
            .code,
        "not_found"
    );
    let mut persona = manifest();
    persona["id"] = json!("persona");
    persona.as_object_mut().unwrap().remove("params");
    persona.as_object_mut().unwrap().remove("template");
    register(&f, &persona, "return 1;");
    let ensured = f
        .module
        .handle(
            &Caller::Core,
            "flow.instance.ensure",
            json!({"package":"persona","version":1,"agent_id":OWNER,"generation":1}),
        )
        .unwrap();
    assert_eq!(
        f.module
            .handle(
                &Caller::Operator,
                "flow.instance.end",
                json!({"flow_id":ensured["flow_id"]})
            )
            .unwrap_err()
            .code,
        "package_not_template"
    );
}

#[test]
fn expiry_blocks_admission_at_deadline_and_stops_kept_schedule() {
    let f = setup("template-expiry", false);
    let mut m = manifest();
    m["trigger"] = json!({"schedule":{"interval":"1m"}});
    register(&f, &m, "return 1;");
    let created = create(&f, OWNER, json!({"note":"a"}), 1, "1m").unwrap();
    let before = admitted(&f, id(&created), "before-expiry");
    f.module.rt.cancel(&before, "operator").unwrap();
    f.clock.advance(60_000);
    no_admission(&f, id(&created), "at-expiry");
    assert!(
        f.module
            .rt
            .scheduler()
            .unwrap()
            .tick()
            .unwrap()
            .new_runs()
            .is_empty()
    );
    assert_eq!(
        f.module
            .rt
            .scheduler()
            .unwrap()
            .schedule(id(&created))
            .unwrap()
            .unwrap()
            .state,
        basal_core::schedule::ScheduleState::Disabled
    );
    assert!(f.module.rt.flow(id(&created)).unwrap().is_some());
    let reopened = create(&f, OWNER, json!({"note":"a"}), 1, "1h").unwrap();
    assert_eq!(reopened["new"], false);
    admitted(&f, id(&created), "after-extension");
}

#[test]
fn list_exposes_template_values_version_expiry_and_remains_owner_scoped() {
    let f = setup("template-list", false);
    register(&f, &manifest(), "return 1;");
    let created = create(&f, OWNER, json!({"note":"a"}), 1, "1h").unwrap();
    let list = f
        .module
        .handle(&agent(OWNER), "flow.list", json!({}))
        .unwrap();
    let row = &list["flows"][0];
    for field in ["flow_id", "template", "version", "params", "expires_at"] {
        assert_eq!(row[field], created[field], "{field}");
    }
    assert_eq!(
        f.module
            .handle(&agent(OTHER), "flow.list", json!({}))
            .unwrap()["flows"],
        json!([])
    );
}

#[test]
#[cfg(unix)]
fn path_and_repo_values_resolve_symlinks_and_future_ancestors_before_bounds() {
    use std::os::unix::fs::symlink;
    let f = setup("template-paths", false);
    let inside = f.dir.join("inside");
    let outside = f.dir.join("outside");
    std::fs::create_dir(&inside).unwrap();
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, inside.join("escape")).unwrap();
    let real = inside.join("real");
    std::fs::create_dir(&real).unwrap();
    symlink(&real, inside.join("alias")).unwrap();
    let mut m = manifest();
    m["params"] = json!({"path":{"type":"path","under":[inside]}});
    m["fs"] = json!({"read":["$params.path"]});
    register(&f, &m, "return 1;");
    for path in [
        inside.join("escape"),
        inside.join("escape/not/yet"),
        outside.clone(),
    ] {
        assert_eq!(
            create(&f, OWNER, json!({"path":path}), 1, "1h")
                .unwrap_err()
                .code,
            "param_out_of_bounds"
        );
    }
    let canonical = std::fs::canonicalize(&real).unwrap().join("not/yet");
    let alias = create(
        &f,
        OWNER,
        json!({"path":inside.join("alias/not/yet")}),
        1,
        "1h",
    )
    .unwrap();
    assert_eq!(alias["params"]["path"], canonical.to_str().unwrap());
    assert_eq!(
        create(&f, OWNER, json!({"path":canonical}), 1, "1h").unwrap()["flow_id"],
        alias["flow_id"]
    );
    let repo = inside.join("repo");
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    symlink(&repo, inside.join("repo-alias")).unwrap();
    let mut next = m;
    next["version"] = json!(2);
    next["params"] = json!({"repo":{"type":"repo","under":[inside]}});
    next.as_object_mut().unwrap().remove("fs");
    next["git"] = json!({"read":["$params.repo"]});
    register(&f, &next, "return 1;");
    let created = create(
        &f,
        OTHER,
        json!({"repo":inside.join("repo-alias")}),
        2,
        "1h",
    )
    .unwrap();
    assert_eq!(
        created["params"]["repo"],
        std::fs::canonicalize(repo).unwrap().to_str().unwrap()
    );
    for path in [outside, real, inside.join("missing")] {
        assert_eq!(
            create(&f, OTHER, json!({"repo":path}), 2, "1h")
                .unwrap_err()
                .code,
            "param_out_of_bounds"
        );
    }
}

#[test]
fn parameter_roots_must_exist_at_registration() {
    let f = setup("template-root-exists", false);
    let mut m = manifest();
    m["params"] = json!({"path":{"type":"path","under":[f.dir.join("absent")]}});
    m["fs"] = json!({"read":["$params.path"]});
    let e = f
        .module
        .handle(
            &Caller::Operator,
            "package.register",
            json!({"script":"return 1;","manifest":m.to_string()}),
        )
        .unwrap_err();
    assert_eq!(e.code, "template_bound_invalid");
    assert_eq!(e.detail.unwrap()["field"], "params.path.under");
}

#[test]
#[cfg(unix)]
fn late_parameter_root_symlink_escape_is_refused_by_the_host() {
    late_symlink_case(false, false, false);
}
#[test]
#[cfg(unix)]
fn late_parameter_ancestor_symlink_escape_is_refused_by_the_host() {
    late_symlink_case(true, false, false);
}
#[test]
#[cfg(unix)]
fn future_parameter_path_cannot_escape_through_a_later_symlink() {
    late_symlink_case(true, true, false);
}
#[cfg(unix)]
fn late_symlink_case(ancestor: bool, future: bool, within: bool) {
    use basal_host::builtins::{BuiltinHost, Failure, Grant, codes, envelope};
    use std::os::unix::fs::symlink;
    let f = setup("template-late-symlink", false);
    let inside = f.dir.join("inside");
    let outside = if within {
        inside.join("other")
    } else {
        f.dir.join("outside")
    };
    std::fs::create_dir(&inside).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let parent = inside.join("parent");
    std::fs::create_dir(&parent).unwrap();
    let target = parent.join("file");
    if !future {
        std::fs::write(&target, "approved").unwrap();
    }
    std::fs::write(outside.join("file"), "secret").unwrap();
    let mut m = manifest();
    m["params"] = json!({"path":{"type":"path","under":[inside]}});
    m["fs"] = json!({"read":["$params.path"]});
    register(&f, &m, "return 1;");
    let instance = create(&f, OWNER, json!({"path":target}), 1, "1h").unwrap();
    let run = admitted(&f, id(&instance), "snapshot");
    let run = f.module.rt.run(&run).unwrap();
    let grant = Grant {
        roots: vec![instance["params"]["path"].as_str().unwrap().into()],
        hosts: vec![],
        template_roots: serde_json::from_str(run.grant_guards.as_str()).unwrap(),
    };
    if ancestor {
        std::fs::rename(&parent, inside.join("saved-parent")).unwrap();
        symlink(&outside, &parent).unwrap();
    } else {
        std::fs::remove_file(&target).unwrap();
        symlink(outside.join("file"), &target).unwrap();
    }
    let request = envelope(&json!({"path":target}), &grant);
    match BuiltinHost::default().run(basal_proto::Primitive::FsRead, &request) {
        Err(Failure::Refused(denial)) => assert_eq!(denial.code, codes::DENIED),
        other => panic!("escaped template authority: {other:?}"),
    }
}

#[test]
fn activation_uses_effective_grants_and_journaled_self_and_params() {
    let f = setup("template-effective-run", true);
    let target = f.dir.join("target");
    std::fs::write(&target, "watched").unwrap();
    std::fs::write(f.dir.join("neighbor"), "private").unwrap();
    let root = common::canonical(&f.dir);
    let mut m = manifest();
    m["params"]["path"] = json!({"type":"path","under":[root]});
    m["fs"] = json!({"read":["$params.path"]});
    register(
        &f,
        &m,
        "const read = await fs.read(params.path); let refusal; try { await fs.read(params.path.slice(0, -6) + 'neighbor'); } catch (e) { refusal = e.data.code; } return { self, params, text: read.text, refusal };",
    );
    let created = create(&f, OWNER, json!({"note":"Grüße","path":target}), 1, "1h").unwrap();
    let run = admitted(&f, id(&created), "input");
    f.module.engine.run_until_idle(50).unwrap();
    let stored = f.module.rt.run(&run).unwrap();
    assert_eq!(stored.state, RunState::Succeeded, "{stored:?}");
    let result: Value = serde_json::from_str(stored.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["self"], json!({"agent_id":OWNER}));
    assert_eq!(result["params"], created["params"]);
    assert_eq!(result["text"], "watched");
    assert_eq!(result["refusal"], "denied");
    let guards: Value = serde_json::from_str(stored.grant_guards.as_str()).unwrap();
    assert_eq!(guards[0]["root"], created["params"]["path"]);
    assert_eq!(guards[0]["under"], json!([root]));
    assert_eq!(
        serde_json::from_str::<Value>(stored.params_input.as_str()).unwrap(),
        created["params"]
    );
    assert_eq!(
        basal_core::ids::hex(&stored.code_hash),
        f.module.rt.get_package("watch", 1).unwrap()["code_hash"]
    );
}

#[test]
fn one_shot_fires_once_after_downtime_and_ends_on_success_failure_or_cancel() {
    for (tag, script, cancel) in [
        ("success", "return params.at;", false),
        ("failure", "throw new Error('failed');", false),
        ("cancel", "return 1;", true),
    ] {
        let f = setup(&format!("template-at-{tag}"), !cancel);
        let mut m = manifest();
        m["params"] = json!({"at":{"type":"timestamp","max_ahead":"1d"}});
        m["trigger"] = json!({"schedule":{"at":"$params.at"}});
        register(&f, &m, script);
        let at = jiff::Timestamp::from_millisecond(f.clock.now_ms() + 60_000)
            .unwrap()
            .to_string();
        let instance = create(&f, OWNER, json!({"at":at}), 1, "1h").unwrap();
        assert!(
            f.module
                .rt
                .scheduler()
                .unwrap()
                .tick()
                .unwrap()
                .new_runs()
                .is_empty()
        );
        f.clock.advance(120_000);
        let tick = f.module.rt.scheduler().unwrap().tick().unwrap();
        assert_eq!(tick.new_runs().len(), 1);
        let run = tick.new_runs()[0].to_owned();
        if cancel {
            f.module.rt.cancel(&run, "operator").unwrap();
        } else {
            f.module.engine.run_until_idle(50).unwrap();
        }
        let state = f.module.rt.run(&run).unwrap().state;
        assert_eq!(
            state,
            if cancel {
                RunState::Cancelled
            } else if tag == "failure" {
                RunState::Failed
            } else {
                RunState::Succeeded
            }
        );
        no_admission(&f, id(&instance), "after-fire");
        assert!(
            f.module
                .rt
                .scheduler()
                .unwrap()
                .tick()
                .unwrap()
                .new_runs()
                .is_empty()
        );
        assert_eq!(
            f.module
                .rt
                .scheduler()
                .unwrap()
                .schedule(id(&instance))
                .unwrap()
                .unwrap()
                .state,
            basal_core::schedule::ScheduleState::Disabled
        );
        assert_eq!(
            create(&f, OWNER, json!({"at":at}), 1, "1h")
                .unwrap_err()
                .code,
            "param_out_of_bounds"
        );
    }
}

#[test]
fn past_or_too_distant_timestamp_is_refused_at_every_creation() {
    let f = setup("template-at-bounds", false);
    let mut m = manifest();
    m["params"] = json!({"at":{"type":"timestamp","max_ahead":"1h"}});
    m["trigger"] = json!({"schedule":{"at":"$params.at"}});
    register(&f, &m, "return 1;");
    for delta in [-1, 3_600_001] {
        let at = jiff::Timestamp::from_millisecond(f.clock.now_ms() + delta)
            .unwrap()
            .to_string();
        assert_eq!(
            create(&f, OWNER, json!({"at":at}), 1, "1h")
                .unwrap_err()
                .code,
            "param_out_of_bounds"
        );
    }
    let at = jiff::Timestamp::from_millisecond(f.clock.now_ms())
        .unwrap()
        .to_string();
    let instance = create(&f, OWNER, json!({"at":at}), 1, "1h").unwrap();
    assert_eq!(
        f.module
            .rt
            .scheduler()
            .unwrap()
            .tick()
            .unwrap()
            .new_runs()
            .len(),
        1
    );
    assert!(f.module.rt.flow(id(&instance)).unwrap().is_some());
}

#[test]
fn a_template_run_ends_only_itself_after_finishing_even_when_it_fails() {
    for fails in [false, true] {
        let f = setup("template-self-end", true);
        let script = if fails {
            "await instance.end(); throw new Error('after end');"
        } else {
            "await instance.end(); return await kv.set('finished', true);"
        };
        register(&f, &manifest(), script);
        let a = create(&f, OWNER, json!({"note":"a"}), 1, "1h").unwrap();
        let b = create(&f, OTHER, json!({"note":"b"}), 1, "1h").unwrap();
        let run = admitted(&f, id(&a), "self-end");
        f.module.engine.run_until_idle(50).unwrap();
        let stored = f.module.rt.run(&run).unwrap();
        assert_eq!(
            stored.state,
            if fails {
                RunState::Failed
            } else {
                RunState::Succeeded
            },
            "{stored:?}"
        );
        no_admission(&f, id(&a), "ended");
        admitted(&f, id(&b), "still-live");
        assert_eq!(
            f.module
                .rt
                .calls(&run)
                .unwrap()
                .iter()
                .filter(|row| row.kind
                    == basal_proto::CallKind::Primitive(basal_proto::Primitive::InstanceEnd))
                .count(),
            1
        );
    }
}

#[test]
fn self_end_outside_a_template_instance_is_a_journaled_refusal() {
    let f = setup("template-self-end-refusal", true);
    let script = "try { await instance.end(); } catch (e) { return e.data.code; } throw new Error('accepted');";
    let flow =
        common::install_approved(&f, &Caller::Operator, script, &events_manifest("ordinary"));
    let run = admitted(&f, &flow, "outside");
    f.module.engine.run_until_idle(50).unwrap();
    let stored = f.module.rt.run(&run).unwrap();
    assert_eq!(stored.state, RunState::Succeeded, "{stored:?}");
    assert_eq!(
        f.module.rt.run(&run).unwrap().result.as_deref(),
        Some("\"not_a_template_instance\"")
    );
    assert!(f.module.rt.flow(&flow).unwrap().unwrap().enabled);
}

#[test]
#[cfg(unix)]
fn existing_parameter_root_cannot_be_retargeted_within_its_bound() {
    late_symlink_case(false, false, true);
}

#[test]
#[cfg(unix)]
fn late_repo_root_symlink_escape_is_refused_and_static_grants_still_apply() {
    use basal_host::builtins::{BuiltinHost, Failure, Grant, codes, envelope};
    use std::os::unix::fs::symlink;
    let f = setup("template-late-repo", false);
    let inside = f.dir.join("inside");
    let outside = f.dir.join("outside");
    std::fs::create_dir(&inside).unwrap();
    let repo = inside.join("repo");
    for path in [&repo, &outside] {
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    }
    let mut m = manifest();
    m["params"] = json!({"repo":{"type":"repo","under":[inside]}});
    m["git"] = json!({"read":["$params.repo"]});
    register(&f, &m, "return 1;");
    let instance = create(&f, OWNER, json!({"repo":repo}), 1, "1h").unwrap();
    let run = admitted(&f, id(&instance), "guards");
    let run = f.module.rt.run(&run).unwrap();
    let grant = Grant {
        roots: vec![instance["params"]["repo"].as_str().unwrap().into()],
        hosts: vec![],
        template_roots: serde_json::from_str(run.grant_guards.as_str()).unwrap(),
    };
    std::fs::rename(&repo, inside.join("saved")).unwrap();
    symlink(&outside, &repo).unwrap();
    let args = json!({"repo":repo,"ref":"HEAD"});
    match BuiltinHost::default().run(
        basal_proto::Primitive::GitRevParse,
        &envelope(&args, &grant),
    ) {
        Err(Failure::Refused(denial)) => assert_eq!(denial.code, codes::DENIED),
        other => panic!("repo escaped: {other:?}"),
    }
    let mut static_grant = grant;
    static_grant.roots.push(
        std::fs::canonicalize(&outside)
            .unwrap()
            .to_str()
            .unwrap()
            .into(),
    );
    assert_eq!(
        static_grant.checked_roots().unwrap().len(),
        1,
        "a rejected parameter root does not cancel a separate static grant"
    );
    std::fs::write(outside.join("static-note"), "explicit grant").unwrap();
    assert_eq!(
        BuiltinHost::default()
            .run(
                basal_proto::Primitive::FsRead,
                &envelope(&json!({"path":outside.join("static-note")}), &static_grant)
            )
            .unwrap()["text"],
        "explicit grant"
    );
}

#[test]
fn template_stat_of_a_missing_target_reports_absence_then_appearance() {
    let f = setup("template-future-stat", true);
    let target = f.dir.join("future/nested/file");
    let mut m = manifest();
    m["params"] = json!({"path":{"type":"path","under":[f.dir]}});
    m["fs"] = json!({"read":["$params.path"]});
    register(&f, &m, "return await fs.stat(params.path);");
    let instance = create(&f, OWNER, json!({"path":target}), 1, "1h").unwrap();
    let first = admitted(&f, id(&instance), "missing");
    f.module.engine.run_until_idle(50).unwrap();
    let run = f.module.rt.run(&first).unwrap();
    assert_eq!(run.state, RunState::Succeeded, "{run:?}");
    assert_eq!(
        serde_json::from_str::<Value>(run.result.as_deref().unwrap()).unwrap(),
        json!({"exists":false})
    );
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "appeared").unwrap();
    let second = admitted(&f, id(&instance), "appeared");
    f.module.engine.run_until_idle(50).unwrap();
    let run = f.module.rt.run(&second).unwrap();
    assert_eq!(run.state, RunState::Succeeded, "{run:?}");
    assert_eq!(
        serde_json::from_str::<Value>(run.result.as_deref().unwrap()).unwrap()["exists"],
        true
    );
}

#[test]
#[cfg(unix)]
fn missing_stat_with_an_escaped_ancestor_is_refused_not_reported_absent() {
    use basal_host::builtins::{BuiltinHost, Failure, Grant, codes, envelope};
    use std::os::unix::fs::symlink;
    let f = setup("template-missing-stat-escape", false);
    let inside = f.dir.join("inside");
    let outside = f.dir.join("outside");
    std::fs::create_dir(&inside).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let target = inside.join("future/file");
    let mut m = manifest();
    m["params"] = json!({"path":{"type":"path","under":[inside]}});
    m["fs"] = json!({"read":["$params.path"]});
    register(&f, &m, "return 1;");
    let instance = create(&f, OWNER, json!({"path":target}), 1, "1h").unwrap();
    let run = admitted(&f, id(&instance), "stat");
    let run = f.module.rt.run(&run).unwrap();
    let grant = Grant {
        roots: vec![instance["params"]["path"].as_str().unwrap().into()],
        hosts: vec![],
        template_roots: serde_json::from_str(run.grant_guards.as_str()).unwrap(),
    };
    let host = BuiltinHost::default();
    let request = envelope(&json!({"path":target}), &grant);
    assert_eq!(
        host.run(basal_proto::Primitive::FsStat, &request).unwrap(),
        json!({"exists":false})
    );
    let mut ordinary = grant.clone();
    ordinary.template_roots.clear();
    assert!(matches!(
        host.run(
            basal_proto::Primitive::FsStat,
            &envelope(&json!({"path":target}), &ordinary)
        ),
        Err(Failure::Refused(_))
    ));
    symlink(&outside, inside.join("future")).unwrap();
    match host.run(basal_proto::Primitive::FsStat, &request) {
        Err(Failure::Refused(denial)) => assert_eq!(denial.code, codes::DENIED),
        other => panic!("escaped missing stat: {other:?}"),
    }
}

#[test]
fn repo_creation_checks_layout_and_git_reads_validate_its_contents() {
    use basal_host::builtins::{BuiltinHost, Failure, Grant, codes, envelope};
    let f = setup("template-repo-layout", false);
    let repo = f.dir.join("repo");
    std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
    std::fs::create_dir_all(repo.join(".git/refs")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "not a valid git HEAD").unwrap();
    let mut m = manifest();
    m["params"] = json!({"repo":{"type":"repo","under":[f.dir]}});
    m["git"] = json!({"read":["$params.repo"]});
    register(&f, &m, "return 1;");
    let instance = create(&f, OWNER, json!({"repo":repo}), 1, "1h").unwrap();
    let run = admitted(&f, id(&instance), "layout");
    let run = f.module.rt.run(&run).unwrap();
    let grant = Grant {
        roots: vec![instance["params"]["repo"].as_str().unwrap().into()],
        hosts: vec![],
        template_roots: serde_json::from_str(run.grant_guards.as_str()).unwrap(),
    };
    match BuiltinHost::default().run(
        basal_proto::Primitive::GitRevParse,
        &envelope(&json!({"repo":repo,"ref":"HEAD"}), &grant),
    ) {
        Err(Failure::Refused(denial)) => assert_ne!(
            denial.code,
            codes::DENIED,
            "the repository layout was authorized; Git refused its contents"
        ),
        other => panic!("malformed repository read: {other:?}"),
    }
}

#[test]
fn repository_layout_pointer_reads_are_bounded_during_creation() {
    let f = setup("template-repo-pointer-cap", false);
    let real = f.dir.join("real");
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&real)
            .status()
            .unwrap()
            .success()
    );
    let candidate = f.dir.join("candidate");
    std::fs::create_dir(&candidate).unwrap();
    let pointer = format!("gitdir: {}\n", real.join(".git").display());
    std::fs::write(candidate.join(".git"), &pointer).unwrap();
    let mut m = manifest();
    m["params"] = json!({"repo":{"type":"repo","under":[f.dir]}});
    m["git"] = json!({"read":["$params.repo"]});
    register(&f, &m, "return 1;");
    assert!(create(&f, OWNER, json!({"repo":candidate}), 1, "1h").is_ok());
    std::fs::write(
        candidate.join(".git"),
        format!("{pointer}{}", " ".repeat(1024 * 1024)),
    )
    .unwrap();
    assert_eq!(
        create(&f, OWNER, json!({"repo":candidate}), 1, "1h")
            .unwrap_err()
            .code,
        "param_out_of_bounds"
    );
}

#[test]
fn null_optional_at_does_not_end_a_recurring_instance() {
    let f = setup("template-null-at", true);
    let mut m = manifest();
    m["trigger"] = json!({"schedule":{"interval":"1m","at":null}});
    register(&f, &m, "return true;");
    let instance = create(&f, OWNER, json!({"note":"recurring"}), 1, "1h").unwrap();
    let run = admitted(&f, id(&instance), "first");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
    assert!(matches!(
        f.module
            .rt
            .admit_trigger(id(&instance), "second", JsonText::null())
            .unwrap(),
        Admission::Admitted { .. }
    ));
}

#[test]
fn overdue_expiry_is_recorded_before_renewal_or_end_without_scheduler_tick() {
    let f = setup("expiry-before-change", false);
    register(&f, &manifest(), "return 1;");
    let a = create(&f, OWNER, json!({"note":"renew"}), 1, "1m").unwrap();
    f.clock.set(a["expires_at"].as_i64().unwrap());
    create(&f, OWNER, json!({"note":"renew"}), 1, "1m").unwrap();
    let events = f.module.rt.pending_template_events(32).unwrap();
    assert_eq!(
        events
            .iter()
            .map(|e| e.event_name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "flow_instance_created",
            "flow_instance_expired",
            "flow_instance_extended"
        ]
    );
    let b = create(&f, OTHER, json!({"note":"end"}), 1, "1m").unwrap();
    f.clock.set(b["expires_at"].as_i64().unwrap());
    f.module.rt.end_instance(id(&b)).unwrap();
    let events = f.module.rt.pending_template_events(32).unwrap();
    let ended = events
        .iter()
        .filter(|e| e.flow_id == id(&b))
        .collect::<Vec<_>>();
    assert_eq!(
        ended
            .iter()
            .map(|e| e.event_name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "flow_instance_created",
            "flow_instance_expired",
            "flow_instance_ended"
        ]
    );
    assert_eq!(
        ended.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    f.module.rt.end_instance(id(&a)).unwrap();
    f.clock.advance(120_000);
    f.module.rt.scheduler().unwrap().tick().unwrap();
    assert!(
        f.module
            .rt
            .pending_template_events(32)
            .unwrap()
            .iter()
            .filter(|e| e.flow_id == id(&b))
            .count()
            == 3
    );
}

fn dry(f: &Fixture, caller: &Caller, request: Value) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(caller, "flow.dry_run", request)
}

#[test]
fn template_dry_run_uses_canonical_values_effective_grants_and_capture() {
    let f = setup("template-dry-effective", true);
    let target = f.dir.join("target");
    std::fs::write(&target, "watched").unwrap();
    std::fs::write(f.dir.join("neighbor"), "private").unwrap();
    let root = common::canonical(&f.dir);
    let mut m = manifest();
    m["params"]["path"] = json!({"type":"path","under":[root]});
    m["fs"] = json!({"read":["$params.path"]});
    register(
        &f,
        &m,
        "const read = await fs.read(params.path).catch(e => e.data.code); let refusal; try { await fs.read(params.path.slice(0, -6) + 'neighbor'); } catch (e) { refusal = e.data.code; } const captured = await sink.digest(self.agent_id, {title: params.note}).catch(e => e.data.code); return {self, params, read, refusal, captured};",
    );
    let reply = dry(&f, &agent(OTHER), json!({"flow_id":"watch","params":{"note":"Grüße","path":f.dir.join("./target")},"trigger":{}})).unwrap();
    assert_eq!(reply["version"], 1);
    assert_eq!(reply["summary"]["mode"], "capture");
    let run = &reply["summary"]["runs"][0];
    assert_eq!(run["state"], "succeeded", "{reply:#}");
    assert_eq!(
        run["result"],
        json!({"self":{"agent_id":OTHER},"params":{"note":"Grüße","path":root.join("target")},"read":"captured","refusal":"denied","captured":"captured"})
    );
    assert_eq!(f.mock.total_sends(), 0);
    assert!(f.consent.undelivered().is_empty());
    assert_eq!(
        f.module.rt.get_package("watch", 1).unwrap()["manifest"],
        m.to_string()
    );
}

#[test]
fn template_dry_run_param_missing_matches_instantiate() {
    let f = setup("template-dry-missing", false);
    register(&f, &manifest(), "return 1;");
    let error = dry(&f, &agent(OWNER), json!({"flow_id":"watch","params":{}})).unwrap_err();
    assert_eq!(error.code, "param_missing");
    assert_eq!(
        error.detail,
        create(&f, OWNER, json!({}), 1, "1h").unwrap_err().detail
    );
}

#[test]
fn template_dry_run_param_unknown_matches_instantiate() {
    let f = setup("template-dry-unknown", false);
    register(&f, &manifest(), "return 1;");
    let values = json!({"note":"a", "unknown":1});
    let error = dry(
        &f,
        &agent(OWNER),
        json!({"flow_id":"watch","params":values}),
    )
    .unwrap_err();
    assert_eq!(error.code, "param_unknown");
    assert_eq!(
        error.detail,
        create(&f, OWNER, values, 1, "1h").unwrap_err().detail
    );
}

#[test]
fn template_dry_run_bounds_preserve_integer_and_timestamp_rules() {
    let f = setup("template-dry-bounds", false);
    let mut m = manifest();
    m["params"] =
        json!({"at":{"type":"timestamp","max_ahead":"1h"}, "n":{"type":"integer","min":1,"max":2}});
    register(&f, &m, "return params;");
    for values in [
        json!({"at":"2026-05-01T00:00:00Z","n":1.0}),
        json!({"at":"2026-05-01T00:00:00Z","n":3}),
        json!({"at":"2026-04-30T23:59:59Z","n":1}),
        json!({"at":"2026-05-01T01:00:01Z","n":1}),
        json!({"at":"2026-05-01T00:00:00.000000001Z","n":1}),
    ] {
        let error = dry(
            &f,
            &agent(OWNER),
            json!({"flow_id":"watch","params":values}),
        )
        .unwrap_err();
        assert_eq!(error.code, "param_out_of_bounds", "{values}");
        let instantiated = create(&f, OWNER, values, 1, "1h").unwrap_err();
        assert_eq!(error.detail, instantiated.detail);
    }
    assert_eq!(
        dry(&f, &agent(OWNER), json!({"flow_id":"watch","params":null}))
            .unwrap_err()
            .code,
        "invalid_params"
    );
}

#[test]
fn template_dry_run_creates_no_rows_and_uses_no_quota() {
    let f = setup("template-dry-quota", true);
    let mut m = manifest();
    m["template"] = json!({"max_per_agent":1,"max_live":1,"max_lifetime":"1d"});
    register(&f, &m, "return params.note;");
    let snapshot = || {
        f.module.rt.store().read(|c| {
        Ok(c.query_row("SELECT (SELECT count(*) FROM flows),(SELECT count(*) FROM template_instances),(SELECT count(*) FROM runs),(SELECT count(*) FROM schedules),(SELECT count(*) FROM template_event_outbox)", [], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?)))?)
    }).unwrap()
    };
    let before = snapshot();
    let request = json!({"flow_id":"watch","params":{"note":"preview"},"trigger":{}});
    let reply = dry(&f, &agent(OWNER), request.clone()).unwrap();
    assert_eq!(reply["summary"]["runs"][0]["result"], "preview");
    assert_eq!(snapshot(), before);
    create(&f, OWNER, json!({"note":"real"}), 1, "1h").unwrap();
    assert_eq!(
        create(&f, OWNER, json!({"note":"another"}), 1, "1h")
            .unwrap_err()
            .code,
        "instance_limit_reached"
    );
    let full = snapshot();
    assert_eq!(
        dry(&f, &agent(OWNER), request).unwrap()["summary"]["runs"][0]["result"],
        "preview"
    );
    assert_eq!(snapshot(), full);
    assert_eq!(
        dry(
            &f,
            &agent(OTHER),
            json!({"flow_id":"watch","params":{"note":"other"},"trigger":{}})
        )
        .unwrap()["summary"]["runs"][0]["result"],
        "other"
    );
    assert_eq!(snapshot(), full);
}

#[test]
fn template_dry_run_at_fires_once_immediately_with_canonical_timestamp() {
    let f = setup("template-dry-at", true);
    let mut m = manifest();
    m["params"] = json!({"at":{"type":"timestamp","max_ahead":"1d"}});
    m["trigger"] = json!({"schedule":{"at":"$params.at"}});
    register(&f, &m, "return {params, trigger, now: Date.now()};");
    let reply = dry(
        &f,
        &agent(OWNER),
        json!({"flow_id":"watch","params":{"at":"2026-05-01T01:00:00.000+00:00"},"window":"1h"}),
    )
    .unwrap();
    let runs = reply["summary"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "{reply:#}");
    assert_eq!(runs[0]["state"], "succeeded", "{reply:#}");
    assert_eq!(runs[0]["result"]["params"]["at"], "2026-05-01T01:00:00Z");
    assert_eq!(runs[0]["trigger"]["due"], "2026-05-01T00:00:00Z");
    assert_eq!(runs[0]["trigger"]["dry_run"], true);
    assert_eq!(runs[0]["result"]["now"], f.clock.now_ms());
    assert_eq!(f.clock.now_ms(), 1_777_593_600_000);
    let instance = create(&f, OWNER, json!({"at":"2026-05-01T01:00:00Z"}), 1, "1d").unwrap();
    f.clock.advance(7_200_000);
    let replay = dry(
        &f,
        &Caller::Operator,
        json!({"flow_id":id(&instance),"window":"1h"}),
    )
    .unwrap();
    assert_eq!(replay["summary"]["runs"].as_array().unwrap().len(), 1);
    assert_eq!(
        replay["summary"]["runs"][0]["result"]["params"],
        instance["params"]
    );
    assert_eq!(
        replay["summary"]["runs"][0]["result"]["now"],
        f.clock.now_ms()
    );
    assert_eq!(replay["summary"]["runs"][0]["trigger"]["dry_run"], true);
    assert!(
        !f.module
            .rt
            .store()
            .read(|c| Ok(c.query_row(
                "SELECT removed FROM flows WHERE flow_id=?1",
                [id(&instance)],
                |r| r.get::<_, bool>(0)
            )?))
            .unwrap()
    );
}

#[test]
fn template_dry_run_captures_self_end_with_values_and_bound_instance() {
    let f = setup("template-dry-self-end", true);
    register(
        &f,
        &manifest(),
        "return await instance.end().catch(e => e.data.code);",
    );
    let before = f.module.rt.live_instances().unwrap();
    let reply = dry(
        &f,
        &agent(OWNER),
        json!({"flow_id":"watch","params":{"note":"preview"},"trigger":{}}),
    )
    .unwrap();
    assert_captured_end(&reply);
    assert_eq!(f.module.rt.live_instances().unwrap(), before);

    let instance = create(&f, OWNER, json!({"note":"real"}), 1, "1h").unwrap();
    let before = f.module.rt.live_instances().unwrap();
    let reply = dry(
        &f,
        &agent(OWNER),
        json!({"flow_id":id(&instance),"trigger":{}}),
    )
    .unwrap();
    assert_captured_end(&reply);
    assert_eq!(f.module.rt.live_instances().unwrap(), before);
    assert!(
        f.module
            .rt
            .pending_template_events(32)
            .unwrap()
            .iter()
            .all(|e| e.event_name != "flow_instance_ended")
    );
    assert_eq!(f.mock.total_sends(), 0);
}

fn assert_captured_end(reply: &Value) {
    assert_eq!(reply["summary"]["mode"], "capture");
    assert_eq!(reply["summary"]["partial"], false);
    let runs = reply["summary"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["state"], "succeeded", "{reply:#}");
    assert_eq!(runs[0]["result"], "captured", "{reply:#}");
    let calls = runs[0]["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["kind"], "instance.end");
    assert_eq!(calls[0]["action"], "captured");
    assert_eq!(calls[0]["outcome"]["rejected"], "captured");
}

#[test]
fn ordinary_flow_dry_run_still_refuses_self_end() {
    let f = setup("ordinary-dry-self-end", true);
    common::install_approved(
        &f,
        &Caller::Operator,
        "return await instance.end().catch(e => e.data.code);",
        &events_manifest("ordinary"),
    );
    let reply = dry(
        &f,
        &Caller::Operator,
        json!({"flow_id":"ordinary","trigger":{}}),
    )
    .unwrap();
    let run = &reply["summary"]["runs"][0];
    assert_eq!(run["state"], "succeeded", "{reply:#}");
    assert_eq!(run["result"], "not_a_template_instance");
    assert_eq!(run["calls"][0]["kind"], "instance.end");
    assert_eq!(run["calls"][0]["action"], "refused");
    assert_eq!(
        run["calls"][0]["outcome"]["rejected"],
        "not_a_template_instance"
    );
    assert_eq!(f.mock.total_sends(), 0);
}

#[test]
fn instance_dry_run_uses_bound_values_owner_grants_and_approved_version() {
    let f = setup("instance-dry-bound", true);
    let target = f.dir.join("target");
    std::fs::write(&target, "bound").unwrap();
    let root = common::canonical(&f.dir);
    let mut m = manifest();
    m["params"]["path"] = json!({"type":"path","under":[root]});
    m["fs"] = json!({"read":["$params.path"]});
    register(
        &f,
        &m,
        "return {self, params, read: await fs.read(params.path).catch(e => e.data.code)};",
    );
    let instance = create(&f, OWNER, json!({"note":"bound","path":target}), 1, "1h").unwrap();
    m["version"] = json!(2);
    register(&f, &m, "return 'newest';");
    for caller in [agent(OWNER), Caller::Operator] {
        let reply = dry(&f, &caller, json!({"flow_id":id(&instance),"trigger":{}})).unwrap();
        assert_eq!(reply["version"], 1);
        assert_eq!(
            reply["summary"]["runs"][0]["result"],
            json!({"self":{"agent_id":OWNER},"params":instance["params"],"read":"captured"}),
            "{reply:#}"
        );
    }
    assert_eq!(
        dry(
            &f,
            &agent(OWNER),
            json!({"flow_id":"watch","params":instance["params"],"trigger":{}})
        )
        .unwrap()["version"],
        2
    );
    assert_eq!(
        dry(
            &f,
            &agent(OWNER),
            json!({"flow_id":id(&instance),"version":2,"trigger":{}})
        )
        .unwrap()["summary"]["runs"][0]["result"],
        "newest"
    );
    assert_eq!(
        f.module
            .rt
            .flow(id(&instance))
            .unwrap()
            .unwrap()
            .approved_version,
        Some(1)
    );
}

#[test]
fn instance_dry_run_refuses_params_including_null() {
    let f = setup("instance-dry-params", false);
    register(&f, &manifest(), "return params;");
    let instance = create(&f, OWNER, json!({"note":"fixed"}), 1, "1h").unwrap();
    for values in [
        json!({"note":"fixed"}),
        json!({"note":"replacement"}),
        Value::Null,
    ] {
        assert_eq!(
            dry(
                &f,
                &agent(OWNER),
                json!({"flow_id":id(&instance),"params":values})
            )
            .unwrap_err()
            .code,
            "params_not_allowed"
        );
    }
}

#[test]
fn template_dry_run_requires_agent_and_capture_instance_requires_owner_or_operator() {
    let f = setup("template-dry-authority", false);
    register(&f, &manifest(), "return params;");
    let instance = create(&f, OWNER, json!({"note":"fixed"}), 1, "1h").unwrap();
    assert_eq!(
        dry(
            &f,
            &Caller::Operator,
            json!({"flow_id":"watch","params":{"note":"a"}})
        )
        .unwrap_err()
        .code,
        "not_permitted"
    );
    assert_eq!(
        dry(&f, &agent(OTHER), json!({"flow_id":id(&instance)}))
            .unwrap_err()
            .code,
        "not_permitted"
    );
    for (name, caller, params) in [
        ("watch", agent(OWNER), json!({"note":"a"})),
        (id(&instance), Caller::Operator, Value::Null),
    ] {
        let mut request = json!({"flow_id":name,"mode":"live"});
        if name == "watch" {
            request["params"] = params;
        }
        assert_eq!(dry(&f, &caller, request).unwrap_err().code, "not_permitted");
    }
    let mut ordinary = events_manifest("ordinary");
    ordinary["sinks"][0]["agent"] = json!(OWNER);
    f.module
        .handle(
            &agent(OWNER),
            "flow.install",
            json!({"script":"return 1;","manifest":ordinary.to_string()}),
        )
        .unwrap();
    for values in [json!({}), Value::Null] {
        assert_eq!(
            dry(
                &f,
                &Caller::Operator,
                json!({"flow_id":"ordinary","params":values})
            )
            .unwrap_err()
            .code,
            "params_require_package"
        );
    }
}

#[test]
#[cfg(unix)]
fn template_dry_run_path_and_repo_canonicalization_and_bounds_match_instances() {
    use std::os::unix::fs::symlink;
    let f = setup("template-dry-path-repo", true);
    let inside = f.dir.join("inside");
    let outside = f.dir.join("outside");
    std::fs::create_dir(&inside).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let repo = inside.join("repo");
    std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
    std::fs::create_dir_all(repo.join(".git/refs")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    symlink(&repo, inside.join("alias")).unwrap();
    symlink(&outside, inside.join("escape")).unwrap();
    let mut m = manifest();
    m["params"] =
        json!({"repo":{"type":"repo","under":[inside]},"path":{"type":"path","under":[inside]}});
    m["git"] = json!({"read":["$params.repo"]});
    m["fs"] = json!({"read":["$params.path"]});
    register(&f, &m, "return params;");
    let values = json!({"repo":inside.join("alias"),"path":inside.join("alias/future")});
    let reply = dry(
        &f,
        &agent(OWNER),
        json!({"flow_id":"watch","params":values,"trigger":{}}),
    )
    .unwrap();
    let canonical = std::fs::canonicalize(repo).unwrap();
    assert_eq!(
        reply["summary"]["runs"][0]["result"],
        json!({"repo":canonical,"path":canonical.join("future")})
    );
    for values in [
        json!({"repo":inside,"path":canonical}),
        json!({"repo":canonical,"path":inside.join("escape/future")}),
    ] {
        let error = dry(
            &f,
            &agent(OWNER),
            json!({"flow_id":"watch","params":values}),
        )
        .unwrap_err();
        assert_eq!(error.code, "param_out_of_bounds");
        assert_eq!(
            error.detail,
            create(&f, OWNER, values, 1, "1h").unwrap_err().detail
        );
    }
}
