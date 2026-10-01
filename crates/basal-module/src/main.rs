//! `ck-basal`: the supervised module.
//!
//! ```text
//! ck-basal --manifest        print the subc manifest as JSON and exit
//! ck-basal --subc <file>     serve (how the supervisor starts it)
//! ```
//!
//! Anything else is refused with status 64, so a mistyped command never
//! falls through to serving.

use std::process::ExitCode;
use std::sync::Arc;

use basal_core::{Config, Durability, NoHooks};
use basal_module::dryrun::DryRunConfig;
use basal_module::engine::EngineConfig;
use basal_module::manifest::manifest;
use basal_module::module::{Hosts, ModuleConfig};
use basal_module::pool::PoolConfig;
use basal_module::serve::BasalHandler;
use basal_module::unconfigured::{EmptyCatalog, UnconfiguredConsent, UnconfiguredHost};

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
        _ if serving => serve(),
        _ => {
            eprintln!("usage: ck-basal --manifest | ck-basal --subc <connection file>");
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
    let pool = match PoolConfig::beside_current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("cannot locate ck-basal-worker: {e}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("basal module starting");
    let handler = BasalHandler::new(
        Box::new(move |store_path| {
            let scratch = store_path
                .parent()
                .map(|d| d.join("dry-run"))
                .unwrap_or_else(|| std::env::temp_dir().join("ck-basal-dry-run"));
            ModuleConfig {
                store_path,
                durability: Durability::default(),
                runtime: Config::default(),
                pool: pool.clone(),
                engine: EngineConfig::default(),
                dry_run: DryRunConfig::new(scratch),
            }
        }),
        // No real adapters exist yet: every dispatch is refused as never
        // sent, no op or agent is known, and no card can be raised.
        Box::new(|| Hosts {
            host: Arc::new(UnconfiguredHost::new()),
            catalog: Arc::new(EmptyCatalog),
            consent: Arc::new(UnconfiguredConsent),
            hooks: Arc::new(NoHooks),
        }),
    );
    match run(handler) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("module exited: {e}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn run(handler: BasalHandler) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    subc_client_rs::serve(manifest(), handler).await?;
    Ok(())
}
