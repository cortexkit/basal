//! Shared helpers for the worker's contract tests.
//!
//! The tests drive the real `ck-basal-worker` binary through the test parent.
//! Set `BASAL_WORKER_BIN` to run the suite against another build of it, for
//! example the signed and placed binary.

#![allow(dead_code)]

use std::path::PathBuf;

use basal_proto::{ActivationResult, Failure, Nondeterminism};
use basal_testkit::{Ending, Report, TestParent};

pub fn worker_binary() -> PathBuf {
    basal_testkit::dev_binary(
        std::env::var_os("BASAL_WORKER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ck-basal-worker"))),
    )
}

pub fn parent() -> TestParent {
    TestParent::new(worker_binary())
}

pub fn finished(report: &Report) -> &ActivationResult {
    match &report.ending {
        Ending::Finished(result) => result,
        other => panic!("activation did not finish: {other:?}\n{report:#?}"),
    }
}

pub fn nondeterminism(report: &Report) -> &Nondeterminism {
    match finished(report) {
        ActivationResult::Failed(Failure::Nondeterminism(n)) => n,
        other => panic!("expected a nondeterminism failure, got {other:?}"),
    }
}

pub fn script_error(report: &Report) -> &str {
    match finished(report) {
        ActivationResult::Failed(
            Failure::Script { message } | Failure::ScriptHostRejection { message, .. },
        ) => message,
        other => panic!("expected a script failure, got {other:?}"),
    }
}
