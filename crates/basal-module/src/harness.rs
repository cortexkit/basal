//! The module against the mock hosts, driven over stdin and stdout instead
//! of subc: the process the end-to-end tests start, kill and restart.
//!
//! ```text
//! ck-basal-harness --dir <dir> --worker <ck-basal-worker>
//!     [--kill-worker-at <point>] [--kill-self-at <point>] [--cut-store-at <point>]
//!     [--warm-spares <n>] [--max-concurrent <n>]
//!     [--builtins-trust-der <file>] [--builtins-resolve <host>=<ip>:<port>]...
//! ```
//!
//! With `--routing-fake`, the built-ins run for real through basal's own
//! host. `--builtins-trust-der` makes `net.fetch` trust only the DER
//! certificate in the file instead of the bundled roots, and each
//! `--builtins-resolve` answers a host name with a fixed address, which may
//! be a loopback one: together they point `net.fetch` at a local test
//! server. The production module has neither setting.
//!
//! It is the same [`Module`] the production binary starts (store, recovery
//! first, pool, engine, ops, consent), with the mock host (its state in
//! `<dir>/mock.json`, synced before every reply so effects survive a kill),
//! the standard mock catalog, the mock consent plane, and a manual clock
//! kept in `<dir>/clock`. The harness stands in for the daemon too: each op
//! command names who calls it, and the harness turns that into the route
//! bind the daemon would have sent (principal and scope stamp), which goes
//! through the same [`crate::serve::Routes`] as a real route's bind.
//!
//! A point is a boundary as its debug form, then `#` and which occurrence
//! in this process: `HostAnswered { position: 2 }#1`. At it the harness
//! kills the run's worker, sends itself SIGKILL, or cuts the store (a
//! storage failure).
//!
//! Commands are one JSON object per line; each gets one JSON line back:
//! `op`, `clock`, `decide`, `cards`, `pump`, `runs`, `effects`, `quit`.

#[path = "harness_transport.rs"]
mod harness_transport;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use basal_core::broca::BrocaStore;
use basal_core::{Boundary, Clock, Config, Durability, Hooks, Step};
use basal_host::Host;
use basal_host::broca::{BrocaHost, fake::FakeBroca};
use basal_host::mock::MockHost;
use basal_host::{CardDecision, MockCatalog, MockConsent};
use serde_json::{Value, json};
use subc_client_rs::{RouteBindRequest, RouteHandle};
use subc_protocol::scope::{ScopeAttributes, ScopeKind, ScopeStamp};
use subc_protocol::{BindIdentity, Principal, RouteTarget};

use crate::caller::{CORE_MODULE, OPERATOR_MODULE};
use crate::dryrun::DryRunConfig;
use crate::engine::EngineConfig;
use crate::fatal::EXIT_STORE_FAILURE;
use crate::module::{Hosts, Module, ModuleConfig};
use crate::pool::{Pool, PoolConfig, ProcessSpawner};
use crate::serve::Routes;

/// The clock's start when the directory has none: 2026-05-01T00:00:00Z.
const DEFAULT_CLOCK_MS: i64 = 1_777_593_600_000;

struct Args {
    dir: PathBuf,
    worker: PathBuf,
    kill_worker_at: Option<String>,
    kill_self_at: Option<String>,
    cut_store_at: Option<String>,
    warm_spares: usize,
    max_concurrent: usize,
    routing_fake: bool,
    broca: bool,
    builtins_trust_der: Option<PathBuf>,
    builtins_resolve: Vec<(String, std::net::SocketAddr)>,
}

fn parse() -> Result<Args, String> {
    let mut args = Args {
        dir: PathBuf::new(),
        worker: PathBuf::new(),
        kill_worker_at: None,
        kill_self_at: None,
        cut_store_at: None,
        warm_spares: 1,
        max_concurrent: 4,
        routing_fake: false,
        broca: false,
        builtins_trust_der: None,
        builtins_resolve: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    let mut dir = None;
    let mut worker = None;
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--routing-fake" => args.routing_fake = true,
            "--broca" => args.broca = true,
            "--dir" => dir = Some(PathBuf::from(value()?)),
            "--worker" => worker = Some(PathBuf::from(value()?)),
            "--kill-worker-at" => args.kill_worker_at = Some(value()?),
            "--kill-self-at" => args.kill_self_at = Some(value()?),
            "--cut-store-at" => args.cut_store_at = Some(value()?),
            "--builtins-trust-der" => args.builtins_trust_der = Some(PathBuf::from(value()?)),
            "--builtins-resolve" => {
                let text = value()?;
                let (host, addr) = text
                    .split_once('=')
                    .ok_or("--builtins-resolve needs <host>=<ip>:<port>")?;
                let addr = addr
                    .parse()
                    .map_err(|e| format!("--builtins-resolve {text}: {e}"))?;
                args.builtins_resolve.push((host.to_owned(), addr));
            }
            "--warm-spares" => {
                args.warm_spares = value()?
                    .parse()
                    .map_err(|e| format!("--warm-spares: {e}"))?;
            }
            "--max-concurrent" => {
                args.max_concurrent = value()?
                    .parse()
                    .map_err(|e| format!("--max-concurrent: {e}"))?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    args.dir = dir.ok_or("--dir is required")?;
    args.worker = worker.ok_or("--worker is required")?;
    Ok(args)
}

/// Acts at named boundaries.
struct HarnessHooks {
    pool: Arc<OnceLock<Pool>>,
    kill_worker_at: Option<String>,
    kill_self_at: Option<String>,
    cut_store_at: Option<String>,
    seen: Mutex<HashMap<String, u32>>,
}

impl Hooks for HarnessHooks {
    fn at(&self, run_id: &str, boundary: &Boundary) -> Step {
        let name = format!("{boundary:?}");
        let count = {
            let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
            let n = seen.entry(name.clone()).or_insert(0);
            *n += 1;
            *n
        };
        let point = format!("{name}#{count}");
        if self.kill_worker_at.as_deref() == Some(point.as_str()) {
            let killed = self
                .pool
                .get()
                .is_some_and(|pool| pool.kill_for_run(run_id));
            eprintln!("ck-basal-harness: killed the worker of {run_id} at {point}: {killed}");
        }
        if self.kill_self_at.as_deref() == Some(point.as_str()) {
            eprintln!("ck-basal-harness: killing itself at {point}");
            // SAFETY: kill(2) on our own pid has no memory-safety
            // preconditions.
            unsafe {
                libc::kill(libc::getpid(), libc::SIGKILL);
            }
            // kill(2) can return before the signal takes the process down;
            // this thread must not go on to the next commit meanwhile.
            loop {
                std::thread::park();
            }
        }
        if self.cut_store_at.as_deref() == Some(point.as_str()) {
            eprintln!("ck-basal-harness: cutting the store at {point}");
            return Step::Crash;
        }
        Step::Continue
    }
}

/// The built-ins' settings from the command line: production's unless a
/// test points `net.fetch` at its own server.
#[cfg(feature = "rig-kill-hook")]
fn builtins_config(args: &Args) -> Result<basal_host::builtins::BuiltinConfig, String> {
    let mut config = basal_host::builtins::BuiltinConfig::default();
    if let Some(path) = &args.builtins_trust_der {
        let der = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        config.net.trust_anchors = Some(vec![der]);
    }
    if !args.builtins_resolve.is_empty() {
        let mut table = std::collections::BTreeMap::new();
        for (host, addr) in &args.builtins_resolve {
            table
                .entry(host.clone())
                .or_insert_with(Vec::new)
                .push(*addr);
            config.net.private_hosts.insert(host.clone());
        }
        config.net.resolver = Arc::new(basal_host::builtins::StaticResolver(table));
    }
    Ok(config)
}

#[cfg(not(feature = "rig-kill-hook"))]
fn builtins_config(args: &Args) -> Result<basal_host::builtins::BuiltinConfig, String> {
    if args.builtins_trust_der.is_some() || !args.builtins_resolve.is_empty() {
        return Err("network fixtures require the rig-kill-hook test feature".into());
    }
    Ok(basal_host::builtins::BuiltinConfig::default())
}

fn read_clock(dir: &Path) -> i64 {
    std::fs::read_to_string(dir.join("clock"))
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(DEFAULT_CLOCK_MS)
}

fn write_clock(dir: &Path, ms: i64) -> std::io::Result<()> {
    let tmp = dir.join("clock.tmp");
    std::fs::write(&tmp, ms.to_string())?;
    std::fs::rename(tmp, dir.join("clock"))
}

/// The route-bind request the daemon would send on `handle` for the caller
/// an op command names in `who` (`"operator"` for the attested callosum,
/// `"local"` for an unscoped direct key-holder, `"core"`, `{"agent": ..}`,
/// `{"module": ..}`, anything else unverified): its principal and, for an
/// agent, a session scope owned by core and marked owner-authorized.
fn bind_request(handle: RouteHandle, who: &Value) -> RouteBindRequest {
    let request = RouteBindRequest::new(
        handle,
        RouteTarget::ManagementSurface {
            module_id: crate::manifest::MODULE_ID.to_owned(),
        },
        BindIdentity::new("/", "harness", "ses-harness"),
    );
    match who {
        Value::String(s) if s == "operator" => request.with_principal(Principal::Reserved {
            module_id: OPERATOR_MODULE.into(),
        }),
        Value::String(s) if s == "local" => request.with_principal(Principal::Direct),
        Value::String(s) if s == "core" => request.with_principal(Principal::Reserved {
            module_id: CORE_MODULE.into(),
        }),
        Value::Object(o) if o.contains_key("agent") => {
            let agent = o.get("agent").and_then(Value::as_str).map(str::to_owned);
            let attributes = ScopeAttributes::new().with_agent_id(agent);
            request
                .with_principal(Principal::Direct)
                .with_scope(ScopeStamp {
                    owner: Principal::Reserved {
                        module_id: CORE_MODULE.into(),
                    },
                    scope_ref: "harness-session".into(),
                    scope_epoch: 1,
                    kind: ScopeKind::Head,
                    parent: None,
                    parent_state: None,
                    attributes,
                    owner_authorized: true,
                })
        }
        Value::Object(o) if o.contains_key("module") => {
            request.with_principal(Principal::Reserved {
                module_id: o
                    .get("module")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
            })
        }
        _ => request.with_principal(Principal::Unverified),
    }
}

fn runs(module: &Module) -> Result<Value, String> {
    module
        .rt
        .store()
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT run_id, flow_id, trigger_id, state, broken, result, error_kind \
                 FROM runs ORDER BY admit_seq, run_id",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "run_id": r.get::<_, String>(0)?,
                        "flow_id": r.get::<_, String>(1)?,
                        "trigger_id": r.get::<_, String>(2)?,
                        "state": r.get::<_, String>(3)?,
                        "broken": r.get::<_, i64>(4)?,
                        "result": r.get::<_, Option<String>>(5)?,
                        "error_kind": r.get::<_, Option<String>>(6)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(Value::Array(rows))
        })
        .map_err(|e| e.to_string())
}

pub fn main() -> std::process::ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ck-basal-harness: {e}");
            return std::process::ExitCode::from(64);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&args.dir) {
        eprintln!("ck-basal-harness: {}: {e}", args.dir.display());
        return std::process::ExitCode::FAILURE;
    }
    let mock = match MockHost::persistent(args.dir.join("mock.json")) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("ck-basal-harness: mock host: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let broca_store = Arc::new(BrocaStore::default());
    let selector = Arc::new(basal_host::selector::FakeSelector::default());
    let fake = match FakeBroca::persistent(args.dir.join("broca.json")) {
        Ok(fake) => Arc::new(fake),
        Err(error) => {
            eprintln!("ck-basal-harness: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let broca = Arc::new(BrocaHost::new(
        fake.clone(),
        broca_store.clone(),
        args.dir.to_string_lossy().into_owned(),
        "basal".into(),
        selector.clone(),
    ));
    let model_host: Arc<dyn Host> = if args.broca {
        broca.clone()
    } else {
        Arc::new(mock.clone())
    };
    let consent = MockConsent::new();
    let clock = Clock::manual(read_clock(&args.dir));
    let pool_slot = Arc::new(OnceLock::new());
    let hooks = Arc::new(HarnessHooks {
        pool: pool_slot.clone(),
        kill_worker_at: args.kill_worker_at.clone(),
        kill_self_at: args.kill_self_at.clone(),
        cut_store_at: args.cut_store_at.clone(),
        seen: Mutex::new(HashMap::new()),
    });
    let mut pool = PoolConfig::new(&args.worker, crate::process::WorkerLaunch::Plain);
    pool.warm_spares = args.warm_spares;
    pool.handshake_timeout = Duration::from_secs(180);
    let config = ModuleConfig {
        store_path: args.dir.join("basal.db"),
        durability: Durability { fullfsync: false },
        runtime: Config {
            clock: clock.clone(),
            selector: selector.clone(),
            activation_deadline: Duration::from_secs(60),
            // The harness's consent plane is a mock, so no core approved its
            // flows and no core could be asked about them.
            install_gate: basal_core::InstallGate::Off,
            ..Config::default()
        },
        pool: pool.clone(),
        engine: EngineConfig {
            max_concurrent_activations: args.max_concurrent,
            ..EngineConfig::default()
        },
        dry_run: DryRunConfig::new(args.dir.join("dry-run")),
    };
    let host: Arc<dyn basal_host::Host> = if args.routing_fake {
        use basal_host::{
            core_host::CoreHost,
            routing::{ModuleOpsHost, RoutingHost},
            subc_catalog::SubcCatalog,
        };
        let transport = Arc::new(harness_transport::HarnessTransport(mock.clone()));
        let catalog = Arc::new(SubcCatalog::new(transport.clone()));
        let builtins = match builtins_config(&args) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("ck-basal-harness: {e}");
                return std::process::ExitCode::from(64);
            }
        };
        Arc::new(
            RoutingHost::new(
                Arc::new(ModuleOpsHost::new(transport.clone(), catalog)),
                Arc::new(CoreHost::new(transport)),
                model_host.clone(),
            )
            .with_builtins(Arc::new(basal_host::builtins::BuiltinHost::new(builtins))),
        )
    } else {
        model_host
    };
    let module = match Module::start_with_store(
        config,
        Hosts {
            host,
            catalog: Arc::new(MockCatalog::standard()),
            consent: Arc::new(consent.clone()),
            hooks,
        },
        Arc::new(ProcessSpawner::new(&pool)),
        |shared| {
            if args.broca {
                broca_store.bind(shared).map_err(|e| e.to_string())
            } else {
                Ok(())
            }
        },
    ) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("ck-basal-harness: {e}");
            return std::process::ExitCode::from(EXIT_STORE_FAILURE as u8);
        }
    };
    if args.broca
        && let Err(error) = broca.poll()
    {
        eprintln!("ck-basal-harness: {error}");
        return std::process::ExitCode::FAILURE;
    }
    let _ = pool_slot.set(module.pool.clone());
    module.fatal.exit_when_raised();

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(command) => {
                if args.broca && command.get("cmd").and_then(Value::as_str) == Some("broca_finish")
                {
                    let text = command.get("text").and_then(Value::as_str).unwrap_or("");
                    let usage = command
                        .get("usage")
                        .filter(|v| !v.is_null())
                        .cloned()
                        .map(serde_json::from_value)
                        .transpose();
                    match usage {
                        Ok(usage) => {
                            let result = fake.keys().iter().try_for_each(|key| {
                                fake.finish(
                                    key,
                                    text,
                                    basal_host::broca::wire::RunFinishReason::Completed,
                                    usage.clone(),
                                )
                            });
                            match result.and_then(|_| broca.poll()) {
                                Ok(()) => json!({"ok": true, "result": true}),
                                Err(error) => json!({"ok": false, "error": error.to_string()}),
                            }
                        }
                        Err(error) => json!({"ok": false, "error": error.to_string()}),
                    }
                } else if args.broca
                    && command.get("cmd").and_then(Value::as_str) == Some("broca_sends")
                {
                    let sends: Vec<Value> = fake.sends().into_iter().map(|(route, bytes)| json!({"route": route, "params": serde_json::from_slice::<Value>(&bytes).ok()})).collect();
                    json!({"ok": true, "result": sends})
                } else {
                    let reply =
                        command_reply(&module, &mock, &consent, &clock, &args.dir, &command);
                    if args.broca
                        && let Err(error) = broca.poll()
                    {
                        eprintln!("ck-basal-harness: {error}");
                    }
                    reply
                }
            }
            Err(e) => json!({ "ok": false, "error": format!("not JSON: {e}") }),
        };
        if reply.get("quit").is_some() {
            module.pool.stop();
            let _ = writeln!(out, "{reply}");
            let _ = out.flush();
            return std::process::ExitCode::SUCCESS;
        }
        if let Some(why) = module.fatal.get() {
            eprintln!("ck-basal-harness: exiting after a store failure: {why}");
            std::process::exit(EXIT_STORE_FAILURE);
        }
        if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
    module.pool.stop();
    std::process::ExitCode::SUCCESS
}

fn command_reply(
    module: &Module,
    mock: &MockHost,
    consent: &MockConsent,
    clock: &Clock,
    dir: &Path,
    command: &Value,
) -> Value {
    let ok = |result: Value| json!({ "ok": true, "result": result });
    let err = |e: String| json!({ "ok": false, "error": e });
    match command.get("cmd").and_then(Value::as_str) {
        Some("op") => {
            // Each op is decided on a route bound for it alone, so one
            // command's caller can never carry over to the next.
            let handle = RouteHandle::detached(1, 1);
            let routes = Routes::default();
            routes.bind(&bind_request(
                handle,
                command.get("as").unwrap_or(&Value::Null),
            ));
            let caller = routes.caller(&handle);
            routes.forget(&handle);
            let method = command.get("method").and_then(Value::as_str).unwrap_or("");
            let params = command.get("params").cloned().unwrap_or(Value::Null);
            match module.handle(&caller, method, params) {
                Ok(v) => ok(v),
                Err(e) => {
                    let mut error = json!({ "code": e.code, "message": e.message });
                    if let Some(detail) = e.detail { error["detail"] = detail; }
                    json!({ "ok": false, "error": error })
                },
            }
        }
        Some("clock") => match command.get("set_ms").and_then(Value::as_i64) {
            Some(ms) => match write_clock(dir, ms) {
                Ok(()) => {
                    clock.set(ms);
                    ok(json!(ms))
                }
                Err(e) => err(e.to_string()),
            },
            None => err("clock needs set_ms".into()),
        },
        Some("decide") => {
            let card = command.get("card_id").and_then(Value::as_str).unwrap_or("");
            let decision = if command.get("approve").and_then(Value::as_bool) == Some(true) {
                CardDecision::Approve
            } else {
                CardDecision::Reject
            };
            let by = command.get("by").and_then(Value::as_str).unwrap_or("operator");
            if consent.decide(card, decision, by) {
                ok(json!({ "undelivered": consent.undelivered().len() }))
            } else {
                err(format!("no card {card}"))
            }
        }
        Some("cards") => ok(Value::Array(
            consent
                .cards()
                .into_iter()
                .map(|c| json!({ "card_id": c.card_id, "flow_id": c.flow_id, "version": c.version, "fields": c.fields }))
                .collect(),
        )),
        Some("pump") => match module.engine.run_until_idle(500) {
            Ok(()) => ok(json!({ "pool": format!("{:?}", module.pool.stats()) })),
            Err(e) => err(e),
        },
        Some("runs") => match runs(module) {
            Ok(v) => ok(v),
            Err(e) => err(e),
        },
        Some("effects") => {
            let effects: Vec<Value> = mock
                .effects()
                .into_iter()
                .map(|e| {
                    json!({
                        "key": e.key,
                        "module": e.module,
                        "op": e.op,
                        "args": e.args,
                        "sends": mock.send_count(&e.key),
                    })
                })
                .collect();
            ok(Value::Array(effects))
        }
        Some("quit") => json!({ "ok": true, "quit": true }),
        other => err(format!("unknown command {other:?}")),
    }
}
