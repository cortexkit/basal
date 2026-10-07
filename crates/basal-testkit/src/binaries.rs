//! Executable names distinguish test processes from the placed production fleet.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Scratch copies of related executables, kept side by side for sibling lookup.
/// Hard links preserve the signed bytes and inode; copying also works across
/// filesystems. The directory lives until this owner is dropped.
pub struct DevBinaries {
    directory: PathBuf,
}

impl DevBinaries {
    pub fn new(binaries: &[&Path]) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = loop {
            let directory = std::env::temp_dir().join(format!(
                "basal-dev-binaries-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&directory) {
                Ok(()) => break directory,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        let placed = Self { directory };
        for binary in binaries {
            link_or_copy(binary, &placed.path(binary)?, |source, destination| {
                std::fs::hard_link(source, destination)
            })?;
        }
        Ok(placed)
    }

    pub fn path(&self, source: &Path) -> io::Result<PathBuf> {
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("executable has no UTF-8 file name"))?;
        let name = match name.strip_prefix("ck-") {
            Some(suffix) => format!("ckdev-{suffix}"),
            None => name.to_owned(),
        };
        Ok(self.directory.join(name))
    }
}

impl Drop for DevBinaries {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn link_or_copy(
    source: &Path,
    destination: &Path,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    if link(source, destination).is_err() {
        std::fs::copy(source, destination)?;
    }
    Ok(())
}

/// A process-lifetime scratch executable for callers whose API returns a path.
/// Already-development executables need no new copy. Cache by source path so
/// repeated worker launches do not create a scratch directory per activation.
pub fn dev_binary(binary: impl AsRef<Path>) -> PathBuf {
    let binary = binary.as_ref();
    if !binary
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("ck-"))
    {
        return binary.to_path_buf();
    }
    static COPIES: OnceLock<Mutex<HashMap<PathBuf, DevBinaries>>> = OnceLock::new();
    let mut copies = COPIES.get_or_init(Mutex::default).lock().unwrap();
    let placed = copies.entry(binary.to_path_buf()).or_insert_with(|| {
        let worker = binary.with_file_name("ck-basal-worker");
        let mut sources = vec![binary];
        if binary.file_name().is_some_and(|name| name == "ck-basal") && worker.is_file() {
            sources.push(&worker);
        }
        DevBinaries::new(&sources).expect("place development executables")
    });
    placed.path(binary).expect("development executable path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_and_worker_are_linked_side_by_side_with_development_names() {
        use std::os::unix::fs::MetadataExt;

        let source = DevBinaries::new(&[]).unwrap();
        let module = source.directory.join("ck-basal");
        let worker = source.directory.join("ck-basal-worker");
        std::fs::write(&module, b"module bytes").unwrap();
        std::fs::write(&worker, b"worker bytes").unwrap();
        let placed = DevBinaries::new(&[&module, &worker]).unwrap();
        for (original, name) in [(&module, "ckdev-basal"), (&worker, "ckdev-basal-worker")] {
            let path = placed.path(original).unwrap();
            assert_eq!(path.file_name().unwrap(), name);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                std::fs::read(original).unwrap()
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().ino(),
                std::fs::metadata(original).unwrap().ino()
            );
        }
        let directory = placed.directory.clone();
        drop(placed);
        assert!(!directory.exists());
    }

    #[test]
    fn failed_hard_link_falls_back_to_an_executable_copy() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let source = DevBinaries::new(&[]).unwrap();
        let binary = source.directory.join("ck-basal-worker");
        std::fs::write(&binary, b"signed worker bytes").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let placed = DevBinaries::new(&[]).unwrap();
        let destination = placed.path(&binary).unwrap();
        link_or_copy(&binary, &destination, |_, _| {
            Err(io::Error::from_raw_os_error(libc::EXDEV))
        })
        .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"signed worker bytes");
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_ne!(
            std::fs::metadata(&destination).unwrap().ino(),
            std::fs::metadata(&binary).unwrap().ino()
        );
    }
}
