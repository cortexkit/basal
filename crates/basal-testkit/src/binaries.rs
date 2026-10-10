//! Executable names distinguish test processes from the placed production fleet.

use std::collections::HashMap;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Content-addressed copies of related executables, kept side by side for sibling lookup.
pub struct DevBinaries {
    directory: PathBuf,
}

impl DevBinaries {
    pub fn new(binaries: &[&Path]) -> io::Result<Self> {
        if binaries.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one executable is required",
            ));
        }

        let mut sources = binaries
            .iter()
            .map(|binary| {
                let name = binary
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| io::Error::other("executable has no UTF-8 file name"))?;
                let directory = binary.parent().unwrap_or_else(|| Path::new("."));
                Ok((
                    directory.to_path_buf(),
                    name.to_owned(),
                    binary.to_path_buf(),
                ))
            })
            .collect::<io::Result<Vec<_>>>()?;
        sources.sort_by(|left, right| left.1.cmp(&right.1));

        let source_directory = sources[0].0.clone();
        let mut digest = blake3::Hasher::new();
        // Sorting filenames gives a module-and-worker pair a stable byte order.
        // Related artifacts can live in separate Cargo output directories.
        for (_, _, source) in &sources {
            let mut file = File::open(source)?;
            let mut buffer = [0; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
        }
        let digest = digest.finalize();
        let digest_name = digest.to_hex().to_string()[..16].to_owned();
        // Old digests accumulate under target/ for reuse until cargo clean removes them.
        let directory = source_directory.join("ckdev-exec").join(digest_name);
        fs::create_dir_all(&directory)?;

        let placed = Self { directory };
        for (_, _, source) in &sources {
            let destination = placed.path(source)?;
            publish_executable(source, &destination)?;
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

fn publish_executable(source: &Path, destination: &Path) -> io::Result<()> {
    if destination.is_file() {
        return Ok(());
    }

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = destination
        .file_name()
        .ok_or_else(|| io::Error::other("executable destination has no file name"))?;
    let (temporary, mut output) = loop {
        let temporary = destination.with_file_name(format!(
            ".{}.{}.{}.tmp",
            name.to_string_lossy(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(output) => break (temporary, output),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        let mut input = File::open(source)?;
        io::copy(&mut input, &mut output)?;
        set_executable_mode(&output)?;
        output.sync_all()?;
        match rename_without_replacing(&temporary, destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && destination.is_file() => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(unix)]
fn set_executable_mode(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable_mode(_: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn rename_without_replacing(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in source path"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in destination path"))?;
    // RENAME_EXCL makes publication atomic without replacing another process's copy.
    let result = unsafe {
        // SAFETY: both paths are live NUL-terminated strings for this call.
        libc::renameatx_np(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn rename_without_replacing(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in source path"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in destination path"))?;
    let result = unsafe {
        // SAFETY: both paths are live NUL-terminated strings for this call.
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_without_replacing(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
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
        let suffix = if cfg!(windows) { ".exe" } else { "" };
        let worker_name = format!("ck-basal-worker{suffix}");
        let module_name = format!("ck-basal{suffix}");
        let worker = binary.with_file_name(&worker_name);
        let module = binary.with_file_name(&module_name);
        let mut sources = vec![binary];
        if binary
            .file_name()
            .is_some_and(|name| name == module_name.as_str())
            && worker.is_file()
        {
            sources.push(&worker);
        } else if binary
            .file_name()
            .is_some_and(|name| name == worker_name.as_str())
            && module.is_file()
        {
            sources.insert(0, &module);
        }
        DevBinaries::new(&sources).expect("place development executables")
    });
    placed.path(binary).expect("development executable path")
}

/// Places a test worker and makes that directory launchable by the Windows
/// AppContainer. Production installation never uses this test-only ACL grant.
pub(crate) fn dev_worker_binary(binary: impl AsRef<Path>) -> PathBuf {
    let binary = dev_binary(binary);
    #[cfg(windows)]
    {
        use std::collections::HashSet;
        static GRANTED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
        let directory = binary.parent().expect("worker directory").to_path_buf();
        let mut granted = GRANTED.get_or_init(Mutex::default).lock().unwrap();
        if !granted.contains(&directory) {
            let package = basal_launch::create_or_open_profile().expect("test worker package SID");
            basal_launch::grant_test_binary_directory(&binary, &package)
                .expect("test worker directory ACL");
            granted.insert(directory);
        }
    }
    binary
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SourceFiles(PathBuf);

    impl SourceFiles {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "basal-dev-binaries-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&directory).unwrap();
            Self(directory)
        }

        fn executable(&self, name: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for SourceFiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn module_and_worker_share_a_content_addressed_development_directory() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let source = SourceFiles::new();
        let worker_source = SourceFiles::new();
        let module = source.executable("ck-basal", b"module bytes");
        let worker = worker_source.executable("ck-basal-worker", b"worker bytes");
        let placed = DevBinaries::new(&[&module, &worker]).unwrap();
        for (original, name) in [(&module, "ckdev-basal"), (&worker, "ckdev-basal-worker")] {
            let path = placed.path(original).unwrap();
            assert_eq!(path.file_name().unwrap(), name);
            assert_eq!(fs::read(&path).unwrap(), fs::read(original).unwrap());
            assert_eq!(path.parent(), Some(placed.directory.as_path()));
            #[cfg(unix)]
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn repeated_placement_reuses_the_same_file_and_inode() {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let source = SourceFiles::new();
        let binary = source.executable("ck-basal-worker", b"same executable bytes");
        let first = DevBinaries::new(&[&binary]).unwrap().path(&binary).unwrap();
        let second = DevBinaries::new(&[&binary]).unwrap().path(&binary).unwrap();
        assert_eq!(first, second);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(first).unwrap().ino(),
            fs::metadata(second).unwrap().ino()
        );
        #[cfg(windows)]
        assert_eq!(file_identity(&first), file_identity(&second));
    }

    #[cfg(windows)]
    fn file_identity(path: &Path) -> (u32, u32, u32) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let file = File::open(path).unwrap();
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: file owns the handle and info is writable for the call.
        assert_ne!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) },
            0
        );
        (
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        )
    }

    #[test]
    fn changed_binary_bytes_use_a_different_directory() {
        let source = SourceFiles::new();
        let binary = source.executable("ck-basal-worker", b"first build");
        let first = DevBinaries::new(&[&binary]).unwrap().path(&binary).unwrap();
        fs::write(&binary, b"second build with changed bytes").unwrap();
        let second = DevBinaries::new(&[&binary]).unwrap().path(&binary).unwrap();
        assert_ne!(first.parent(), second.parent());
        assert_eq!(
            fs::read(second).unwrap(),
            b"second build with changed bytes"
        );
    }

    #[test]
    fn pre_existing_complete_copy_is_reused_without_rewriting() {
        use std::time::{Duration, SystemTime};

        let source = SourceFiles::new();
        let binary = source.executable("ck-basal-worker", b"reusable executable bytes");
        let first = DevBinaries::new(&[&binary]).unwrap().path(&binary).unwrap();
        let timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        File::options()
            .write(true)
            .open(&first)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(timestamp))
            .unwrap();
        let before = fs::metadata(&first).unwrap().modified().unwrap();

        let second = DevBinaries::new(&[&binary]).unwrap().path(&binary).unwrap();
        let after = fs::metadata(second).unwrap().modified().unwrap();
        assert_eq!(
            after, before,
            "an existing executable copy must not be rewritten"
        );
    }
}
