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
    m["ops"] = json!([{"module":"prefrontal-core","op":"board.read"}]);
    m["board_read"] = json!([OWNER_NAME]);
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
    let mut boards = self_manifest("foreign-board-read");
    boards["board_read"] = json!([OWNER_NAME, FOREIGN]);
    vec![
        ("sinks", sinks),
        ("status", status),
        ("claims", claims),
        ("facts.targets", facts),
        ("board_read", boards),
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
    f.catalog.set_op(
        "prefrontal-core",
        "board.read",
        basal_host::OpDecl {
            kind: Some(basal_host::OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
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
                basal_core::install::record(
                    tx,
                    &parsed,
                    &request,
                    basal_core::install::Validation {
                        warnings: Vec::new(),
                        tool_policies: Default::default(),
                    },
                    T0,
                )
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
    f.catalog.set_op(
        "prefrontal-core",
        "board.read",
        basal_host::OpDecl {
            kind: Some(basal_host::OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
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
    by_id["board_read"] = json!([OWNER]);
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

fn install_attempt(
    f: &Fixture,
    caller: &Caller,
    id: &str,
    target: &str,
) -> Result<Value, basal_module::ops::OpError> {
    let mut m = events_manifest(id);
    m["sinks"][0]["agent"] = json!(target);
    f.module.handle(
        caller,
        "flow.install",
        json!({ "script": SCRIPT, "manifest": m.to_string() }),
    )
}

#[test]
fn install_refuses_a_failed_agent_registry_read_as_retryable_and_records_nothing() {
    use basal_host::transport::WireError;
    for failure in [
        WireError::NeverSent("core restarting".into()),
        WireError::TimedOut("agent.list timed out".into()),
        WireError::Unreadable("agent.list reply undecodable".into()),
    ] {
        let f = fixture("agent-target-registry-down", Options::default());
        f.catalog.add_named_agent(OWNER, OWNER_NAME);
        f.catalog.fail_agent_registry(Some(failure.clone()));
        // An agent-owned install resolves its targets to compare them with the
        // author; a global install only checks that they exist.
        for (caller, id) in [
            (agent(OWNER), "registry-down-agent"),
            (Caller::Operator, "registry-down-operator"),
        ] {
            let error = install_attempt(&f, &caller, id, OWNER_NAME).unwrap_err();
            assert_eq!(error.code, "agent_registry_unavailable", "{failure:?}");
            assert_eq!(error.detail, Some(json!({ "retryable": true })));
            assert_no_install(&f, id);
        }
        f.catalog.fail_agent_registry(None);
        let reply = install_attempt(&f, &agent(OWNER), "registry-down-agent", OWNER_NAME)
            .expect("the same install succeeds once the registry answers");
        assert_eq!(reply["state"], "pending");
    }
}

#[test]
fn install_refuses_an_agent_absent_from_an_answered_registry_as_unknown() {
    let f = fixture("agent-target-registry-absent", Options::default());
    f.catalog.add_named_agent(OWNER, OWNER_NAME);
    for (caller, id) in [
        (agent(OWNER), "registry-absent-agent"),
        (Caller::Operator, "registry-absent-operator"),
    ] {
        let error = install_attempt(&f, &caller, id, "MISSING").unwrap_err();
        assert_eq!(error.code, "install_refused", "{error}");
        assert!(error.message.contains("UnknownAgent"), "{error}");
        assert_eq!(error.detail, None);
        assert_no_install(&f, id);
    }
}
