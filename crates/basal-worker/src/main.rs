//! Entry point for `ck-basal-worker`.
//!
//! Started by basal with an explicit Landlock policy on Linux, the worker confines itself and then
//! serves frames on stdin and stdout. Logs go to stderr. The other modes are
//! `--confinement-probe`, which reports what the sandbox denies, and
//! `--version`, which prints the version and the build revision.

use std::io::{self, BufWriter};
use std::process::ExitCode;

use basal_worker::{confinement, probe, serve};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {}
        Some("--confinement-probe") => return ExitCode::from(probe::run(&args[1..])),
        Some("--version") if args.len() == 1 => {
            println!(
                "ck-basal-worker {} ({}{})",
                env!("CARGO_PKG_VERSION"),
                env!("BASAL_BUILD_GIT_SHA"),
                if env!("BASAL_BUILD_GIT_DIRTY") == "true" {
                    ", dirty"
                } else {
                    ""
                }
            );
            return ExitCode::SUCCESS;
        }
        #[cfg(target_os = "linux")]
        Some("--landlock=required" | "--landlock=optional") if args.len() == 1 => {}
        Some(other) => {
            eprintln!("ck-basal-worker: unknown argument {other}");
            return ExitCode::from(64);
        }
    }

    // Confinement comes before the first read: nothing the parent sends is
    // ever processed by an unconfined worker.
    #[cfg(target_os = "linux")]
    let result = match confinement::linux::engine_arguments(&args) {
        Ok(landlock) => confinement::linux::enter(&confinement::linux::Options {
            landlock,
            ..Default::default()
        }),
        Err(confinement::linux::ArgumentError::Missing) => Err(
            confinement::ConfinementError::Linux("landlock-argument-missing"),
        ),
        Err(confinement::linux::ArgumentError::Usage) => return ExitCode::from(64),
    };
    #[cfg(not(target_os = "linux"))]
    let result = confinement::enter();
    let entered = match result {
        Ok(entered) => entered,
        Err(e) => {
            eprintln!("ck-basal-worker: refusing to run unconfined: {e}");
            return ExitCode::from(70);
        }
    };

    let reader = io::stdin().lock();
    let writer = BufWriter::new(io::stdout().lock());
    let exit = serve::serve(reader, writer, entered.confinement);
    if let serve::ServeExit::Broken(detail) = &exit {
        eprintln!("ck-basal-worker: channel broken: {detail}");
    }
    ExitCode::from(exit.exit_code())
}
