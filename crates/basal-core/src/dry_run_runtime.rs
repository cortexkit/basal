//! Ephemeral inspection for dry runs; ordinary runtimes keep summaries only.
use crate::{Config, CoreError, Durability, Hooks, Result, Runtime, Store, channel::WorkerSource};
use std::{
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Owns both a scratch runtime and the fresh directory it exclusively created.
/// Dropping it joins dispatch threads, closes the store/log, then removes detail.
pub struct ScratchRuntime {
    runtime: Option<Runtime>,
    directory: PathBuf,
}
impl Deref for ScratchRuntime {
    type Target = Runtime;
    fn deref(&self) -> &Runtime {
        self.runtime.as_ref().expect("initialized scratch runtime")
    }
}
impl Drop for ScratchRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.quiesce();
            drop(runtime);
        }
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            tracing::warn!(path=%self.directory.display(),%error,"removing dry-run scratch directory");
        }
    }
}
impl Runtime {
    /// Creates its own child directory and store. An existing Store is never
    /// accepted, so scratch detail retention cannot be enabled on production.
    pub fn scratch_for_dry_run(
        root: &Path,
        host: Arc<dyn basal_host::Host>,
        catalog: Arc<dyn basal_host::Catalog>,
        hooks: Arc<dyn Hooks>,
        source: Option<Arc<dyn WorkerSource>>,
        config: Config,
    ) -> Result<ScratchRuntime> {
        std::fs::create_dir_all(root).map_err(|e| CoreError::Store(e.to_string()))?;
        let mut owned = None;
        for _ in 0..16 {
            let mut entropy = [0u8; 8];
            getrandom::fill(&mut entropy).map_err(|e| CoreError::Store(e.to_string()))?;
            let directory = root.join(format!("inspection-{}", crate::ids::hex(&entropy)));
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    owned = Some(directory);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(CoreError::Store(e.to_string())),
            }
        }
        let directory = owned
            .ok_or_else(|| CoreError::Store("cannot allocate fresh dry-run directory".into()))?;
        let mut scratch = ScratchRuntime {
            runtime: None,
            directory,
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&scratch.directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| CoreError::Store(e.to_string()))?;
        }
        let store = Arc::new(Store::open(
            scratch.directory.join("scratch.db"),
            Durability::default(),
        )?);
        let runtime = Self::new(store, host, catalog, hooks, source, config)?;
        runtime
            .shared
            .staged_hosts
            .as_ref()
            .expect("recovered scratch registry")
            .registry()
            .retain_for_scratch();
        scratch.runtime = Some(runtime);
        Ok(scratch)
    }
}
