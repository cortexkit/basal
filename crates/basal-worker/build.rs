//! Embeds the commit this binary was built from, for
//! `ck-basal-worker --version`.
//!
//! The revision comes from `CK_BUILD_GIT_SHA` and the tree state from
//! `CK_BUILD_GIT_DIRTY`, which `script/stage.sh` sets. A build without them
//! reports the revision as `unknown`, and only an explicit
//! `CK_BUILD_GIT_DIRTY=false` certifies a clean tree. The same rule is in
//! basal-module's build script; the worker keeps its own copy because it
//! must not depend on the module's crates (`tests/dependency_fence.rs`).

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
}
