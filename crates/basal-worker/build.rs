//! Embeds the commit this binary was built from, for
//! `ck-basal-worker --version`.
//!
//! The revision comes from `CK_BUILD_GIT_SHA` and the tree state from
//! `CK_BUILD_GIT_DIRTY`, which `script/stage.sh` sets. A build without them
//! reports the revision as `unknown`, and only an explicit
//! `CK_BUILD_GIT_DIRTY=false` certifies a clean tree. The same rule is in
//! basal-module's build script; the worker keeps its own copy because it
//! must not depend on the module's crates (`tests/dependency_fence.rs`).
//!
//! On Windows with the MSVC linker it also sets the worker's main-thread
//! stack reserve to 8 MiB, the size of the main thread's stack on macOS.
//! The worker runs JavaScript on its main thread with a QuickJS stack budget
//! of up to 4 MiB (`MAX_STACK_BYTES` in `src/engine.rs`); under the linker's
//! default 1 MiB reserve a deep recursion overflows the native stack and
//! kills the process before QuickJS can report the exhausted stack budget.
//! A reserve is address space, not committed memory, so the Windows job's
//! commit limit does not count it. `tests/windows_image.rs` checks the
//! reserve in the built image.

/// The worker's main-thread stack reserve on Windows, in bytes.
const WINDOWS_STACK_RESERVE: u64 = 8 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-env-changed=CK_BUILD_GIT_SHA");
    println!("cargo:rerun-if-env-changed=CK_BUILD_GIT_DIRTY");
    let revision = std::env::var("CK_BUILD_GIT_SHA").ok().filter(|revision| {
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    let dirty = std::env::var("CK_BUILD_GIT_DIRTY").as_deref() != Ok("false");
    println!(
        "cargo:rustc-env=BASAL_BUILD_GIT_SHA={}",
        revision.as_deref().unwrap_or("unknown")
    );
    println!("cargo:rustc-env=BASAL_BUILD_GIT_DIRTY={dirty}");

    // The build script runs on the host, so the target is read from Cargo's
    // environment rather than from `cfg!`.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        println!("cargo:rustc-link-arg-bin=ck-basal-worker=/STACK:{WINDOWS_STACK_RESERVE}");
    }
}
