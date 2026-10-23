mod common;

use basal_host::transport::WireError;
use basal_module::caller::Caller;
use common::{Fixture, Options, T0, agent, events_manifest, fixture, install, install_approved};
use rusqlite::params;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const OWNER: &str = "agent_phone_owner";
const TARGET: &str = "agent_phone_target";
const OTHER: &str = "agent_phone_other";
const WORKSPACE: &str = "workspace_phone_synthetic";

fn setup(tag: &str) -> Fixture {
    let f = fixture(
        tag,
        Options {
            dry_run_max_fires: Some(0),
            ..Options::default()
        },
    );
    for (id, name) in [(OWNER, "Owner"), (TARGET, "Target"), (OTHER, "Other")] {
        f.catalog.add_named_agent(id, name);
    }
    f.catalog.name_workspace(WORKSPACE, "Synthetic workspace");
    f.catalog.place_agent(TARGET, WORKSPACE);
    f
}

fn op(f: &Fixture, method: &str, params: Value) -> Value {
    f.module.handle(&Caller::Operator, method, params).unwrap()
}

fn manifest(id: &str, audience: Option<Value>) -> Value {
    let mut m = events_manifest(id);
    m["title"] = json!("Synthetic flow");
    m["sinks"][0]["agent"] = json!(if audience.is_some() {
        "$audience"
    } else {
        "Target"
    });
    if let Some(audience) = audience {
        m["audience"] = audience;
    }
    m
}

// Seed stored history with fixed synthetic identifiers. The response itself is
// always produced by Module::handle, including the checked-in phone vectors.
fn seed(f: &Fixture, id: &str, audience: Option<Value>, author: &str) {
    let m = manifest(id, audience);
    f.module.rt.store().write(|tx| {
        tx.execute("INSERT INTO flows(flow_id,owner,state,approved_version,created_at) VALUES (?1,?2,'enabled',1,?3)",params![id,author,T0])?;
        tx.execute("INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,installed_at,approved_at,approval_ref,approval_kind)
            VALUES (?1,1,zeroblob(32),?2,'return 1;',?3,'approved',?4,?5,'synthetic-card','card')",params![id,m.to_string(),author,T0,T0+1])?;
        Ok(())
    }).unwrap();
}

#[allow(clippy::too_many_arguments)]
fn seed_run(
    f: &Fixture,
    flow: &str,
    id: &str,
    seq: i64,
    state: &str,
    ended: Option<i64>,
    trigger: Value,
    error: Option<&str>,
) {
    f.module.rt.store().write(|tx| {
        tx.execute("INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at,ended_at,admit_seq,error_kind,error_detail)
            VALUES (?1,?2,?1,1,?3,'SECRET SCRIPT','{}',zeroblob(32),?4,?5,?6,?7,?8,?9)",
            params![id,flow,trigger.to_string(),state,T0+seq,ended,seq,error.map(|_|"script"),error])?;
        Ok(())
    }).unwrap();
}

fn ids(reply: &Value) -> Vec<&str> {
    reply["flows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["flow_id"].as_str().unwrap())
        .collect()
}

#[test]
fn phone_overview_filters_reach_and_registry_failure() {
    let f = setup("phone-filters");
    seed(&f, "a-global", Some(json!({"kind":"global"})), "operator");
    seed(
        &f,
        "b-workspace",
        Some(json!({"kind":"workspace","id":WORKSPACE})),
        "operator",
    );
    seed(
        &f,
        "c-agent",
        Some(json!({"kind":"agent","id":TARGET})),
        "operator",
    );
    seed(
        &f,
        "d-owned",
        Some(json!({"kind":"agent","id":OTHER})),
        TARGET,
    );
    seed(&f, "e-static", None, "operator");
    seed(
        &f,
        "f-other",
        Some(json!({"kind":"agent","id":OTHER})),
        OTHER,
    );
    assert_eq!(
        ids(&op(&f, "flow.overview", json!({"agent_id":TARGET}))),
        ["a-global", "b-workspace", "c-agent", "d-owned", "e-static"]
    );
    assert_eq!(
        ids(&op(&f, "flow.overview", json!({"workspace_id":WORKSPACE}))),
        ["b-workspace"]
    );
    assert_eq!(
        ids(&op(
            &f,
            "flow.overview",
            json!({"agent_id":TARGET,"workspace_id":WORKSPACE})
        )),
        ["b-workspace"]
    );
    assert_eq!(
        ids(&op(
            &f,
            "flow.overview",
            json!({"agent_id":OTHER,"workspace_id":WORKSPACE})
        )),
        Vec::<&str>::new()
    );
    assert_eq!(
        ids(&op(
            &f,
            "flow.overview",
            json!({"agent_id":"agent_unknown"})
        )),
        ["a-global"]
    );
    let full = op(&f, "flow.overview", json!({}));
    assert_eq!(full["as_of_ms"], T0);
    assert_eq!(full["flows"][1]["audience"]["name"], "Synthetic workspace");
    assert!(full.get("workspace_membership").is_none());
    f.catalog
        .fail_memberships(Some(WireError::NeverSent("offline".into())));
    let partial = op(&f, "flow.overview", json!({"agent_id":TARGET}));
    assert_eq!(partial["workspace_membership"], "unavailable");
    assert_eq!(
        ids(&partial),
        ["a-global", "c-agent", "d-owned", "e-static"]
    );
    seed(
        &f,
        "g-unreadable-workspace",
        Some(json!({"kind":"workspace","id":"workspace_unknown"})),
        "operator",
    );
    let full = op(&f, "flow.overview", json!({}));
    assert_eq!(
        full["flows"][6]["audience"],
        json!({"kind":"workspace","id":"workspace_unknown","name":null})
    );
}

#[test]
fn phone_details_fields_precedence_history_and_limits() {
    let f = setup("phone-details");
    seed(&f, "flow-details", None, OWNER);
    seed_run(
        &f,
        "flow-details",
        "run-old",
        9,
        "failed",
        Some(T0 + 90),
        json!({"kind":"schedule"}),
        Some("first\nerror"),
    );
    seed_run(
        &f,
        "flow-details",
        "run-new",
        10,
        "succeeded",
        Some(T0 + 80),
        json!({"event_notice":{"subject":"ck.synthetic.event.plexus.pull_request_review.v1"}}),
        None,
    );
    seed_run(
        &f,
        "flow-details",
        "run-wait",
        11,
        "needs_reconcile",
        None,
        Value::Null,
        Some(&"é".repeat(150)),
    );
    let detail = op(&f, "flow.details", json!({"flow_id":"flow-details"}));
    assert_eq!(detail["state"], "needs_reconcile");
    assert_eq!(detail["title"], "Synthetic flow");
    assert_eq!(detail["purpose"], "A test flow.");
    assert_eq!(detail["audience"], Value::Null);
    assert_eq!(detail["owner"], OWNER);
    assert_eq!(detail["version"], 1);
    assert_eq!(detail["disabled"], Value::Null);
    assert_eq!(detail["trigger"], "on plexus pull_request_review");
    assert_eq!(detail["manifest"], manifest("flow-details", None));
    assert_eq!(
        detail["last_run"],
        json!({"at_ms":T0+90,"outcome":"failed"})
    );
    assert_eq!(
        detail["installs"][0],
        json!({"version":1,"code_hash":"00".repeat(32),"requested_by":OWNER,"requested_at_ms":T0,"approval":{"kind":"card"},"state":"approved","decided_at_ms":T0+1})
    );
    assert_eq!(detail["runs"][0]["run_id"], "run-wait");
    assert_eq!(detail["runs"][0]["trigger"], "manual");
    assert_eq!(detail["runs"][0]["admitted_at_ms"], T0 + 11);
    assert_eq!(detail["runs"][0]["ended_at_ms"], Value::Null);
    assert_eq!(detail["runs"][0]["error_kind"], "script");
    assert!(detail["runs"][0]["error"].as_str().unwrap().len() <= 200);
    assert_eq!(detail["runs"][1]["trigger"], "pull_request_review");
    assert_eq!(detail["runs"][2]["trigger"], "schedule");
    assert_eq!(detail["runs"][2]["error"], "first error");
    assert!(!detail.to_string().contains("SECRET SCRIPT"));
    assert!(detail["runs"][0].get("output").is_none());
    assert!(detail["runs"][0].get("started_at_ms").is_none());
    assert_eq!(
        op(
            &f,
            "flow.details",
            json!({"flow_id":"flow-details","runs":1})
        )["runs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for n in [0, 51] {
        assert_eq!(
            f.module
                .handle(
                    &Caller::Operator,
                    "flow.details",
                    json!({"flow_id":"flow-details","runs":n})
                )
                .unwrap_err()
                .code,
            "invalid_params"
        );
    }
    op(
        &f,
        "flow.disable",
        json!({"flow_id":"flow-details","reason":"phone pause"}),
    );
    let disabled = op(&f, "flow.details", json!({"flow_id":"flow-details"}));
    assert_eq!(disabled["state"], "disabled");
    assert_eq!(
        disabled["disabled"],
        json!({"by":"operator","reason":"phone pause"})
    );
    op(&f, "flow.uninstall", json!({"flow_id":"flow-details"}));
    assert!(ids(&op(&f, "flow.overview", json!({}))).is_empty());
    assert_eq!(
        op(&f, "flow.overview", json!({"include_removed":true}))["flows"][0]["state"],
        "removed"
    );
}

#[test]
fn phone_pending_titles_and_all_install_decisions_are_truthful() {
    let f = setup("phone-install-history");
    seed(&f, "flow-history", None, OWNER);
    f.module.rt.store().write(|tx| {
        let mut old = manifest("flow-history",None);
        old.as_object_mut().unwrap().remove("title");
        tx.execute("UPDATE installs SET manifest=?1 WHERE flow_id='flow-history'",[old.to_string()])?;
        for (version,state,author) in [(2,"declined","operator"),(3,"expired","operator"),(4,"rejected","operator"),(5,"stale","operator"),(6,"pending","local:unverified")] {
            let mut m = manifest("flow-history",None);
            m["version"] = json!(version);
            m["title"] = json!("Pending title");
            tx.execute("INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,installed_at) VALUES ('flow-history',?1,zeroblob(32),?2,'return 1;',?3,'validated',?4)",params![version,m.to_string(),author,T0+version])?;
            tx.execute("INSERT INTO install_cards(card_id,flow_id,version,code_hash,author,card,state,created_at,decided_by,decided_at) VALUES (?1,'flow-history',?2,zeroblob(32),?3,'{}',?4,?5,?6,?7)",params![format!("synthetic-{version}"),version,author,state,T0+version,if state=="pending" {None} else {Some("operator")},if state=="pending" {None} else {Some(T0+version+1)}])?;
        }
        tx.execute("UPDATE installs SET approval_kind='policy',approval_rule='synthetic-rule' WHERE version=1 AND flow_id='flow-history'",[])?;
        Ok(())
    }).unwrap();
    let detail = op(&f, "flow.details", json!({"flow_id":"flow-history"}));
    assert_eq!(detail["title"], "Pending title");
    assert_eq!(detail["version"], 1);
    assert_eq!(detail["owner"], OWNER);
    let installs = detail["installs"].as_array().unwrap();
    assert_eq!(
        installs
            .iter()
            .map(|v| v["state"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "pending", "stale", "rejected", "expired", "declined", "approved"
        ]
    );
    assert_eq!(installs[0]["requested_by"], "local");
    assert_eq!(installs[1]["requested_by"], "operator");
    assert!(installs[0].get("decided_at_ms").is_none());
    assert_eq!(installs[0]["approval"], json!({"kind":"card"}));
    assert_eq!(
        installs[5]["approval"],
        json!({"kind":"policy","rule":"synthetic-rule"})
    );
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute(
                "UPDATE flows SET approved_version=NULL,owner=NULL WHERE flow_id='flow-history'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let pending = op(&f, "flow.details", json!({"flow_id":"flow-history"}));
    assert_eq!(pending["state"], "pending_approval");
    assert_eq!(pending["version"], 6);
    assert_eq!(pending["owner"], Value::Null);
    assert_eq!(pending["manifest"]["version"], 6);
    f.module.rt.store().write(|tx| {
        tx.execute("UPDATE installs SET manifest=json_remove(manifest,'$.title') WHERE flow_id='flow-history'",[])?;
        Ok(())
    }).unwrap();
    assert_eq!(
        op(&f, "flow.details", json!({"flow_id":"flow-history"}))["title"],
        "flow-history"
    );
}

#[test]
fn phone_uninstall_stops_events_and_reinstall_does_not_replay_backlog() {
    let f = setup("phone-uninstall");
    let m = manifest("flow-uninstall", None);
    install_approved(&f, &Caller::Operator, "return 1;", &m);
    let admitted = common::admit(&f, "flow-uninstall", "in-flight");
    let notice = basal_core::events::Notice {
        subject: "ck.synthetic.event.plexus.pull_request_review.v1".into(),
        event_key: "a".repeat(64),
        digest: "b".repeat(64),
        headers: BTreeMap::new(),
        published_at_ms: T0 + 1,
    };
    f.module.rt.store().write(|tx| {
        tx.execute("INSERT INTO event_receipts(flow_id,subject,event_key,notice,state,received_at) VALUES ('flow-uninstall',?1,?2,?3,'backlog',?4)",params![notice.subject,notice.event_key,serde_json::to_string(&notice).unwrap(),T0+1])?;
        Ok(())
    }).unwrap();
    assert_eq!(
        op(&f, "flow.uninstall", json!({"flow_id":"flow-uninstall"})),
        json!({"flow_id":"flow-uninstall","removed":true,"changed":true})
    );
    assert_eq!(
        op(&f, "flow.uninstall", json!({"flow_id":"flow-uninstall"}))["changed"],
        false
    );
    assert_eq!(
        f.module
            .handle(
                &Caller::Operator,
                "flow.enable",
                json!({"flow_id":"flow-uninstall"})
            )
            .unwrap_err()
            .code,
        "flow_removed"
    );
    assert_eq!(
        f.module
            .rt
            .admit_events(std::slice::from_ref(&notice))
            .unwrap()
            .matched,
        0
    );
    assert_eq!(f.module.rt.drain_event_backlog().unwrap(), 0);
    f.module.engine.run_until_idle(20).unwrap();
    assert_eq!(
        f.module.rt.run(&admitted).unwrap().state,
        basal_core::RunState::Cancelled
    );
    let mut m2 = m.clone();
    m2["version"] = json!(2);
    let pending = install(&f, &Caller::Operator, "return 2;", &m2);
    assert_eq!(
        op(&f, "flow.details", json!({"flow_id":"flow-uninstall"}))["state"],
        "removed"
    );
    assert!(f.consent.decide(
        pending["card_id"].as_str().unwrap(),
        basal_host::CardDecision::Approve,
        "operator"
    ));
    let detail = op(&f, "flow.details", json!({"flow_id":"flow-uninstall"}));
    assert_eq!(detail["state"], "enabled");
    assert_eq!(detail["version"], 2);
    assert_eq!(f.module.rt.drain_event_backlog().unwrap(), 0);
    let (state, audit, count): (String, Option<String>, i64) = f
        .module
        .rt
        .store()
        .read(|c| {
            Ok((
                c.query_row(
                    "SELECT state FROM event_receipts WHERE flow_id='flow-uninstall'",
                    [],
                    |r| r.get(0),
                )?,
                c.query_row(
                    "SELECT removed_by FROM flows WHERE flow_id='flow-uninstall'",
                    [],
                    |r| r.get(0),
                )?,
                c.query_row(
                    "SELECT COUNT(*) FROM runs WHERE flow_id='flow-uninstall'",
                    [],
                    |r| r.get(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!(state, "refused");
    assert_eq!(audit, None);
    assert_eq!(
        count, 1,
        "queued event must never become a run after approval"
    );
    assert_eq!(f.module.rt.admit_events(&[notice]).unwrap().duplicate, 1);
}

#[test]
fn phone_uninstall_stops_schedules_and_keeps_history() {
    let f = setup("phone-schedule-remove");
    let mut m = manifest("flow-scheduled", None);
    m["trigger"] = json!({"schedule":{"interval":"5m"}});
    install_approved(&f, &Caller::Operator, "return 1;", &m);
    op(&f, "flow.uninstall", json!({"flow_id":"flow-scheduled"}));
    f.clock.set(T0 + 600_000);
    let tick = f.module.rt.scheduler().unwrap().tick().unwrap();
    assert!(tick.admitted.is_empty());
    assert_eq!(
        op(&f, "flow.details", json!({"flow_id":"flow-scheduled"}))["installs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.module
        .rt
        .store()
        .read(|c| {
            let audit: (String, i64) = c.query_row(
                "SELECT removed_by,removed_at FROM flows WHERE flow_id='flow-scheduled'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(audit, ("operator".into(), T0));
            Ok(())
        })
        .unwrap();
}

#[test]
fn phone_uninstall_ends_templates_but_refuses_core_packages() {
    let f = setup("phone-instances");
    let mut m = manifest("synthetic-package", None);
    m["sinks"][0]["agent"] = json!("$self");
    op(
        &f,
        "package.register",
        json!({"manifest":m.to_string(),"script":"return 1;"}),
    );
    let instance = f
        .module
        .handle(
            &Caller::Core,
            "flow.instance.ensure",
            json!({"package":"synthetic-package","version":1,"agent_id":TARGET,"generation":1}),
        )
        .unwrap();
    assert_eq!(
        f.module
            .handle(
                &Caller::Operator,
                "flow.uninstall",
                json!({"flow_id":instance["flow_id"]})
            )
            .unwrap_err()
            .code,
        "managed_by_core"
    );
    m["id"] = json!("synthetic-template");
    m["params"] = json!({"note":{"type":"string","max_bytes":100}});
    m["template"] = json!({"max_per_agent":2,"max_live":3,"max_lifetime":"1d"});
    op(
        &f,
        "package.register",
        json!({"manifest":m.to_string(),"script":"return 1;"}),
    );
    let instance = f.module.handle(&agent(TARGET),"flow.instantiate",json!({"template":"synthetic-template","version":1,"params":{"note":"synthetic"},"expires_in":"1h"})).unwrap();
    let detail = op(&f, "flow.details", json!({"flow_id":instance["flow_id"]}));
    assert_eq!(detail["template"], "synthetic-template");
    assert_eq!(detail["expires_at_ms"], T0 + 3_600_000);
    assert_eq!(detail["owner"], TARGET);
    assert_eq!(
        detail["installs"],
        json!([]),
        "package registry has no per-instance install request history"
    );
    op(&f, "flow.uninstall", json!({"flow_id":instance["flow_id"]}));
    assert_eq!(
        op(
            &f,
            "flow.instance.end",
            json!({"flow_id":instance["flow_id"]})
        )["ended"],
        false
    );
    assert_eq!(
        op(&f, "flow.details", json!({"flow_id":instance["flow_id"]}))["state"],
        "removed"
    );
}

#[test]
fn phone_ops_refuse_every_non_operator_and_invalid_params() {
    let f = setup("phone-refusals");
    seed(&f, "flow-refuse", None, OWNER);
    for method in ["flow.overview", "flow.details", "flow.uninstall"] {
        for caller in [
            agent(OWNER),
            Caller::Core,
            Caller::Local,
            Caller::Module {
                module_id: "broca".into(),
            },
            Caller::Other("synthetic".into()),
        ] {
            let code = if caller == Caller::Local {
                "operator_attestation_required"
            } else {
                "not_permitted"
            };
            assert_eq!(
                f.module
                    .handle(&caller, method, json!({"flow_id":"flow-refuse"}))
                    .unwrap_err()
                    .code,
                code,
                "{method} {caller:?}"
            );
        }
    }
    for method in ["flow.details", "flow.uninstall"] {
        assert_eq!(
            f.module
                .handle(&Caller::Operator, method, json!({"flow_id":"unknown"}))
                .unwrap_err()
                .code,
            "not_found"
        );
        assert_eq!(
            f.module
                .handle(
                    &Caller::Operator,
                    method,
                    json!({"flow_id":"flow-refuse","extra":true})
                )
                .unwrap_err()
                .code,
            "invalid_params"
        );
    }
    assert_eq!(
        f.module
            .handle(
                &Caller::Operator,
                "flow.overview",
                json!({"agent_id":"Target"})
            )
            .unwrap_err()
            .code,
        "invalid_params"
    );
    assert_eq!(
        f.module
            .handle(&Caller::Operator, "flow.overview", json!({"extra":true}))
            .unwrap_err()
            .code,
        "invalid_params"
    );
    for method in ["flow.overview", "flow.details", "flow.uninstall"] {
        assert!(
            basal_module::manifest::OPERATIONS
                .iter()
                .any(|(name, _, _)| *name == method)
        );
    }
}

fn refusal(f: &Fixture, caller: &Caller, method: &str, params: Value) -> Value {
    let error = f.module.handle(caller, method, params).unwrap_err();
    let mut value = json!({"error":{"code":error.code,"message":error.message}});
    if let Some(detail) = error.detail {
        value["error"]["detail"] = detail;
    }
    value
}

fn vectors() -> BTreeMap<String, Value> {
    let f = setup("phone-vectors");
    seed(
        &f,
        "flow-phone-global",
        Some(json!({"kind":"global"})),
        "operator",
    );
    seed(&f, "flow-phone-owned", None, OWNER);
    seed(
        &f,
        "flow-phone-other",
        Some(json!({"kind":"agent","id":OTHER})),
        OTHER,
    );
    seed(
        &f,
        "flow-phone-workspace",
        Some(json!({"kind":"workspace","id":WORKSPACE})),
        "operator",
    );
    seed_run(
        &f,
        "flow-phone-owned",
        "run-phone-failed",
        1,
        "failed",
        Some(T0 + 2),
        json!({"kind":"schedule"}),
        Some("Synthetic failure\nsecond line"),
    );
    let mut values = BTreeMap::new();
    values.insert("overview.json".into(), op(&f, "flow.overview", json!({})));
    values.insert(
        "overview-by-agent.json".into(),
        op(&f, "flow.overview", json!({"agent_id":TARGET})),
    );
    f.catalog
        .fail_memberships(Some(WireError::NeverSent("synthetic outage".into())));
    values.insert(
        "overview-membership-unavailable.json".into(),
        op(&f, "flow.overview", json!({"agent_id":TARGET})),
    );
    f.catalog.fail_memberships(None);
    values.insert(
        "details.json".into(),
        op(&f, "flow.details", json!({"flow_id":"flow-phone-owned"})),
    );
    values.insert(
        "uninstall.json".into(),
        op(&f, "flow.uninstall", json!({"flow_id":"flow-phone-owned"})),
    );
    values.insert(
        "uninstall-repeat.json".into(),
        op(&f, "flow.uninstall", json!({"flow_id":"flow-phone-owned"})),
    );
    for method in ["flow.overview", "flow.details", "flow.uninstall"] {
        let short = method.strip_prefix("flow.").unwrap();
        values.insert(
            format!("refusal-{short}-not-permitted.json"),
            refusal(
                &f,
                &agent(OWNER),
                method,
                json!({"flow_id":"flow-phone-owned"}),
            ),
        );
        values.insert(
            format!("refusal-{short}-operator-attestation-required.json"),
            refusal(
                &f,
                &Caller::Local,
                method,
                json!({"flow_id":"flow-phone-owned"}),
            ),
        );
    }
    for method in ["flow.details", "flow.uninstall"] {
        values.insert(
            format!(
                "refusal-{}-not-found.json",
                method.strip_prefix("flow.").unwrap()
            ),
            refusal(
                &f,
                &Caller::Operator,
                method,
                json!({"flow_id":"flow-phone-unknown"}),
            ),
        );
    }
    values.insert(
        "refusal-enable-flow-removed.json".into(),
        refusal(
            &f,
            &Caller::Operator,
            "flow.enable",
            json!({"flow_id":"flow-phone-owned"}),
        ),
    );
    f.module.rt.store().write(|tx| {
        tx.execute("INSERT INTO flows(flow_id,owner,package,approved_version,created_at) VALUES ('flow-phone-managed',?1,'synthetic-package',1,?2)",params![TARGET,T0])?; Ok(())
    }).unwrap();
    values.insert(
        "refusal-uninstall-managed-by-core.json".into(),
        refusal(
            &f,
            &Caller::Operator,
            "flow.uninstall",
            json!({"flow_id":"flow-phone-managed"}),
        ),
    );
    values.insert(
        "refusal-details-invalid-params.json".into(),
        refusal(
            &f,
            &Caller::Operator,
            "flow.details",
            json!({"flow_id":"flow-phone-owned","runs":51}),
        ),
    );
    values
}

#[test]
fn phone_vectors_match_real_dispatch_and_sha256sums() {
    let values = vectors();
    let files: BTreeMap<_, _> = values
        .into_iter()
        .map(|(name, value)| (name, serde_json::to_string_pretty(&value).unwrap() + "\n"))
        .collect();
    if std::env::var_os("BASAL_PRINT_PHONE_VECTORS").is_some() {
        println!("PHONE_VECTORS={}", serde_json::to_string(&files).unwrap());
        return;
    }
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/flows-phone-v1");
    let mut sums = String::new();
    for (name, actual) in &files {
        let expected = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        assert_eq!(&expected, actual, "handler vector drift: {name}");
        sums.push_str(&format!(
            "{:x}  {name}\n",
            Sha256::digest(actual.as_bytes())
        ));
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap(),
        sums
    );
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut expected: Vec<_> = files.keys().cloned().chain(["SHA256SUMS".into()]).collect();
    expected.sort();
    assert_eq!(names, expected, "every vector must be exercised");
}

#[test]
fn phone_details_default_and_maximum_run_bounds() {
    let f = setup("phone-run-bounds");
    seed(&f, "flow-bounded", None, "operator");
    for n in 0..55 {
        seed_run(
            &f,
            "flow-bounded",
            &format!("run-bounded-{n}"),
            n,
            "succeeded",
            Some(T0 + n),
            Value::Null,
            None,
        );
    }
    let default = op(&f, "flow.details", json!({"flow_id":"flow-bounded"}));
    assert_eq!(default["runs"].as_array().unwrap().len(), 20);
    assert_eq!(default["runs"][0]["run_id"], "run-bounded-54");
    assert_eq!(default["runs"][19]["run_id"], "run-bounded-35");
    let max = op(
        &f,
        "flow.details",
        json!({"flow_id":"flow-bounded","runs":50}),
    );
    assert_eq!(max["runs"].as_array().unwrap().len(), 50);
    assert_eq!(max["runs"][49]["run_id"], "run-bounded-5");
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute(
                "UPDATE installs SET approval_kind=NULL WHERE flow_id='flow-bounded'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        op(&f, "flow.details", json!({"flow_id":"flow-bounded"}))["installs"][0]["approval"],
        json!({"kind":"unknown"})
    );
}
