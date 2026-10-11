use basal_core::manifest::{Manifest, ManifestError};
use basal_core::templates::{canonical_params, effective, instance_id, validate_values};
use serde_json::{Value, json};

fn base() -> Value {
    json!({"id":"watch","version":1,"purpose":"Watch a condition.",
        "trigger":{"schedule":{"interval":"1m"}},
        "params":{},"template":{"max_per_agent":2,"max_live":3,"max_lifetime":"7d"}})
}
fn parse(value: &Value) -> Result<Manifest, ManifestError> {
    Manifest::parse(&value.to_string())
}
fn with_parameter(decl: Value) -> Value {
    let mut m = base();
    m["params"] = json!({"value":decl});
    m
}

#[test]
fn parameter_names_and_count_are_bounded() {
    for name in ["", "Upper", "hy-phen", "space name", &"a".repeat(33), "é"] {
        let mut m = base();
        m["params"] = json!({name:{"type":"integer","min":0,"max":1}});
        assert!(
            matches!(parse(&m), Err(ManifestError::TemplateBoundInvalid { .. })),
            "{name}"
        );
    }
    for count in [16, 17] {
        let mut m = base();
        for n in 0..count {
            m["params"][format!("p{n}")] = json!({"type":"integer","min":0,"max":1});
        }
        assert_eq!(parse(&m).is_ok(), count == 16);
    }
    let mut m = base();
    m["params"] = json!({"a".repeat(32):{"type":"integer","min":0,"max":1}});
    assert!(parse(&m).is_ok());
}

#[test]
fn all_parameter_declarations_require_closed_valid_bounds() {
    for decl in [
        json!({"type":"path","under":[]}),
        json!({"type":"path","under":["relative"]}),
        json!({"type":"repo"}),
        json!({"type":"url","hosts":["*.example.com"]}),
        json!({"type":"url","hosts":["127.0.0.1"]}),
        json!({"type":"url","hosts":[]}),
        json!({"type":"duration","min":"1m","max":"0s"}),
        json!({"type":"integer","min":2,"max":1}),
        json!({"type":"integer","min":1}),
        json!({"type":"string","max_bytes":0}),
        json!({"type":"string","max_bytes":4097}),
        json!({"type":"enum","values":[]}),
        json!({"type":"enum","values":(0..33).map(|n|format!("v{n}")).collect::<Vec<_>>()}),
        json!({"type":"enum","values":["v","v"]}),
        json!({"type":"path","under":["/tmp","/tmp"]}),
        json!({"type":"url","hosts":["example.com","example.com"]}),
        json!({"type":"timestamp","max_ahead":"forever"}),
        json!({"type":"unknown"}),
        json!({"type":"integer","min":0,"max":1,"typo":true}),
    ] {
        assert!(
            matches!(
                parse(&with_parameter(decl.clone())),
                Err(ManifestError::TemplateBoundInvalid { .. })
            ),
            "{decl}"
        );
    }
    assert!(parse(&with_parameter(json!({"type":"string","max_bytes":4096}))).is_ok());
    assert!(
        parse(&with_parameter(
            json!({"type":"enum","values":(0..32).map(|n|format!("v{n}")).collect::<Vec<_>>()})
        ))
        .is_ok()
    );
}

#[test]
fn duration_minimum_and_interval_maximum_apply_at_registration() {
    let m = with_parameter(json!({"type":"duration","min":"59s","max":"1h"}));
    assert!(matches!(
        parse(&m),
        Err(ManifestError::TemplateBoundInvalid { .. })
    ));
    let mut m = with_parameter(json!({"type":"duration","min":"1m","max":"730d"}));
    assert!(parse(&m).is_ok());
    m["trigger"]["schedule"]["interval"] = json!("$params.value");
    assert!(matches!(
        parse(&m),
        Err(ManifestError::TemplateBoundInvalid { .. })
    ));
    m["params"]["value"]["max"] = json!("365d");
    assert!(parse(&m).is_ok());
}

#[test]
fn template_limits_and_lifetime_are_bounded_and_required() {
    for field in ["max_per_agent", "max_live", "max_lifetime"] {
        for value in match field {
            "max_per_agent" => vec![json!(0), json!(101)],
            "max_live" => vec![json!(0), json!(10001)],
            _ => vec![json!("0s"), json!("never")],
        } {
            let mut m = base();
            m["template"][field] = value;
            assert!(
                matches!(parse(&m), Err(ManifestError::TemplateBoundInvalid { .. })),
                "{m}"
            );
        }
    }
    for field in ["max_per_agent", "max_lifetime"] {
        let mut m = base();
        m["template"].as_object_mut().unwrap().remove(field);
        assert!(matches!(
            parse(&m),
            Err(ManifestError::TemplateBoundInvalid { .. })
        ));
    }
    let mut m = base();
    m.as_object_mut().unwrap().remove("template");
    assert!(matches!(
        parse(&m),
        Err(ManifestError::TemplateBoundInvalid { .. })
    ));
    let mut m = base();
    m.as_object_mut().unwrap().remove("params");
    assert!(matches!(
        parse(&m),
        Err(ManifestError::TemplateBoundInvalid { .. })
    ));
    let mut m = base();
    m["template"] = json!({"max_per_agent":100,"max_live":10000,"max_lifetime":"1d"});
    assert!(parse(&m).is_ok());
}

#[test]
fn placeholders_must_occupy_whole_strings_in_typed_slots_only() {
    let mut m = with_parameter(json!({"type":"path","under":["/tmp"]}));
    for field in ["purpose", "placement"] {
        m[field] = json!("$params.value");
        assert!(
            matches!(parse(&m),Err(ManifestError::TemplatePlaceholderInvalid { field:f }) if f==field)
        );
        m[field] = json!("ordinary");
    }
    for value in [
        "$params.value/suffix",
        "prefix $params.value",
        "$params.absent",
    ] {
        m["fs"] = json!({"read":[value]});
        assert!(
            matches!(
                parse(&m),
                Err(ManifestError::TemplatePlaceholderInvalid { .. })
            ),
            "{value}"
        );
    }
    for (decl, slot) in [
        (
            json!({"type":"string","max_bytes":100}),
            json!({"fs":{"read":["$params.value"]}}),
        ),
        (
            json!({"type":"path","under":["/tmp"]}),
            json!({"git":{"read":["$params.value"]}}),
        ),
        (
            json!({"type":"repo","under":["/tmp"]}),
            json!({"fs":{"write":["$params.value"]}}),
        ),
        (
            json!({"type":"url","hosts":["example.com"]}),
            json!({"trigger":{"schedule":{"interval":"$params.value"}}}),
        ),
    ] {
        let mut m = with_parameter(decl);
        for (key, value) in slot.as_object().unwrap() {
            m[key] = value.clone();
        }
        assert!(
            matches!(
                parse(&m),
                Err(ManifestError::TemplatePlaceholderInvalid { .. })
            ),
            "{m}"
        );
    }
}

#[test]
fn one_shot_requires_timestamp_placeholder_and_once_policy() {
    let mut m = with_parameter(json!({"type":"timestamp","max_ahead":"1d"}));
    m["trigger"] = json!({"schedule":{"at":"$params.value"}});
    assert!(parse(&m).is_ok());
    for (field, value) in [
        ("at", json!("2026-11-01T09:00:00Z")),
        ("tz", json!("UTC")),
        ("each_cap", json!(1)),
        ("missed", json!("skip")),
        ("missed", json!("each")),
    ] {
        let mut bad = m.clone();
        bad["trigger"]["schedule"][field] = value;
        assert!(
            matches!(parse(&bad),Err(ManifestError::TemplatePlaceholderInvalid { field:f }) if f==format!("trigger.schedule.{field}")),
            "{bad}"
        );
    }
    m["params"]["value"] = json!({"type":"duration","min":"1m","max":"1d"});
    assert!(matches!(
        parse(&m),
        Err(ManifestError::TemplatePlaceholderInvalid { .. })
    ));
}

#[test]
fn scalar_values_keep_spelling_and_enforce_types_and_bounds() {
    let mut m = base();
    m["params"] = json!({
        "cadence":{"type":"duration","min":"1m","max":"1h"},
        "count":{"type":"integer","min":-2,"max":2},
        "mode":{"type":"enum","values":["a","b"]},
        "note":{"type":"string","max_bytes":4096},
        "time":{"type":"timestamp","max_ahead":"1h"},
        "url":{"type":"url","hosts":["example.com"]}});
    let m = parse(&m).unwrap();
    let now = "2026-11-01T09:00:00Z"
        .parse::<jiff::Timestamp>()
        .unwrap()
        .as_millisecond();
    let valid = json!({"cadence":"060s","count":-2,"mode":"b","note":"x".repeat(4096),"time":"2026-11-01T09:30:00Z","url":"https://example.com/x"});
    assert_eq!(validate_values(&m, &valid, now).unwrap(), valid);
    for (name, value) in [
        ("cadence", json!("59s")),
        ("cadence", json!("61m")),
        ("count", json!(3)),
        ("count", json!(1.0)),
        ("count", json!(true)),
        ("mode", json!("c")),
        ("note", json!("x".repeat(4097))),
        ("note", json!("line\nline")),
        ("time", json!("2026-11-01T08:59:59Z")),
        ("time", json!("2026-11-01T10:00:01Z")),
        ("time", json!("2026-11-01T10:00:00+01:00")),
        ("url", json!("http://example.com")),
        ("url", json!("https://other.com")),
    ] {
        let mut values = valid.clone();
        values[name] = value;
        let refusal = validate_values(&m, &values, now).unwrap_err();
        assert_eq!(refusal.code, "param_out_of_bounds", "{name}");
        assert_eq!(refusal.detail["param"], name);
    }
}

#[test]
fn parameter_failures_are_reported_in_name_order_with_bounds() {
    let mut m = base();
    m["params"] =
        json!({"b":{"type":"integer","min":1,"max":2},"z":{"type":"integer","min":1,"max":2}});
    let m = parse(&m).unwrap();
    for (values, name, code) in [
        (json!({"a":1}), "a", "param_unknown"),
        (json!({"b":5}), "b", "param_out_of_bounds"),
        (json!({"b":1}), "z", "param_missing"),
        (json!({"z":9}), "b", "param_missing"),
    ] {
        let refusal = validate_values(&m, &values, 0).unwrap_err();
        assert_eq!(refusal.code, code);
        assert_eq!(refusal.detail["param"], name);
        assert_eq!(
            refusal.detail["bound"],
            if code == "param_unknown" {
                Value::Null
            } else {
                m.params.as_ref().unwrap()[name].clone()
            }
        );
    }
}

#[test]
fn effective_manifest_narrows_every_allowed_slot_and_preserves_static_grants() {
    let mut m = base();
    m["params"] = json!({"path":{"type":"path","under":["/tmp"]},"repo":{"type":"repo","under":["/tmp"]},"url":{"type":"url","hosts":["example.com"]},"interval":{"type":"duration","min":"1m","max":"1h"}});
    m["fs"] = json!({"read":["$params.path","/tmp"],"write":["$params.path"]});
    m["git"] = json!({"read":["$params.repo"]});
    m["net"] = json!({"fetch":[{"host":"$params.url"}]});
    m["trigger"]["schedule"]["interval"] = json!("$params.interval");
    let mut m = parse(&m).unwrap();
    effective(
        &mut m,
        &json!({"path":"/tmp/file","repo":"/tmp/repo","url":"https://example.com/only","interval":"2m"}),
    );
    assert_eq!(m.fs.as_ref().unwrap().read, vec!["/tmp/file", "/tmp"]);
    assert_eq!(m.fs.unwrap().write, vec!["/tmp/file"]);
    assert_eq!(m.git.unwrap().read, vec!["/tmp/repo"]);
    assert_eq!(m.net.unwrap().fetch[0].host, "example.com");
    assert_eq!(m.trigger.schedule.unwrap().interval.as_deref(), Some("2m"));
}

#[test]
fn template_ids_match_the_public_vectors() {
    let vectors: Value = serde_json::from_str(include_str!(
        "../../../test-vectors/templates/instance-ids.json"
    ))
    .unwrap();
    for vector in vectors.as_array().unwrap() {
        if let Some(at) = vector["supplied_params"]["at"].as_str() {
            let manifest = parse(&with_parameter(
                json!({"type":"timestamp","max_ahead":"1d"}),
            ))
            .unwrap();
            let now = "2026-11-01T00:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_millisecond();
            assert_eq!(
                validate_values(&manifest, &json!({"value":at}), now).unwrap()["value"],
                vector["params"]["at"]
            );
        }
        assert_eq!(
            canonical_params(&vector["params"]),
            vector["canonical_params"]
        );
        assert_eq!(
            instance_id(
                vector["template"].as_str().unwrap(),
                vector["agent_id"].as_str().unwrap(),
                &vector["params"]
            ),
            vector["flow_id"]
        );
    }
    assert_ne!(
        instance_id("watch", "agent_test", &json!({})),
        basal_core::packages::instance_id("watch", "agent_test")
    );
    assert_ne!(
        instance_id("watch", "agent_test", &json!({"text":"first"})),
        instance_id("watch", "agent_test", &json!({"text":"second"}))
    );
}

#[test]
fn template_objects_refuse_duplicate_keys_nulls_and_unknown_fields() {
    let text = base().to_string();
    for replacement in [
        "\"params\":null",
        "\"params\":{\"a\":{\"type\":\"integer\",\"min\":0,\"max\":1},\"a\":{\"type\":\"integer\",\"min\":0,\"max\":2}}",
        "\"params\":{\"a\":{\"type\":\"integer\",\"min\":0,\"min\":1,\"max\":2}}",
    ] {
        assert!(
            Manifest::parse(&text.replace("\"params\":{}", replacement)).is_err(),
            "{replacement}"
        );
    }
    let mut m = base();
    m["template"]["extra"] = json!(true);
    assert!(matches!(
        parse(&m),
        Err(ManifestError::TemplateBoundInvalid { .. })
    ));
    let text = base().to_string();
    assert!(
        Manifest::parse(&text.replace(
            "\"max_per_agent\":2",
            "\"max_per_agent\":2,\"max_per_agent\":3"
        ))
        .is_err()
    );
}

#[test]
fn timestamps_normalize_utc_milliseconds_and_reject_finer_precision() {
    let m = parse(&with_parameter(
        json!({"type":"timestamp","max_ahead":"1h"}),
    ))
    .unwrap();
    let now = "2026-11-01T09:00:00Z"
        .parse::<jiff::Timestamp>()
        .unwrap()
        .as_millisecond();
    for text in [
        "2026-11-01T09:00:00Z",
        "2026-11-01T09:00:00.000Z",
        "2026-11-01t09:00:00+00:00",
    ] {
        let params = validate_values(&m, &json!({"value":text}), now).unwrap();
        assert_eq!(params, json!({"value":"2026-11-01T09:00:00Z"}));
    }
    assert_eq!(
        validate_values(&m, &json!({"value":"2026-11-01T09:00:00.1+00:00"}), now).unwrap(),
        json!({"value":"2026-11-01T09:00:00.100Z"})
    );
    for text in [
        "2026-11-01T09:00:00.0000Z",
        "2026-11-01T09:00:00.1234Z",
        "2026-11-01T10:00:00+01:00",
    ] {
        assert_eq!(
            validate_values(&m, &json!({"value":text}), now)
                .unwrap_err()
                .code,
            "param_out_of_bounds"
        );
    }
    assert_eq!(
        instance_id(
            "watch",
            "agent_test",
            &validate_values(&m, &json!({"value":"2026-11-01T09:00:00Z"}), now).unwrap()
        ),
        instance_id(
            "watch",
            "agent_test",
            &validate_values(&m, &json!({"value":"2026-11-01T09:00:00.000Z"}), now).unwrap()
        )
    );
}

#[test]
fn template_migration_preserves_persona_flow_run_state_and_foreign_keys() {
    use basal_core::schema::MIGRATIONS;
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    let migration = MIGRATIONS
        .iter()
        .find(|m| m.statements.contains("CREATE TABLE template_instances"))
        .unwrap();
    for before in MIGRATIONS
        .iter()
        .take_while(|m| m.version < migration.version)
    {
        conn.execute_batch(before.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO package_versions(package,version,script,manifest,code_hash,registered_at) VALUES ('persona',1,'return 1','{}',zeroblob(32),1); INSERT INTO flows(flow_id,owner,package,approved_version,created_at) VALUES ('persona_1234567890abcdef','agent_owner','persona',1,1); INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at,self_input) VALUES ('old','persona_1234567890abcdef','first',1,'{}','return 1','{}',zeroblob(32),'pending',1,'{\"agent_id\":\"agent_owner\"}'); INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES ('persona_1234567890abcdef','key','7',4,2);").unwrap();
    conn.execute_batch(migration.statements).unwrap();
    assert_eq!(
        conn.query_row("SELECT owner FROM flows", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "agent_owner"
    );
    assert_eq!(
        conn.query_row("SELECT revision FROM kv", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let (identity,params,guards,incarnation,end): (String,Option<String>,Option<String>,Option<i64>,bool)=conn.query_row("SELECT self_input,params_input,grant_guards,template_incarnation,end_instance FROM runs",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(identity, "{\"agent_id\":\"agent_owner\"}");
    assert_eq!(params, None);
    assert_eq!(guards, None);
    assert_eq!(incarnation, None);
    assert!(!end);
    assert!(
        conn.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}

#[test]
fn integer_bounds_and_values_remain_exact_script_numbers() {
    let safe = (1_i64 << 53) - 1;
    for decl in [
        json!({"type":"integer","min":-safe-1,"max":safe}),
        json!({"type":"integer","min":-safe,"max":safe+1}),
    ] {
        assert!(matches!(
            parse(&with_parameter(decl)),
            Err(ManifestError::TemplateBoundInvalid { .. })
        ));
    }
    let m = parse(&with_parameter(
        json!({"type":"integer","min":-safe,"max":safe}),
    ))
    .unwrap();
    for n in [-safe, safe] {
        assert_eq!(
            validate_values(&m, &json!({"value":n}), 0).unwrap(),
            json!({"value":n})
        );
    }
    for n in [-safe - 1, safe + 1] {
        assert_eq!(
            validate_values(&m, &json!({"value":n}), 0)
                .unwrap_err()
                .code,
            "param_out_of_bounds"
        );
    }
}

#[test]
fn admission_boundaries_are_not_manifest_input_fields() {
    let mut m = base();
    m["template_grants"] = json!({"14":[]});
    assert!(matches!(parse(&m), Err(ManifestError::Decode(_))));
}

#[test]
fn lifecycle_migration_preserves_existing_instances_and_adds_outbox() {
    use basal_core::schema::MIGRATIONS;
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for migration in MIGRATIONS.iter().filter(|m| m.version < 27) {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO flows(flow_id,owner,package,approved_version,created_at) VALUES ('watch_1234567890abcdef','owner','watch',1,0); INSERT INTO template_instances(flow_id,params,grant_guards,expires_at,incarnation) VALUES ('watch_1234567890abcdef','{\"note\":\"kept\"}','[]',1000,3);").unwrap();
    conn.execute_batch(
        MIGRATIONS
            .iter()
            .find(|m| m.version == 27)
            .unwrap()
            .statements,
    )
    .unwrap();
    let row: (String, i64, i64, i64, bool) = conn
        .query_row(
            "SELECT params,expires_at,incarnation,seq,expiry_notified FROM template_instances",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(row, ("{\"note\":\"kept\"}".into(), 1000, 3, 0, false));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM template_event_outbox", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(
        conn.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}
