//! Embeds the commit this binary was built from, for the manifest's build
//! provenance and `ck-basal --version`.
//!
//! The revision comes from `CK_BUILD_GIT_SHA` and the tree state from
//! `CK_BUILD_GIT_DIRTY`, the fleet's names, which `script/stage.sh` sets (as
//! prefrontal's deploy script does). A build without them, such as a plain
//! `cargo build` or `cargo test`, declares no revision rather than guessing
//! one, and only an explicit `CK_BUILD_GIT_DIRTY=false` certifies a clean
//! tree.

use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rerun-if-env-changed=CK_BUILD_GIT_SHA");
    println!("cargo:rerun-if-env-changed=CK_BUILD_GIT_DIRTY");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    let revision = std::env::var("CK_BUILD_GIT_SHA").ok().filter(|revision| {
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    let dirty = std::env::var("CK_BUILD_GIT_DIRTY").as_deref() != Ok("false");
    println!(
        "cargo:rustc-env=BASAL_BUILD_GIT_SHA={}",
        revision.as_deref().unwrap_or("unknown")
    );
    println!("cargo:rustc-env=BASAL_BUILD_GIT_DIRTY={dirty}");
    let lock_digest = std::fs::read("../../Cargo.lock")
        .ok()
        .map(|contents| format!("{:x}", Sha256::digest(contents)));
    println!(
        "cargo:rustc-env=BASAL_BUILD_LOCK_DIGEST={}",
        lock_digest.as_deref().unwrap_or("unknown")
    );
}
