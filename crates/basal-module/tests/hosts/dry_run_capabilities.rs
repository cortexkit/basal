use super::*;

fn assert_dry_runs_follow_catalog(
    script: &str,
    manifest: Value,
    capability: &str,
    kind: &str,
    unsupported: &str,
    args: Value,
) {
    let fake = Fake::new();
    set_core_capability(&fake, capability, true);
    let f = worker_fixture(
        fake.clone(),
        Arc::new(CoreConsent::new(fake.clone())),
        unsupported,
    );
    let parsed = basal_core::manifest::Manifest::parse(&manifest.to_string()).unwrap();
    let check = |summary: &Value, supported: bool| {
        let runs = summary["runs"].as_array().unwrap();
        assert!(!runs.is_empty(), "{summary:#}");
        for run in runs {
            let calls = run["calls"].as_array().unwrap();
            assert_eq!(calls.len(), 1, "{run:#}");
            let call = &calls[0];
            assert_eq!(call["kind"], kind);
            for (key, value) in args.as_object().unwrap() {
                assert_eq!(&call["args"][key], value, "{call:#}");
            }
            if supported {
                assert_eq!(call["action"], "captured", "{call:#}");
                assert_eq!(call["outcome"]["rejected"], "captured", "{call:#}");
                assert!(!call.to_string().contains(unsupported), "{call:#}");
            } else {
                assert_eq!(call["action"], "refused", "{call:#}");
                assert_eq!(call["outcome"]["rejected"], unsupported, "{call:#}");
            }
        }
        let text = basal_core::dry_run_text::render(&parsed, summary);
        assert_eq!(text.problems.is_empty(), supported, "{text:?}");
    };

    // Install cards and explicit dry runs must use the same capability answer.
    let installed = common::install(&f, &Caller::Operator, script, &manifest);
    check(&installed["dry_run"], true);
    // Change the catalog after construction to catch stale capability snapshots.
    for supported in [false, true, false] {
        set_core_capability(&fake, capability, supported);
        for mode in ["capture", "live"] {
            let result = f
                .module
                .handle(
                    &Caller::Operator,
                    "flow.dry_run",
                    json!({"flow_id":"host-flow","mode":mode,"trigger":{"kind":"test"}}),
                )
                .unwrap();
            check(&result["summary"], supported);
        }
    }
    assert!(fake.calls("sink.digest").is_empty());
    assert!(fake.calls("sink.wait_resolve").is_empty());
}

#[test]
fn digest_replace_key_dry_runs_follow_live_catalog() {
    assert_dry_runs_follow_catalog(
        "await sink.digest('SYNAPSE', {title:'report'}, 'silent', {replace_key:'daily-report'}).catch(()=>null);",
        manifest(),
        basal_host::core_host::FLOW_DIGEST_REPLACE_KEY_V1,
        "sink.digest",
        "digest_replace_key_unsupported",
        json!({"options":{"replace_key":"daily-report"},"item":{"title":"report"}}),
    );
}

#[test]
fn digest_withdraw_dry_runs_follow_live_catalog() {
    assert_dry_runs_follow_catalog(
        "await sink.digest.withdraw('SYNAPSE', {replace_key:'daily-report'}).catch(()=>null);",
        manifest(),
        basal_host::core_host::FLOW_DIGEST_WITHDRAW_V1,
        "sink.digest",
        "digest_withdraw_unsupported",
        json!({"options":{"replace_key":"daily-report"},"withdraw":true}),
    );
}

#[test]
fn wait_resolve_dry_runs_follow_live_catalog() {
    let mut manifest = manifest();
    manifest["sinks"] = json!([{"kind":"wait"}]);
    assert_dry_runs_follow_catalog(
        "await sink.wait.resolve('met').catch(()=>null);",
        manifest,
        basal_host::core_host::FLOW_WAIT_RESOLVE_V1,
        "sink.wait.resolve",
        "wait_resolve_unsupported",
        json!({"outcome":"met"}),
    );
}
