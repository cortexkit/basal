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
        let mut digest = Sha256::new();
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
        let digest = digest.finish();
        let digest_name = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
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

struct Sha256 {
    state: [u32; 8],
    length: u64,
    block: [u8; 64],
    used: usize,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            length: 0,
            block: [0; 64],
            used: 0,
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len() as u64);
        if self.used != 0 {
            let count = (64 - self.used).min(bytes.len());
            self.block[self.used..self.used + count].copy_from_slice(&bytes[..count]);
            self.used += count;
            bytes = &bytes[count..];
            if self.used == 64 {
                let block = self.block;
                self.compress(&block);
                self.used = 0;
            } else {
                return;
            }
        }
        while bytes.len() >= 64 {
            let block: &[u8; 64] = bytes[..64].try_into().expect("fixed-size SHA-256 block");
            self.compress(block);
            bytes = &bytes[64..];
        }
        self.block[..bytes.len()].copy_from_slice(bytes);
        self.used = bytes.len();
    }

    fn finish(mut self) -> [u8; 32] {
        let bit_length = self.length.wrapping_mul(8);
        self.block[self.used] = 0x80;
        self.used += 1;
        if self.used > 56 {
            self.block[self.used..].fill(0);
            let block = self.block;
            self.compress(&block);
            self.block = [0; 64];
        } else {
            self.block[self.used..56].fill(0);
        }
        self.block[56..].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.block;
        self.compress(&block);

        let mut output = [0; 32];
        for (chunk, word) in output.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        output
    }

    fn compress(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut schedule = [0u32; 64];
        for (index, word) in schedule.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(block[index * 4..index * 4 + 4].try_into().unwrap());
        }
        for index in 16..64 {
            let x = schedule[index - 15];
            let y = schedule[index - 2];
            let s0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
            let s1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(schedule[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            [a, b, c, d, e, f, g, h] = [
                temp1.wrapping_add(temp2),
                a,
                b,
                c,
                d.wrapping_add(temp1),
                e,
                f,
                g,
            ];
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }
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
        let module = binary.with_file_name("ck-basal");
        let mut sources = vec![binary];
        if binary.file_name().is_some_and(|name| name == "ck-basal") && worker.is_file() {
            sources.push(&worker);
        } else if binary
            .file_name()
            .is_some_and(|name| name == "ck-basal-worker")
            && module.is_file()
        {
            sources.insert(0, &module);
        }
        DevBinaries::new(&sources).expect("place development executables")
    });
    placed.path(binary).expect("development executable path")
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
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn repeated_placement_reuses_the_same_file_and_inode() {
        use std::os::unix::fs::MetadataExt;

        let source = SourceFiles::new();
        let binary = source.executable("ck-basal-worker", b"same executable bytes");
        let first = DevBinaries::new(&[&binary]).unwrap().path(&binary).unwrap();
        let second = DevBinaries::new(&[&binary]).unwrap().path(&binary).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs::metadata(first).unwrap().ino(),
            fs::metadata(second).unwrap().ino()
        );
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

    #[test]
    fn sha256_matches_the_standard_empty_input_vector() {
        let digest = Sha256::new().finish();
        let rendered = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            rendered,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_matches_a_non_empty_standard_vector() {
        let mut digest = Sha256::new();
        digest.update(b"abc");
        let rendered = digest
            .finish()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            rendered,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_matches_a_multi_block_standard_vector() {
        let mut digest = Sha256::new();
        let input = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
        for chunk in input.chunks(17) {
            digest.update(chunk);
        }
        let rendered = digest
            .finish()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            rendered,
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }
}
