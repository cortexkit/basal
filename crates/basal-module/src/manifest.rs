//! The subc manifest, built in Rust so the binary and its declaration cannot
//! drift: `ck-basal --manifest` prints exactly what `serve` registers.

use serde_json::json;
use subc_protocol::PROTOCOL_VERSION;
use subc_protocol::manifest::{
    BuildGitShaSource, Concurrency, GitTreeState, ManagementOperation, ManagementOperationKind,
    ManifestProvenance, ModuleManifest, ProviderRole, build_provenance_from_source,
};

/// The id the daemon serves basal under. subc stamps a reserved module's
/// routes with `reserved:<module id>`, so this is what makes basal's
/// principal in the fleet `reserved:basal`. The daemon also derives the
/// module's store path from it, so it never changes once deployed.
pub const MODULE_ID: &str = "basal";

/// Every op basal serves, with whether it changes anything and what it does.
pub const OPERATIONS: &[(&str, ManagementOperationKind, &str)] = &[
    (
        "package.register",
        ManagementOperationKind::Mutate,
        "Register immutable package bytes; operator or core only.",
    ),
    (
        "package.get",
        ManagementOperationKind::Query,
        "Return exact registered package bytes; core only.",
    ),
    (
        "flow.instance.ensure",
        ManagementOperationKind::Mutate,
        "Ensure an agent's package version, ordered by generation; core only.",
    ),
    (
        "flow.instance.remove",
        ManagementOperationKind::Mutate,
        "Mark an instance removed without changing enable state; core only.",
    ),
    (
        "flow.install",
        ManagementOperationKind::Mutate,
        "Validate a flow version, run its capture-only dry run and raise its consent card; returns the pending install.",
    ),
    (
        "flow.dry_run",
        ManagementOperationKind::Query,
        "Run a flow against a schedule window or a synthetic trigger in an isolated scratch store and return the trace of every call and sink write.",
    ),
    (
        "flow.list",
        ManagementOperationKind::Query,
        "List caller-visible flows and their approval, disable and last-run state.",
    ),
    (
        "flow.health",
        ManagementOperationKind::Query,
        "Per-flow health (state, last run, overdue work, consecutive failures, runs needing reconcile) and the runtime's own figures.",
    ),
    (
        "flow.reconcile",
        ManagementOperationKind::Mutate,
        "Resolve a call whose outcome is unknown: an observed result, not applied, or cancel the run.",
    ),
    (
        "flow.drain",
        ManagementOperationKind::Mutate,
        "Stop (or resume) admitting runs and list the runs not yet finished, before an engine-affecting upgrade.",
    ),
    (
        "flow.disable",
        ManagementOperationKind::Mutate,
        "Disable a flow at once: no new runs, no new calls from runs in flight.",
    ),
    (
        "flow.enable",
        ManagementOperationKind::Mutate,
        "Enable a disabled flow; its schedule resumes from the next due time.",
    ),
];

pub fn manifest() -> ModuleManifest {
    ModuleManifest::builder(MODULE_ID, env!("CARGO_PKG_VERSION"))
        .protocol_ver(PROTOCOL_VERSION)
        .provides(vec![ProviderRole::ManagementSurface {
            operations: OPERATIONS
                .iter()
                .map(|(name, kind, description)| ManagementOperation {
                    name: (*name).to_owned(),
                    kind: kind.clone(),
                    description: Some((*description).to_owned()),
                })
                .collect(),
            config_schema: json!({"type": "object"}),
            observability: Vec::new(),
            // Answers depend on who calls (an agent sees and changes only
            // its own flows), but that comes from the daemon's stamp on the
            // route, not from the bind identity's project or session.
            identity_scope: Vec::new(),
            // Ops run on blocking threads beside the engine, behind the
            // store's own serialisation, so concurrent delivery is safe.
            concurrency: Concurrency::ModuleManaged,
        }])
        // No capability names yet: nothing in the fleet requires a flow
        // engine, and a provides claim belongs with a reviewed registry
        // entry, not ahead of it.
        .capabilities(None)
        // basal declares no self-signals: its effects are its flows', and
        // those are reported through flow.health.
        .self_signals(None)
        .provenance(Some(build_provenance()))
        .build()
}

/// The commit this binary was built from, as `build.rs` embedded it.
pub const BUILD_GIT_SHA: &str = env!("BASAL_BUILD_GIT_SHA");
/// Whether that commit's tree had uncommitted changes. Anything but an
/// explicit clean build counts as dirty.
pub const BUILD_GIT_DIRTY: bool = matches!(env!("BASAL_BUILD_GIT_DIRTY").as_bytes(), b"true");

/// The build's revision for the manifest. `script/stage.sh` builds from a
/// clean tree with the commit set, so a staged binary declares it; any other
/// build (cargo test, the rig without a revision) declares that it never
/// derived one, and a dirty build declares none, which is subc-protocol's
/// rule for a tree the commit does not describe.
pub fn build_provenance() -> ManifestProvenance {
    let source = if BUILD_GIT_SHA == "unknown" {
        BuildGitShaSource::NeverDerived
    } else {
        BuildGitShaSource::Git {
            revision: BUILD_GIT_SHA,
            tree_state: if BUILD_GIT_DIRTY {
                GitTreeState::Dirty
            } else {
                GitTreeState::Clean
            },
        }
    };
    let lock_digest = Some(env!("BASAL_BUILD_LOCK_DIGEST")).filter(|d| *d != "unknown");
    // The launch nonce's source is left unset: subc-client-rs fills it in at
    // HELLO, from the nonce it actually read.
    build_provenance_from_source(source, lock_digest, None)
        .expect("build.rs embeds only canonical hex digests or unknown")
}

/// `ck-basal --version`: the crate version and the embedded build revision.
pub fn version_line() -> String {
    format!(
        "ck-basal {} ({}{})",
        env!("CARGO_PKG_VERSION"),
        BUILD_GIT_SHA,
        if BUILD_GIT_DIRTY { ", dirty" } else { "" }
    )
}
