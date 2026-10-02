//! `ck-basal`: the supervised module.
//!
//! ```text
//! ck-basal --manifest        print the subc manifest as JSON and exit
//! ck-basal --version         print the version and build revision and exit
//! ck-basal --subc <file>     serve (how the supervisor starts it)
//! ```
//!
//! Anything else is refused with status 64, so a mistyped command never
//! falls through to serving.

use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use basal_core::{Config, Durability, NoHooks};
use basal_module::dryrun::DryRunConfig;
use basal_module::engine::EngineConfig;
use basal_module::manifest::{manifest, version_line};
use basal_module::module::{Hosts, ModuleConfig};
use basal_module::pool::PoolConfig;
use basal_module::serve::BasalHandler;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let serving = args
        .iter()
        .any(|a| a == "--subc" || a.starts_with("--subc="));
    match args.as_slice() {
        [flag] if flag == "--manifest" => match serde_json::to_string(&manifest()) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("ck-basal: cannot serialise the manifest: {e}");
                ExitCode::FAILURE
            }
        },
        [flag] if flag == "--version" => {
            println!("{}", version_line());
            ExitCode::SUCCESS
        }
        _ if serving => serve(),
        _ => {
            eprintln!(
                "usage: ck-basal --manifest | ck-basal --version | ck-basal --subc <connection file>"
            );
            ExitCode::from(64)
        }
    }
}

fn serve() -> ExitCode {
    // Module mode logs through the fleet logger, which needs the
    // supervisor's environment; a hand-launched module has none, and stderr
    // is the only place that refusal can go.
    let _logger = match cortexkit_log::init_from_env() {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("ck-basal: cannot start the fleet logger: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The hosts' connection consumes and closes subc's launch-nonce pipe.
    // Establish it before the handler can start the pool, including warm spares.
    let transport =
        match basal_host::transport::SubcTransport::connect(std::time::Duration::from_secs(30)) {
            Ok(transport) => transport,
            Err(error) => {
                tracing::error!("cannot consume launch nonce before worker startup: {error:?}");
                return ExitCode::FAILURE;
            }
        };
    let pool = match PoolConfig::beside_current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("cannot locate ck-basal-worker: {e}");
            return ExitCode::FAILURE;
        }
    };
    struct Models {
        store: Arc<basal_core::broca::BrocaStore>,
        host: Arc<basal_host::broca::BrocaHost>,
        wake: Arc<basal_host::broca::subc::PollWake>,
        selector: Arc<dyn basal_host::selector::ModelSelector>,
    }
    let models = Arc::new(Mutex::new(None::<Models>));
    let configured_models = models.clone();
    let built_models = models.clone();
    let initialized_models = models.clone();
    // Only the rig's kill switch reads this: the store path, once known.
    let store_path_cell = Arc::new(std::sync::OnceLock::<std::path::PathBuf>::new());
    let configured_store = store_path_cell.clone();
    tracing::info!("basal module starting");
    let handler = BasalHandler::new(
        Box::new(move |store_path| {
            let _ = configured_store.set(store_path.clone());
            let scratch = store_path
                .parent()
                .map(|d| d.join("dry-run"))
                .unwrap_or_else(|| std::env::temp_dir().join("ck-basal-dry-run"));
            ModuleConfig {
                store_path,
                durability: Durability::default(),
                runtime: Config {
                    selector: configured_models
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .as_ref()
                        .map(|m| m.selector.clone())
                        .unwrap_or_else(|| Arc::new(basal_host::selector::UnconfiguredSelector)),
                    ..Config::default()
                },
                pool: pool.clone(),
                engine: EngineConfig::default(),
                dry_run: DryRunConfig::new(scratch),
            }
        }),
        Box::new(move || {
            use basal_host::{
                core_consent::CoreConsent,
                core_host::CoreHost,
                routing::{ModuleOpsHost, RoutingHost},
                subc_catalog::SubcCatalog,
            };
            let transport = transport.clone();
            let catalog = Arc::new(SubcCatalog::new(transport.clone()));
            let selector = Arc::new(basal_host::selector::RoutingSelector::new(
                transport.clone(),
            ));
            let store = Arc::new(basal_core::broca::BrocaStore::default());
            let wake = basal_host::broca::subc::PollWake::new();
            let broca_transport = basal_host::broca::subc::SubcBrocaTransport::new(
                transport.clone(),
                "broca".into(),
                wake.callback(),
            );
            let model_host = Arc::new(basal_host::broca::BrocaHost::new(
                broca_transport,
                store.clone(),
                "/".into(),
                "basal".into(),
                selector.clone(),
            ));
            *built_models.lock().unwrap_or_else(|p| p.into_inner()) = Some(Models {
                store,
                host: model_host.clone(),
                wake,
                selector,
            });
            Hosts {
                host: Arc::new(RoutingHost::new(
                    Arc::new(ModuleOpsHost::new(transport.clone(), catalog.clone())),
                    Arc::new(CoreHost::new(transport.clone())),
                    model_host,
                )),
                catalog,
                consent: Arc::new(CoreConsent::new(transport).with_polling()),
                hooks: runtime_hooks(&store_path_cell),
            }
        }),
    )
    .with_store_initializer(Box::new(move |shared| {
        if let Some(models) = initialized_models
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            models.store.bind(shared).map_err(|e| e.to_string())?;
            models.wake.start(&models.host);
        }
        Ok(())
    }));
    match run(handler) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("module exited: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The runtime's hooks: none in production. Only the isolated test rig's own
/// build (feature `rig-kill-hook`, see `rig_kill`) adds a kill switch, which
/// its crash test uses to end the process at a chosen runtime boundary.
fn runtime_hooks(
    store: &Arc<std::sync::OnceLock<std::path::PathBuf>>,
) -> Arc<dyn basal_core::Hooks> {
    #[cfg(feature = "rig-kill-hook")]
    if let Some(hook) = basal_module::rig_kill::RigKillHook::from_env(store.clone()) {
        return Arc::new(hook);
    }
    #[cfg(not(feature = "rig-kill-hook"))]
    let _ = store;
    Arc::new(NoHooks)
}

#[tokio::main]
async fn run(handler: BasalHandler) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    subc_client_rs::serve(manifest(), handler).await?;
    Ok(())
}
