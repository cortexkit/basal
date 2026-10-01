//! Entry point for `ck-basal-worker`.
//!
//! Started by basal with no arguments, the worker confines itself and then
//! serves frames on stdin and stdout. Logs go to stderr. The only other mode
//! is `--confinement-probe`, which reports what the sandbox denies.

use std::io::{self, BufWriter};
use std::process::ExitCode;

use basal_worker::{confinement, probe, serve};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {}
        Some("--confinement-probe") => return ExitCode::from(probe::run(&args[1..])),
        Some(other) => {
            eprintln!("ck-basal-worker: unknown argument {other}");
            return ExitCode::from(64);
        }
    }

    // Confinement comes before the first read: nothing the parent sends is
    // ever processed by an unconfined worker.
    let entered = match confinement::enter() {
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
