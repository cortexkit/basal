mod common;

use basal_core::{Actor, packages::instance_id};
use basal_module::caller::Caller;
use common::{Fixture, Options, agent, events_manifest, fixture};
use serde_json::{Value, json};

const OWNER: &str = "agent_47120287c700722b";
fn setup(tag: &str) -> Fixture {
    let f = fixture(
        tag,
        Options {
            warm_spares: 0,
            ..Options::default()
        },
    );
    f.catalog.add_named_agent(OWNER, "Renamable");
    f
}
fn manifest(version: u32) -> Value {
    let mut m = events_manifest("dark-wake");
    m["version"] = json!(version);
    m["sinks"][0]["agent"] = json!("$self");
    m["status"] = json!(["$self"]);
    m["claims"] = json!([{"agent":"$self","source_kind":"idleness"}]);
    m["facts"] = json!({"targets":["$self"]});
    m
}
fn register(f: &Fixture, version: u32) -> Value {
    f.module
        .handle(
            &Caller::Core,
            "package.register",
            json!({"script":"return 1;","manifest":manifest(version).to_string()}),
        )
        .unwrap()
}
fn ensure(f: &Fixture, version: u32, generation: i64) -> Value {
    f.module.handle(&Caller::Core, "flow.instance.ensure", json!({"package":"dark-wake","version":version,"agent_id":OWNER,"generation":generation})).unwrap()
}
fn remove(f: &Fixture, generation: i64) -> Value {
    f.module
        .handle(
            &Caller::Core,
            "flow.instance.remove",
            json!({"package":"dark-wake","agent_id":OWNER,"generation":generation}),
        )
        .unwrap()
}

#[test]
fn instance_ids_match_all_shared_vectors() {
    for (p, a, expected) in [
        ("dark-wake", OWNER, "dark-wake_d04794834aa77fe7"),
        (
            "dark-wake",
            "agent_d1e7002e2c9b9f43",
            "dark-wake_4dc17caa47226d32",
        ),
        ("ci-watch", OWNER, "ci-watch_461d8a6c652aa677"),
    ] {
        assert_eq!(instance_id(p, a), expected);
    }
}

#[test]
fn packages_require_self_in_every_agent_field() {
    let f = setup("pkg-self-only");
    for field in ["sinks", "status", "claims", "facts"] {
        let mut m = manifest(1);
        match field {
            "sinks" => m["sinks"][0]["agent"] = json!("SYNAPSE"),
            "status" => m["status"] = json!(["SYNAPSE"]),
            "claims" => m["claims"][0]["agent"] = json!("SYNAPSE"),
            _ => m["facts"]["targets"] = json!(["SYNAPSE"]),
        }
        let e = f
            .module
            .handle(
                &Caller::Core,
                "package.register",
                json!({"script":"return 1;","manifest":m.to_string()}),
            )
            .unwrap_err();
        assert_eq!(e.code, "package_names_agent", "{field}");
    }
    assert!(f.module.rt.get_package("dark-wake", 1).is_err());
}

#[test]
fn ordinary_installs_refuse_self_in_every_agent_field() {
    let f = setup("pkg-install-self");
    for field in ["sinks", "status", "claims", "facts"] {
        let mut m = events_manifest("ordinary");
        match field {
            "sinks" => m["sinks"][0]["agent"] = json!("$self"),
            "status" => m["status"] = json!(["$self"]),
            "claims" => m["claims"] = json!([{"agent":"$self","source_kind":"idleness"}]),
            _ => m["facts"] = json!({"targets":["$self"]}),
        }
        assert_eq!(
            f.module
                .handle(
                    &Caller::Operator,
                    "flow.install",
                    json!({"script":"return 1;","manifest":m.to_string()})
                )
                .unwrap_err()
                .code,
            "self_requires_package"
        );
    }
    assert!(f.module.rt.flow("ordinary").unwrap().is_none());
}

#[test]
fn registered_versions_are_immutable_and_get_returns_exact_bytes() {
    let f = setup("pkg-immutable");
    let first = register(&f, 1);
    assert_eq!(first["new"], true);
    let again = register(&f, 1);
    assert_eq!(again["new"], false);
    assert_eq!(again["code_hash"], first["code_hash"]);
    assert_eq!(
        f.module
            .handle(
                &Caller::Operator,
                "package.register",
                json!({"script":"return 2;","manifest":manifest(1).to_string()})
            )
            .unwrap_err()
            .code,
        "package_version_conflict"
    );
    let got = f
        .module
        .handle(
            &Caller::Core,
            "package.get",
            json!({"package":"dark-wake","version":1}),
        )
        .unwrap();
    assert_eq!(
        got,
        json!({"package":"dark-wake","version":1,"script":"return 1;","manifest":manifest(1).to_string(),"code_hash":first["code_hash"]})
    );
    assert!(f.consent.undelivered().is_empty());
    assert!(f.module.rt.cards("dark-wake").unwrap().is_empty());
    f.module
        .rt
        .store()
        .read(|c| {
            assert!(
                c.execute("UPDATE package_versions SET script='changed'", [])
                    .is_err()
            );
        assert!(c.execute("DELETE FROM package_versions", []).is_err());
        assert!(c.execute("INSERT OR REPLACE INTO package_versions SELECT package,version,'changed',manifest,code_hash,registered_at FROM package_versions",[]).is_err());
            Ok(())
        })
        .unwrap();
}

#[test]
fn package_callers_are_core_only_except_operator_registration() {
    let f = setup("pkg-callers");
    register(&f, 1);
    for caller in [
        Caller::Operator,
        Caller::Local,
        agent(OWNER),
        agent("other"),
        Caller::Other("aft".into()),
        Caller::Core,
    ] {
        for (op, params) in [
            (
                "package.register",
                json!({"script":"return 1;","manifest":manifest(1).to_string()}),
            ),
            ("package.get", json!({"package":"dark-wake","version":1})),
            (
                "flow.instance.ensure",
                json!({"package":"dark-wake","version":1,"agent_id":OWNER,"generation":1}),
            ),
            (
                "flow.instance.remove",
                json!({"package":"absent","agent_id":OWNER,"generation":1}),
            ),
        ] {
            let result = f.module.handle(&caller, op, params);
            if caller == Caller::Core || (caller == Caller::Operator && op == "package.register") {
                assert!(result.is_ok(), "{caller:?} {op}: {result:?}");
            } else {
                assert_eq!(result.unwrap_err().code, "not_permitted", "{caller:?} {op}");
            }
        }
    }
}

#[test]
fn generations_fence_stale_calls_and_record_absent_removals() {
    removing_an_absent_instance_records_generation();
    remove_before_ensure_fences_delayed_routes_and_preserves_first_reply();
    let f = setup("pkg-stale");
    register(&f, 1);
    register(&f, 2);
    ensure(&f, 2, 9);
    for (op, p) in [
        (
            "flow.instance.ensure",
            json!({"package":"dark-wake","version":1,"agent_id":OWNER,"generation":8}),
        ),
        (
            "flow.instance.remove",
            json!({"package":"dark-wake","agent_id":OWNER,"generation":7}),
        ),
    ] {
        let e = f.module.handle(&Caller::Core, op, p).unwrap_err();
        assert_eq!(e.code, "generation_stale");
        assert_eq!(e.detail.unwrap()["current"], 9);
    }
    assert_eq!(
        f.module
            .rt
            .flow(&instance_id("dark-wake", OWNER))
            .unwrap()
            .unwrap()
            .approved_version,
        Some(2)
    );
}

fn removing_an_absent_instance_records_generation() {
    let f = setup("pkg-absent");
    assert_eq!(remove(&f, 10)["removed"], false);
    let stored = f.module.rt.store().read(|c| Ok(c.query_row("SELECT generation FROM instance_generations WHERE package='dark-wake' AND agent_id=?1",[OWNER],|r|r.get::<_,i64>(0))?)).unwrap();
    assert_eq!(stored, 10);
    assert!(
        f.module
            .rt
            .flow(&instance_id("dark-wake", OWNER))
            .unwrap()
            .is_none()
    );
    assert_eq!(remove(&f, 10)["removed"], false);
    register(&f, 1);
    assert_eq!(ensure(&f, 1, 11)["new"], true);
}

fn remove_before_ensure_fences_delayed_routes_and_preserves_first_reply() {
    let f = setup("pkg-delayed-route");
    register(&f, 1);
    let removed = remove(&f, 10);
    assert_eq!(
        f.module
            .handle(
                &Caller::Core,
                "flow.instance.ensure",
                json!({"package":"dark-wake","version":1,"agent_id":OWNER,"generation":9})
            )
            .unwrap_err()
            .code,
        "generation_stale"
    );
    assert!(
        f.module
            .rt
            .flow(&instance_id("dark-wake", OWNER))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.module
            .handle(
                &Caller::Core,
                "flow.instance.ensure",
                json!({"package":"dark-wake","version":1,"agent_id":OWNER,"generation":10})
            )
            .unwrap_err()
            .code,
        "generation_conflict"
    );
    assert_eq!(remove(&f, 10), removed);
    ensure(&f, 1, 11);
    assert_eq!(remove(&f, 12)["removed"], true);
}

#[test]
fn removal_and_reensure_never_change_enable_state_or_actor() {
    let f = setup("pkg-enable");
    register(&f, 1);
    register(&f, 2);
    let id = instance_id("dark-wake", OWNER);
    ensure(&f, 1, 1);
    for (index, actor) in [
        None,
        Some(Actor::Operator("operator".into())),
        Some(Actor::Agent(OWNER.into())),
        Some(Actor::Runtime),
    ]
    .into_iter()
    .enumerate()
    {
        f.module.rt.enable_flow(&id).unwrap();
        if let Some(actor) = actor {
            f.module.rt.disable_flow(&id, &actor, "stop").unwrap();
        }
        let before = f.module.rt.flow(&id).unwrap().unwrap();
        remove(&f, 2 + index as i64 * 2);
        assert_eq!(f.module.rt.flow(&id).unwrap().unwrap(), before);
        ensure(&f, 1, 3 + index as i64 * 2);
        assert_eq!(f.module.rt.flow(&id).unwrap().unwrap(), before);
    }
}

#[test]
fn generations_resend_identical_replies_and_conflicts_record_nothing() {
    let f = setup("pkg-order");
    register(&f, 1);
    register(&f, 2);
    let created = ensure(&f, 1, 1);
    assert_eq!(created["previous_version"], Value::Null);
    assert_eq!(ensure(&f, 1, 1), created);
    for params in [
        json!({"package":"dark-wake","version":2,"agent_id":OWNER,"generation":1}),
        json!({"package":"dark-wake","agent_id":OWNER,"generation":1}),
    ] {
        let op = if params.get("version").is_some() {
            "flow.instance.ensure"
        } else {
            "flow.instance.remove"
        };
        assert_eq!(
            f.module.handle(&Caller::Core, op, params).unwrap_err().code,
            "generation_conflict"
        );
    }
    assert_eq!(
        ensure(&f, 1, 2),
        json!({"flow_id":instance_id("dark-wake",OWNER),"generation":2,"new":false,"previous_version":1})
    );
    assert_eq!(ensure(&f, 2, 3)["previous_version"], 1);
    assert_eq!(ensure(&f, 1, 4)["previous_version"], 2);
    let removed = remove(&f, 5);
    assert_eq!(removed["removed"], true);
    assert_eq!(remove(&f, 5), removed);
    assert_eq!(remove(&f, 6)["removed"], false);
    assert_eq!(ensure(&f, 1, 7)["new"], false);
    assert_eq!(
        f.module
            .handle(
                &Caller::Core,
                "flow.instance.ensure",
                json!({"package":"dark-wake","version":99,"agent_id":OWNER,"generation":8})
            )
            .unwrap_err()
            .code,
        "package_unknown"
    );
    assert_eq!(remove(&f, 8)["removed"], true);
}

#[test]
fn package_length_and_unknown_agent_refusals_do_not_write() {
    let f = setup("pkg-refusals");
    let mut m = manifest(1);
    for length in [47, 63, 64, 128] {
        m["id"] = json!("p".repeat(length));
        assert_eq!(
            f.module
                .handle(
                    &Caller::Core,
                    "package.register",
                    json!({"script":"return 1;","manifest":m.to_string()})
                )
                .unwrap_err()
                .code,
            "package_id_too_long"
        );
    }
    m["id"] = json!("p".repeat(46));
    assert!(
        f.module
            .handle(
                &Caller::Operator,
                "package.register",
                json!({"script":"return 1;","manifest":m.to_string()})
            )
            .is_ok()
    );
    register(&f, 1);
    for agent in ["unknown", "Renamable"] {
        assert_eq!(
            f.module
                .handle(
                    &Caller::Core,
                    "flow.instance.ensure",
                    json!({"package":"dark-wake","version":1,"agent_id":agent,"generation":1})
                )
                .unwrap_err()
                .code,
            "agent_unknown"
        );
        assert_eq!(
            f.module
                .handle(
                    &Caller::Core,
                    "flow.instance.remove",
                    json!({"package":"dark-wake","agent_id":agent,"generation":1})
                )
                .unwrap()["removed"],
            false
        );
    }
    assert_eq!(
        f.module
            .handle(
                &Caller::Core,
                "package.get",
                json!({"package":"nope","version":1})
            )
            .unwrap_err()
            .code,
        "package_unknown"
    );
}
