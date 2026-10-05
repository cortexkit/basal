//! Agent-owned manifests act only for their author at install and dry run.

mod common;

use basal_core::{InstallRequest, Manifest};
use basal_module::caller::Caller;
use common::{Fixture, Options, T0, agent, events_manifest, fixture, install};
use serde_json::{Value, json};

const OWNER: &str = "agent_47120287c700722b";
const OWNER_NAME: &str = "SYNAPSE";
const FOREIGN: &str = "ALF";
const SCRIPT: &str = "return 1;";

fn self_manifest(id: &str) -> Value {
    let mut m = events_manifest(id);
    m["status"] = json!([OWNER_NAME]);
    m["claims"] = json!([{ "agent": OWNER_NAME, "source_kind": "idleness" }]);
    m["facts"] = json!({ "targets": [OWNER_NAME], "text": false });
    m
}

fn foreign_manifests() -> Vec<(&'static str, Value)> {
    let mut sinks = self_manifest("foreign-sinks");
    sinks["sinks"].as_array_mut().unwrap().push(json!({
        "agent": FOREIGN, "digest_max": "piggyback"
    }));
    let mut status = self_manifest("foreign-status");
    status["status"] = json!([OWNER_NAME, FOREIGN]);
    let mut claims = self_manifest("foreign-claims");
    claims["claims"].as_array_mut().unwrap().push(json!({
        "agent": FOREIGN, "source_kind": "idleness"
    }));
    let mut facts = self_manifest("foreign-facts");
    facts["facts"]["targets"] = json!([OWNER_NAME, FOREIGN]);
    vec![
        ("sinks", sinks),
        ("status", status),
        ("claims", claims),
        ("facts.targets", facts),
    ]
}

fn assert_no_install(f: &Fixture, id: &str) {
    assert!(f.module.rt.flow(id).expect("flow").is_none());
    let count = f
        .module
        .rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM installs WHERE flow_id=?1",
                [id],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .expect("install count");
    assert_eq!(count, 0, "no install row for {id}");
    assert!(f.module.rt.cards(id).expect("cards").is_empty());
    assert!(
        f.consent.cards().iter().all(|c| c.flow_id != id),
        "no card raised for {id}"
    );
}

#[test]
fn agent_owned_manifests_reject_foreign_targets_before_install_and_dry_run() {
    let f = fixture("agent-target-refusals", Options::default());
    f.catalog.add_named_agent(OWNER, OWNER_NAME);
    f.catalog.add_named_agent("agent_d1e7002e2c9b9f43", FOREIGN);
    for (field, m) in foreign_manifests() {
        let id = m["id"].as_str().unwrap();
        let error = f
            .module
            .handle(
                &agent(OWNER),
                "flow.install",
                json!({ "script": SCRIPT, "manifest": m.to_string() }),
            )
            .expect_err("a foreign agent target must be refused");
        assert_eq!(error.code, "foreign_agent_target", "{field}: {error}");
        assert!(error.message.contains(field), "{error}");
        assert!(error.message.contains(FOREIGN), "{error}");
        assert!(error.message.contains(OWNER), "{error}");
        assert_no_install(&f, id);

        // Versions installed before self-only validation may still be in the
        // store. Seed such a version without validating it so the public dry
        // run must independently check its stored author and every grant.
        let request = InstallRequest {
            script: SCRIPT.into(),
            manifest: m.to_string(),
            author: OWNER.into(),
            loop_override: false,
        };
        let parsed = Manifest::parse(&request.manifest).expect("valid manifest shape");
        f.module
            .rt
            .store()
            .write(|tx| {
                basal_core::install::record(tx, &parsed, &request, Vec::new(), T0)
                    .expect("seed legacy version");
                Ok(())
            })
            .expect("legacy install");
        for caller in [agent(OWNER), Caller::Operator] {
            let error = f
                .module
                .handle(
                    &caller,
                    "flow.dry_run",
                    json!({ "flow_id": id, "trigger": { "synthetic": true } }),
                )
                .expect_err("dry run must refuse the same foreign grant");
            assert_eq!(error.code, "foreign_agent_target", "{field}: {error}");
            assert!(error.message.contains(field), "{error}");
            assert!(error.message.contains(FOREIGN), "{error}");
            assert!(error.message.contains(OWNER), "{error}");
        }
        let run_count = f
            .module
            .rt
            .store()
            .read(|c| {
                Ok(
                    c.query_row("SELECT COUNT(*) FROM runs WHERE flow_id=?1", [id], |r| {
                        r.get::<_, i64>(0)
                    })?,
                )
            })
            .expect("run count");
        assert_eq!(run_count, 0, "no real run for {id}");
        assert!(f.module.rt.cards(id).expect("cards").is_empty());
        assert!(
            f.consent.cards().is_empty(),
            "no legacy dry run raises a card"
        );
    }
    assert_eq!(f.mock.total_sends(), 0, "refused grants never reach a host");
}

#[test]
fn self_only_agents_and_global_and_local_manifests_still_install() {
    let f = fixture("agent-target-allowed", Options::default());
    f.catalog.add_named_agent(OWNER, OWNER_NAME);
    f.catalog.add_named_agent("agent_d1e7002e2c9b9f43", FOREIGN);
    for caller in [agent(OWNER), Caller::Operator] {
        let id = if caller == Caller::Operator {
            "operator-for-owner"
        } else {
            "self-only"
        };
        let m = self_manifest(id);
        let reply = f
            .module
            .handle(
                &caller,
                "flow.install",
                json!({ "script": SCRIPT, "manifest": m.to_string(), "author": OWNER }),
            )
            .expect("self-only grants install even when the operator authors for the agent");
        assert_eq!(reply["state"], "pending");
        assert!(f.consent.card(reply["card_id"].as_str().unwrap()).is_some());
        let dry = f
            .module
            .handle(
                &agent(OWNER),
                "flow.dry_run",
                json!({ "flow_id": id, "trigger": { "synthetic": true } }),
            )
            .expect("self-only dry run");
        assert_eq!(dry["summary"]["runs"][0]["state"], "succeeded");
    }

    let mut by_id = self_manifest("self-by-id");
    by_id["sinks"][0]["agent"] = json!(OWNER);
    by_id["status"] = json!([OWNER]);
    by_id["claims"][0]["agent"] = json!(OWNER);
    by_id["facts"]["targets"] = json!([OWNER]);
    let reply = install(&f, &agent(OWNER), SCRIPT, &by_id);
    assert_eq!(reply["state"], "pending");
    f.module
        .handle(
            &agent(OWNER),
            "flow.dry_run",
            json!({
                "flow_id": "self-by-id", "trigger": { "synthetic": true }
            }),
        )
        .expect("stable-id grants also dry run");

    // Each foreign-field manifest is unchanged for global and local callers;
    // local cards have a digest sink through which core can route approval.
    for (field, m) in foreign_manifests() {
        for caller in [Caller::Operator, Caller::Local] {
            let mut m = m.clone();
            let id = format!("{}-{}", m["id"].as_str().unwrap(), caller.label());
            m["id"] = json!(id);
            let reply = install(&f, &caller, SCRIPT, &m);
            assert_eq!(reply["state"], "pending", "{field}");
            assert!(f.consent.card(reply["card_id"].as_str().unwrap()).is_some());
            assert!(reply["dry_run"].get("error").is_none(), "{reply}");
            if caller == Caller::Operator {
                let dry = f
                    .module
                    .handle(
                        &caller,
                        "flow.dry_run",
                        json!({ "flow_id": id, "trigger": { "synthetic": true } }),
                    )
                    .expect("global dry run retains foreign grants");
                assert_eq!(dry["summary"]["runs"][0]["state"], "succeeded");
            }
        }
    }
}

#[test]
fn plain_install_cannot_occupy_package_instance_ids() {
    let f = fixture("instance-id-reservation", Options::default());
    for id in [
        "dark-wake_d04794834aa77fe7",
        "ci-watch_461d8a6c652aa677",
        "flow_0000000000000000",
        "flow_deadbeefdeadbeef",
    ] {
        for caller in [agent(OWNER_NAME), Caller::Operator, Caller::Local] {
            let error = f
                .module
                .handle(
                    &caller,
                    "flow.install",
                    json!({ "script": SCRIPT, "manifest": events_manifest(id).to_string() }),
                )
                .expect_err("package instance ids are reserved for every plain caller");
            assert_eq!(error.code, "flow_id_reserved", "{error}");
            assert!(error.message.contains(id), "{error}");
            assert_no_install(&f, id);
        }
    }
    // Only an exact trailing underscore-plus-sixteen-hex suffix is reserved.
    for id in [
        "flow_000000000000000",
        "flow_00000000000000000",
        "flow_000000000000000g",
        "flow_0000000000000000_more",
        "flow-0000000000000000",
        "0000000000000000",
    ] {
        let reply = install(&f, &agent(OWNER_NAME), SCRIPT, &events_manifest(id));
        assert_eq!(reply["state"], "pending", "{id}");
    }
}
