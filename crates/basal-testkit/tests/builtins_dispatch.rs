//! The built-ins through basal-core's runtime and the real worker:
//! authorization against the run's approved manifest before dispatch,
//! refusals journaled as rejections, audit rows without contents, the
//! dispatch class of each built-in and what recovery does with each, and
//! install validation of the `fs`, `git` and `net` lines.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use basal_core::{
    Config, InstallError, InstallRequest, NoHooks, RunState, Runtime, Store, StoredClass, Warning,
};
use basal_host::builtins::{self, BuiltinHost};
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, InstallStatus,
    TransportError,
};
use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use basal_testkit::harness::{Point, Probe, World, scratch, test_manifest};
use common::{config, finish, result};
use serde_json::{Value, json};

/// Built-ins go to a real [`BuiltinHost`], except `net.fetch`, which
/// answers a fixed response here (the network rules have their own tests);
/// everything else goes to the world's mock. Every built-in send is
/// recorded by name.
struct Split {
    world_mock: basal_host::mock::MockHost,
    builtins: BuiltinHost,
    sends: Mutex<Vec<String>>,
}

impl Split {
    fn new(world: &World) -> Arc<Self> {
        Arc::new(Self {
            world_mock: world.mock.clone(),
            builtins: BuiltinHost::default(),
            sends: Mutex::new(Vec::new()),
        })
    }

    fn sends(&self) -> Vec<String> {
        self.sends.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn builtin(
        &self,
        request: &CallRequest,
        class: Option<CallClass>,
    ) -> Option<Result<Dispatched, TransportError>> {
        let CallKind::Primitive(p) = request.kind else {
            return None;
        };
        if !p.is_builtin() {
            return None;
        }
        if let Ok(mut s) = self.sends.lock() {
            s.push(p.name().to_owned());
        }
        if p == Primitive::NetFetch {
            let body = json!({ "status": 200, "headers": {}, "body": "fetched" }).to_string();
            return Some(Ok(Dispatched::Completed(HostOutcome::fulfilled(
                JsonText::new(body).expect("small"),
            ))));
        }
        Some(match class {
            Some(c) => self.builtins.dispatch_classified(request, c),
            None => self.builtins.dispatch(request),
        })
    }
}

impl Host for Split {
    fn classify(&self, kind: &CallKind) -> CallClass {
        if builtins::is_builtin(kind) {
            self.builtins.classify(kind)
        } else {
            self.world_mock.classify(kind)
        }
    }

    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        self.builtin(request, None)
            .unwrap_or_else(|| self.world_mock.dispatch(request))
    }

    fn dispatch_classified(
        &self,
        request: &CallRequest,
        class: CallClass,
    ) -> Result<Dispatched, TransportError> {
        self.builtin(request, Some(class))
            .unwrap_or_else(|| self.world_mock.dispatch_classified(request, class))
    }

    fn dispatch_committed(&self, request: &CallRequest) {
        self.world_mock.dispatch_committed(request);
    }

    fn now_ms(&self) -> f64 {
        self.world_mock.now_ms()
    }

    fn random(&self) -> f64 {
        self.world_mock.random()
    }

    fn attach(&self, sink: Arc<dyn CompletionSink>) {
        self.world_mock.attach(sink);
    }

    fn install_status(&self, flow_id: &str, version: u32) -> Result<InstallStatus, TransportError> {
        self.world_mock.install_status(flow_id, version)
    }
}

fn open(world: &World, host: Arc<Split>, hooks: Arc<dyn basal_core::Hooks>) -> Runtime {
    let store = Arc::new(Store::open(world.store_path(), world.durability).expect("store"));
    let rt = Runtime::new(
        store,
        host,
        Arc::new(world.catalog.clone()),
        hooks,
        Some(world.source.clone()),
        Config {
            selector: world.selector.clone(),
            ..config()
        },
    );
    rt.recover().expect("recover");
    rt
}

/// A root with a file, a repository-free sibling outside it, and the test
/// manifest with an `fs` line for the root.
struct Files {
    base: std::path::PathBuf,
    root: std::path::PathBuf,
    outside: std::path::PathBuf,
}

impl Files {
    fn new(tag: &str) -> Self {
        let base = scratch(tag);
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(root.join("notes.txt"), "PRIVATE-FILE-TEXT").expect("notes");
        std::fs::write(outside.join("secret.txt"), "secret").expect("secret");
        Self {
            base,
            root,
            outside,
        }
    }

    fn manifest(&self) -> Value {
        let mut m = test_manifest();
        m["fs"] = json!({
            "read": [self.root.display().to_string()],
            "write": [self.root.display().to_string()],
        });
        m["net"] = json!({ "fetch": [
            { "host": "api.example.com" },
            { "host": "hooks.example.com", "methods": ["POST"] },
        ] });
        m
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn admit_with(rt: &Runtime, world: &World, script: &str, manifest: &Value) -> String {
    rt.admit(&world.spec_with(rt, script, manifest).expect("approve"))
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned()
}

fn code_of(value: &JsonText) -> Option<String> {
    serde_json::from_str::<Value>(value.as_str())
        .ok()
        .and_then(|v| v["code"].as_str().map(str::to_owned))
}

/// Every built-in call is checked against the approved manifest in the
/// parent before it is journaled for dispatch: a call outside it is a
/// journaled rejection the script can catch, and never reaches the host.
#[test]
fn calls_outside_the_manifest_are_refused_in_the_parent_and_journaled() {
    let world = World::new("builtins-auth");
    let files = Files::new("builtins-auth-files");
    let host = Split::new(&world);
    let rt = open(&world, host.clone(), Arc::new(NoHooks));
    let script = format!(
        r#"
        const attempt = async (f) => {{ try {{ await f(); return 'ok'; }} catch (e) {{ return e.data.code; }} }};
        return {{
            inside: (await fs.read({inside:?})).text,
            outside: await attempt(() => fs.read({outside:?})),
            escape: await attempt(() => fs.read({escape:?})),
            git: await attempt(() => git.log({root:?})),
            host: await attempt(() => net.fetch('https://evil.example.com/x')),
            method: await attempt(() => net.fetch('https://api.example.com/x', {{ method: 'POST', body: 'x' }})),
            fetched: (await net.fetch('https://api.example.com/x')).body,
        }};
        "#,
        inside = files.root.join("notes.txt").display().to_string(),
        outside = files.outside.join("secret.txt").display().to_string(),
        escape = files
            .root
            .join("..")
            .join("outside")
            .join("secret.txt")
            .display()
            .to_string(),
        root = files.root.display().to_string(),
    );
    let run_id = admit_with(&rt, &world, &script, &files.manifest());
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert_eq!(
        result(&run),
        json!({
            "inside": "PRIVATE-FILE-TEXT",
            "outside": "denied",
            "escape": "denied",
            "git": "denied",
            "host": "denied",
            "method": "denied",
            "fetched": "fetched",
        })
    );
    // Only the two allowed calls were sent.
    assert_eq!(host.sends(), ["fs.read", "net.fetch"]);
    let calls = rt.calls(&run_id).expect("calls");
    for row in &calls[1..6] {
        let outcome = row.outcome.as_ref().expect("journaled");
        assert_eq!(outcome.settlement, Settlement::Rejected, "{row:?}");
        assert_eq!(
            code_of(&outcome.value).as_deref(),
            Some("denied"),
            "{row:?}"
        );
    }

    // Audit rows name the built-in and carry a digest of its arguments,
    // never the file's text.
    let audit = rt
        .store()
        .read(|c| basal_core::audit::rows(c, &run_id))
        .expect("audit");
    let rows: Vec<(&str, &str)> = audit
        .iter()
        .map(|r| (r.op.as_str(), r.outcome.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("fs.read", "allowed"),
            ("fs.read", "denied"),
            ("fs.read", "denied"),
            ("git.log", "denied"),
            ("net.fetch", "denied"),
            ("net.fetch", "denied"),
            ("net.fetch", "allowed"),
        ]
    );
    let dump: String = rt
        .store()
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT quote(op) || quote(args_digest) || quote(outcome) FROM call_audit",
            )?;
            let all = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(all.join("\n"))
        })
        .expect("dump");
    assert!(!dump.contains("PRIVATE-FILE-TEXT"), "{dump}");
}

/// The manifest a run was approved under decides, not the one on disk
/// now: a later version that drops the `fs` line does not reach a run
/// admitted under the earlier one, and that run's calls carry the scope
/// they were approved under.
#[test]
fn the_grant_travels_with_the_journaled_call() {
    let world = World::new("builtins-envelope");
    let files = Files::new("builtins-envelope-files");
    let host = Split::new(&world);
    let rt = open(&world, host, Arc::new(NoHooks));
    let path = files.root.join("notes.txt").display().to_string();
    let script = format!("return (await fs.read({path:?})).text;");
    let run_id = admit_with(&rt, &world, &script, &files.manifest());
    let run = finish(&rt, &world, &run_id);
    assert_eq!(result(&run), json!("PRIVATE-FILE-TEXT"));
    let call = &rt.calls(&run_id).expect("calls")[0];
    let request: Value =
        serde_json::from_str(call.request.as_ref().expect("request").as_str()).expect("json");
    assert_eq!(request["args"]["path"], path);
    assert_eq!(
        request["grant"]["roots"],
        json!([files.root.display().to_string()])
    );
}

/// The dispatch class of each built-in, stored with the call.
#[test]
fn each_built_in_is_journaled_with_its_class() {
    let world = World::new("builtins-classes");
    let files = Files::new("builtins-classes-files");
    let host = Split::new(&world);
    let rt = open(&world, host, Arc::new(NoHooks));
    let script = format!(
        r#"
        await fs.read({file:?});
        await fs.list({root:?});
        await fs.stat({file:?});
        await fs.write({out:?}, 'digest');
        await net.fetch('https://api.example.com/x');
        await net.fetch('https://api.example.com/x', {{ method: 'HEAD' }});
        await net.fetch('https://hooks.example.com/x', {{ method: 'POST', body: 'b' }});
        return 1;
        "#,
        file = files.root.join("notes.txt").display().to_string(),
        root = files.root.display().to_string(),
        out = files.root.join("out.txt").display().to_string(),
    );
    let run_id = admit_with(&rt, &world, &script, &files.manifest());
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    let classes: Vec<StoredClass> = rt
        .calls(&run_id)
        .expect("calls")
        .iter()
        .map(|c| c.class)
        .collect();
    assert_eq!(
        classes,
        [
            StoredClass::Query,
            StoredClass::Query,
            StoredClass::Query,
            StoredClass::KeyedMutation,
            StoredClass::Query,
            StoredClass::Query,
            StoredClass::Mutation,
        ]
    );
    // Every git read is a query, whatever its arguments.
    for p in [
        Primitive::GitLog,
        Primitive::GitRevParse,
        Primitive::GitDescribeTags,
        Primitive::GitShow,
        Primitive::GitDiff,
    ] {
        assert_eq!(
            builtins::class(&CallKind::Primitive(p), &json!({})),
            Some(CallClass::Query),
            "{p:?}"
        );
    }
}

/// The process dies after the host answered and before the outcome was
/// committed. Recovery sends a query or `fs.write` again (under the same
/// key), and stops an unkeyed `net.fetch` mutation for the operator.
#[test]
fn recovery_reissues_queries_and_writes_and_reconciles_a_post() {
    let files = Files::new("builtins-recover-files");
    let out = files.root.join("out.txt").display().to_string();
    let cases = [
        (
            format!("return (await fs.read({:?})).text;", files.root.join("notes.txt").display().to_string()),
            "fs.read",
            RunState::Succeeded,
        ),
        (
            format!("await fs.write({out:?}, 'the digest'); return 1;"),
            "fs.write",
            RunState::Succeeded,
        ),
        (
            "return (await net.fetch('https://api.example.com/x')).body;".to_owned(),
            "net.fetch",
            RunState::Succeeded,
        ),
        (
            "await net.fetch('https://hooks.example.com/x', { method: 'POST', body: 'b' }); return 1;"
                .to_owned(),
            "net.fetch",
            RunState::NeedsReconcile,
        ),
    ];
    for (script, name, expected) in cases {
        let world = World::new("builtins-recover");
        let host = Split::new(&world);
        let probe = Arc::new(Probe::crash_at(
            Point::parse("HostAnswered { position: 0 }#1").expect("point"),
        ));
        let rt = open(&world, host.clone(), probe.clone());
        let run_id = admit_with(&rt, &world, &script, &files.manifest());
        let _ = rt.run_to_rest(&run_id, Duration::from_secs(30));
        rt.quiesce();
        assert!(probe.fired(), "{script}: the cut was not reached");
        drop(rt);
        world.mock.detach();
        let rt = open(&world, host.clone(), Arc::new(NoHooks));
        let run = finish(&rt, &world, &run_id);
        assert_eq!(run.state, expected, "{script}: {run:#?}");
        let sent = if expected == RunState::Succeeded {
            2
        } else {
            1
        };
        assert_eq!(host.sends(), vec![name; sent], "{script}");
    }
    assert_eq!(
        std::fs::read_to_string(&out).expect("written"),
        "the digest"
    );
}

fn install(rt: &Runtime, manifest: &Value) -> Result<basal_core::Installed, InstallError> {
    rt.install(&InstallRequest {
        script: "return 1;".into(),
        manifest: manifest.to_string(),
        author: "ALF".into(),
        loop_override: false,
    })
}

#[test]
fn install_refuses_bad_roots_hosts_and_methods() {
    let world = World::new("builtins-install");
    let files = Files::new("builtins-install-files");
    let rt = open(&world, Split::new(&world), Arc::new(NoHooks));
    let root = files.root.display().to_string();
    let missing = files.base.join("missing").display().to_string();
    let bad: [(&str, Value); 9] = [
        (
            "relative root",
            json!({ "fs": { "read": ["relative/dir"] } }),
        ),
        (
            "dotdot root",
            json!({ "fs": { "read": [files.root.join("..").join("outside").display().to_string()] } }),
        ),
        (
            "missing read root",
            json!({ "fs": { "read": [missing.clone()] } }),
        ),
        (
            "missing write root",
            json!({ "fs": { "write": [missing.clone()] } }),
        ),
        (
            "missing repo",
            json!({ "git": { "read": [missing.clone()] } }),
        ),
        (
            "wildcard host",
            json!({ "net": { "fetch": [{ "host": "*.example.com" }] } }),
        ),
        (
            "ip host",
            json!({ "net": { "fetch": [{ "host": "127.0.0.1" }] } }),
        ),
        (
            "ipv6 host",
            json!({ "net": { "fetch": [{ "host": "[::1]" }] } }),
        ),
        (
            "bad method",
            json!({ "net": { "fetch": [{ "host": "api.example.com", "methods": ["CONNECT"] }] } }),
        ),
    ];
    for (what, lines) in bad {
        let mut m = test_manifest();
        for (k, v) in lines.as_object().expect("lines") {
            m[k] = v.clone();
        }
        assert!(install(&rt, &m).is_err(), "{what} was accepted");
    }
    let mut good = test_manifest();
    good["fs"] = json!({ "read": [root.clone(), "~"], "write": [root.clone()] });
    good["git"] = json!({ "read": [root.clone()] });
    good["net"] = json!({ "fetch": [{ "host": "api.github.com" }, { "host": "hooks.example.com", "methods": ["POST"] }] });
    let installed = install(&rt, &good).expect("install");
    // The test manifest's facts grant does not read private text.
    assert!(
        !installed
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::PrivateTextWithNetFetch { .. })),
        "{:?}",
        installed.warnings
    );
}

/// Private facts text and any `net.fetch` grant together are the
/// exfiltration case the card warns about.
#[test]
fn private_text_with_a_fetch_grant_warns() {
    let world = World::new("builtins-warning");
    let rt = open(&world, Split::new(&world), Arc::new(NoHooks));
    let mut m = test_manifest();
    m["facts"] = json!({ "targets": ["ALF"], "text": true });
    m["net"] = json!({ "fetch": [{ "host": "api.github.com" }] });
    let installed = install(&rt, &m).expect("install");
    assert!(
        installed
            .warnings
            .contains(&Warning::PrivateTextWithNetFetch {
                host: "api.github.com".into()
            }),
        "{:?}",
        installed.warnings
    );
    // Without the fetch grant, no such warning.
    m.as_object_mut().expect("object").remove("net");
    m["id"] = json!("flow-without-fetch");
    let installed = install(&rt, &m).expect("install");
    assert!(
        !installed
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::PrivateTextWithNetFetch { .. }))
    );
}
