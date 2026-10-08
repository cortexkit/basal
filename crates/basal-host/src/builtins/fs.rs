//! `fs.read`, `fs.list`, `fs.stat` and `fs.write` over the manifest's roots.
//!
//! Scope: a path is resolved with `realpath` (for a path that does not
//! exist, its parent is resolved and the last component kept) and must lie
//! under one of the roots of the call's kind, each resolved the same way.
//! Resolution follows every symlink and removes every `..`, so an escape
//! through either lands outside the roots and is refused.
//!
//! The race: the path is resolved first and opened after, and someone who
//! can write inside a root could swap a component for a symlink between the
//! two. Two things close it where macOS allows:
//! - the last component is opened with `O_NOFOLLOW`, so a symlink swapped
//!   in there fails the open instead of being followed;
//! - after the open, the kernel is asked where the opened file or directory
//!   really is (`F_GETPATH`), and that path must still lie under a root. A
//!   directory higher up swapped for a symlink is caught here, because the
//!   opened file's real path then lies outside.
//!
//! `fs.stat` and `fs.write` open the parent directory and act relative to
//! that verified descriptor (`fstatat`, `openat`, `renameat`), so the name
//! they act on is in the directory that was checked. What this cannot
//! close: a file hard-linked into a root from outside is inside the root
//! by every path the kernel reports, so it is read like any other; and a
//! swap that happens after the open changes nothing for this call, which
//! already holds the checked file.

#[cfg(test)]
mod tests;

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
#[cfg(not(target_os = "linux"))]
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
#[cfg(not(target_os = "linux"))]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use super::{Denial, codes, expand_home};

#[cfg(target_os = "linux")]
pub use linux::{
    KernelCapability, ResolvedPath, Target, open_checked, open_checked_with_capability, resolve,
    stat,
};

/// `fs.read`'s default cap.
pub const DEFAULT_READ_BYTES: u64 = super::MAX_TEXT_RESULT_BYTES as u64;
/// The largest cap `fs.read` accepts.
pub const MAX_READ_BYTES: u64 = DEFAULT_READ_BYTES;
/// The largest text `fs.write` writes.
pub const MAX_WRITE_BYTES: usize = 1024 * 1024;
/// The most entries `fs.list` returns.
pub const MAX_LIST_ENTRIES: usize = 1000;
/// The longest path accepted.
pub const MAX_PATH_BYTES: usize = 4096;

/// What a path is resolved for. A write's last component is never
/// followed: the file is replaced, so its parent is what must be in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Read,
    Write,
}

/// Where a path resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(not(target_os = "linux"))]
pub enum Target {
    /// It exists; this is its real path.
    Existing(PathBuf),
    /// It does not exist (or, for a write, is about to be replaced): the
    /// real path of its parent, and its last component.
    Entry { parent: PathBuf, name: OsString },
}

/// The roots, each resolved to its real path. A root that does not exist
/// now grants nothing.
#[cfg(not(target_os = "linux"))]
fn real_roots(roots: &[String]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|r| expand_home(r))
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect()
}

/// Whether `path` lies under one of `roots` (component by component, so
/// `/a/bc` is not under `/a/b`).
pub fn inside(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn outside(_path: &Path) -> Denial {
    Denial::denied("path is outside the manifest's roots or missing")
}

fn io_denial(path: &Path, e: &std::io::Error) -> Denial {
    match e.kind() {
        std::io::ErrorKind::NotFound => Denial::new(
            codes::NOT_FOUND,
            format!("{} does not exist", path.display()),
        ),
        _ => Denial::new(codes::IO, format!("{}: {e}", path.display())),
    }
}

/// The script's path as an absolute path, with `~` expanded.
fn absolute(path: &str) -> Result<PathBuf, Denial> {
    if path.len() > MAX_PATH_BYTES || path.contains('\0') {
        return Err(Denial::invalid(format!(
            "a path is at most {MAX_PATH_BYTES} bytes with no NUL"
        )));
    }
    expand_home(path).ok_or_else(|| Denial::invalid(format!("{path:?} is not an absolute path")))
}

/// Resolves `path` and requires it to lie under one of `roots`.
#[cfg(not(target_os = "linux"))]
pub fn resolve(path: &str, roots: &[String], purpose: Purpose) -> Result<Target, Denial> {
    let roots = real_roots(roots);
    resolve_real(path, &roots, purpose)
}

#[cfg(not(target_os = "linux"))]
fn resolve_real(path: &str, roots: &[PathBuf], purpose: Purpose) -> Result<Target, Denial> {
    let path = absolute(path)?;
    if purpose == Purpose::Read {
        match std::fs::canonicalize(&path) {
            Ok(real) => {
                return if inside(&real, roots) {
                    Ok(Target::Existing(real))
                } else {
                    Err(outside(&real))
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(outside(&path)),
        }
    }
    // A missing path, or a write: resolve the parent and keep the name.
    let name = match path.components().next_back() {
        Some(Component::Normal(name)) => name.to_owned(),
        _ => {
            return Err(Denial::invalid(format!(
                "{} does not name an entry in a directory",
                path.display()
            )));
        }
    };
    let parent = path.parent().unwrap_or(Path::new("/"));
    let real_parent = std::fs::canonicalize(parent).map_err(|_| outside(&path))?;
    if inside(&real_parent.join(&name), roots) {
        Ok(Target::Entry {
            parent: real_parent,
            name,
        })
    } else {
        Err(outside(&real_parent.join(&name)))
    }
}

/// Where the kernel says an open descriptor's file is.
#[cfg(not(target_os = "linux"))]
fn fd_path(fd: RawFd) -> std::io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
        // SAFETY: F_GETPATH writes at most PATH_MAX bytes, NUL included,
        // into a buffer larger than that.
        let r = unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) };
        if r == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        let os: OsString = std::os::unix::ffi::OsStringExt::from_vec(buf);
        Ok(PathBuf::from(os))
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_link(format!("/proc/self/fd/{fd}"))
    }
}

/// Requires the file behind `fd` (or, with `name`, the entry `name` in the
/// directory behind `fd`) to lie under one of `roots`.
#[cfg(not(target_os = "linux"))]
fn verify(fd: RawFd, name: Option<&OsStr>, roots: &[PathBuf]) -> Result<(), Denial> {
    let mut real = fd_path(fd).map_err(|e| Denial::new(codes::IO, e.to_string()))?;
    if let Some(name) = name {
        real.push(name);
    }
    if inside(&real, roots) {
        Ok(())
    } else {
        Err(outside(&real))
    }
}

/// Opens an already resolved path without following a symlink in its last
/// component, then checks where the opened file really is.
///
/// `resolved` is a real path from [`resolve`]; `roots` are the same roots
/// it was resolved against. A symlink swapped into the last component
/// since resolution fails the open; one swapped in higher up is caught by
/// the check after it.
#[cfg(not(target_os = "linux"))]
pub fn open_checked(resolved: &Path, roots: &[String], directory: bool) -> Result<File, Denial> {
    open_checked_real(resolved, &real_roots(roots), directory)
}

#[cfg(not(target_os = "linux"))]
fn open_checked_real(resolved: &Path, roots: &[PathBuf], directory: bool) -> Result<File, Denial> {
    let mut flags = libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    if directory {
        flags |= libc::O_DIRECTORY;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(resolved)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                Denial::denied(format!(
                    "{} became a symlink while it was being opened",
                    resolved.display()
                ))
            } else {
                io_denial(resolved, &e)
            }
        })?;
    verify(file.as_raw_fd(), None, roots)?;
    Ok(file)
}

/// `fs.read`: the file's text, refused over `max_bytes` or when not UTF-8.
pub fn read(path: &str, roots: &[String], max_bytes: u64) -> Result<Value, Denial> {
    #[cfg(not(target_os = "linux"))]
    let roots = real_roots(roots);
    let max_bytes = max_bytes.min(MAX_READ_BYTES);
    #[cfg(not(target_os = "linux"))]
    let target = resolve_real(path, &roots, Purpose::Read)?;
    #[cfg(target_os = "linux")]
    let target = resolve(path, roots, Purpose::Read)?;
    let real = match target {
        Target::Existing(real) => real,
        Target::Entry { parent, name } => {
            return Err(Denial::new(
                codes::NOT_FOUND,
                format!("{} does not exist", parent.join(name).display()),
            ));
        }
    };
    #[cfg(not(target_os = "linux"))]
    let file = open_checked_real(&real, &roots, false)?;
    #[cfg(target_os = "linux")]
    let file = open_checked(&real, roots, false)?;
    let meta = file.metadata().map_err(|e| io_denial(&real, &e))?;
    if !meta.is_file() {
        return Err(Denial::invalid(format!(
            "{} is not a regular file",
            real.display()
        )));
    }
    let too_large = || {
        Denial::new(
            codes::TOO_LARGE,
            format!("{} is larger than {max_bytes} bytes", real.display()),
        )
    };
    if meta.len() > max_bytes {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_denial(&real, &e))?;
    // The file can grow between the size check and the read.
    if bytes.len() as u64 > max_bytes {
        return Err(too_large());
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        Denial::new(
            codes::NOT_UTF8,
            format!("{} is not UTF-8 text", real.display()),
        )
    })?;
    Ok(json!({ "text": text }))
}

fn kind_of(mode: libc::mode_t) -> &'static str {
    match mode & libc::S_IFMT {
        libc::S_IFREG => "file",
        libc::S_IFDIR => "dir",
        libc::S_IFLNK => "symlink",
        _ => "other",
    }
}

fn c_name(name: &OsStr) -> Result<CString, Denial> {
    CString::new(name.as_bytes()).map_err(|_| Denial::invalid("a name with a NUL byte"))
}

/// `lstat` of `name` in the directory behind `dir`; `None` when it does
/// not exist.
fn stat_at(dir: RawFd, name: &OsStr) -> Result<Option<libc::stat>, Denial> {
    let c = c_name(name)?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `c` is a NUL-terminated name and `st` is large enough for a
    // `stat`; fstatat fills it in when it returns 0.
    let r = unsafe { libc::fstatat(dir, c.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    if r == 0 {
        // SAFETY: fstatat returned 0, so it initialised `st`.
        return Ok(Some(unsafe { st.assume_init() }));
    }
    let e = std::io::Error::last_os_error();
    if e.kind() == std::io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(Denial::new(codes::IO, e.to_string()))
    }
}

/// Opens the parent of an entry and checks the entry's real path.
#[cfg(not(target_os = "linux"))]
fn open_parent(parent: &Path, name: &OsStr, roots: &[PathBuf]) -> Result<File, Denial> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_DIRECTORY)
        .open(parent)
        .map_err(|e| io_denial(parent, &e))?;
    verify(file.as_raw_fd(), Some(name), roots)?;
    Ok(file)
}

/// `fs.stat`: `{exists, kind, size, mtime_ms}`. A path that does not exist
/// answers `{exists: false}` when its parent lies under a root.
/// Existing symlinks are followed by resolution; only a dangling symlink is
/// reported as `kind: "symlink"` by the descriptor-relative metadata lookup.
#[cfg(not(target_os = "linux"))]
pub fn stat(path: &str, roots: &[String]) -> Result<Value, Denial> {
    let roots = real_roots(roots);
    let (parent, name) = match resolve_real(path, &roots, Purpose::Read)? {
        Target::Existing(real) => match (real.parent(), real.file_name()) {
            (Some(parent), Some(name)) => (parent.to_owned(), name.to_owned()),
            // The file system root itself.
            _ => (real.clone(), OsString::from(".")),
        },
        Target::Entry { parent, name } => (parent, name),
    };
    let dir = open_parent(&parent, &name, &roots)?;
    let Some(st) = stat_at(dir.as_raw_fd(), &name)? else {
        return Ok(json!({ "exists": false }));
    };
    #[allow(clippy::unnecessary_cast)] // the field types differ between platforms
    let mtime_ms = (st.st_mtime as i64) * 1000 + (st.st_mtime_nsec as i64) / 1_000_000;
    Ok(json!({
        "exists": true,
        "kind": kind_of(st.st_mode),
        "size": st.st_size,
        "mtime_ms": mtime_ms,
    }))
}

/// `fs.list`: the directory's entries, sorted by name, refused over
/// [`MAX_LIST_ENTRIES`] or when a name is not UTF-8.
pub fn list(path: &str, roots: &[String]) -> Result<Value, Denial> {
    #[cfg(not(target_os = "linux"))]
    let roots = real_roots(roots);
    #[cfg(not(target_os = "linux"))]
    let target = resolve_real(path, &roots, Purpose::Read)?;
    #[cfg(target_os = "linux")]
    let target = resolve(path, roots, Purpose::Read)?;
    let real = match target {
        Target::Existing(real) => real,
        Target::Entry { parent, name } => {
            return Err(Denial::new(
                codes::NOT_FOUND,
                format!("{} does not exist", parent.join(name).display()),
            ));
        }
    };
    #[cfg(not(target_os = "linux"))]
    let dir = open_checked_real(&real, &roots, true)?;
    #[cfg(target_os = "linux")]
    let dir = open_checked(&real, roots, true)?;
    let names = read_dir_fd(dir.as_raw_fd()).map_err(|e| io_denial(&real, &e))?;
    if names.len() > MAX_LIST_ENTRIES {
        return Err(Denial::new(
            codes::TOO_LARGE,
            format!(
                "{} has more than {MAX_LIST_ENTRIES} entries",
                real.display()
            ),
        ));
    }
    let mut entries = Vec::with_capacity(names.len());
    for (name, d_type) in names {
        let kind = match d_type {
            libc::DT_REG => "file",
            libc::DT_DIR => "dir",
            libc::DT_LNK => "symlink",
            libc::DT_UNKNOWN => match stat_at(dir.as_raw_fd(), &name)? {
                Some(st) => kind_of(st.st_mode),
                None => continue,
            },
            _ => "other",
        };
        let Some(text) = name.to_str() else {
            return Err(Denial::new(
                codes::NOT_UTF8,
                format!("{} holds a name that is not UTF-8", real.display()),
            ));
        };
        entries.push((text.to_owned(), kind));
    }
    entries.sort();
    Ok(Value::Array(
        entries
            .into_iter()
            .map(|(name, kind)| json!({ "name": name, "kind": kind }))
            .collect(),
    ))
}

/// The entries of the directory behind `fd` (not `.` or `..`) with their
/// `d_type`, reading at most one more than [`MAX_LIST_ENTRIES`].
fn read_dir_fd(fd: RawFd) -> std::io::Result<Vec<(OsString, u8)>> {
    // fdopendir takes ownership of the descriptor it is given, so it gets
    // a duplicate and the caller keeps its own.
    // SAFETY: dup of a descriptor this function's caller holds open.
    let dup = unsafe { libc::dup(fd) };
    if dup == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `dup` is an open directory descriptor this function owns.
    let dir = unsafe { libc::fdopendir(dup) };
    if dir.is_null() {
        let e = std::io::Error::last_os_error();
        // SAFETY: fdopendir failed, so `dup` is still ours to close.
        unsafe { libc::close(dup) };
        return Err(e);
    }
    let mut out = Vec::new();
    loop {
        // SAFETY: `dir` is the open stream from fdopendir above.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            break;
        }
        // SAFETY: readdir returned a valid entry whose name is
        // NUL-terminated; both are read before the next readdir call.
        let (name, d_type) = unsafe {
            let name = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr());
            (
                OsStr::from_bytes(name.to_bytes()).to_owned(),
                (*entry).d_type,
            )
        };
        if name == "." || name == ".." {
            continue;
        }
        out.push((name, d_type));
        if out.len() > MAX_LIST_ENTRIES {
            break;
        }
    }
    // SAFETY: `dir` is open and is closed exactly once, here; closing it
    // also closes `dup`.
    unsafe { libc::closedir(dir) };
    Ok(out)
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn replacement_mode(mode: libc::mode_t) -> libc::mode_t {
    mode & 0o777
}

pub(super) fn check_write_size(bytes: usize) -> Result<(), Denial> {
    if bytes > MAX_WRITE_BYTES {
        Err(Denial::new(
            codes::TOO_LARGE,
            format!("{bytes} bytes exceed the write cap of {MAX_WRITE_BYTES}"),
        ))
    } else {
        Ok(())
    }
}

/// `fs.write`: replaces the whole file with `text`, atomically. The text
/// goes to a new temporary file in the same directory, which is flushed to
/// disk and then renamed over the target, so a reader sees the old file or
/// the new one and never a part. Writing the same text again leaves the
/// same file, which is why a lost reply may be sent again.
pub fn write(path: &str, roots: &[String], text: &str) -> Result<Value, Denial> {
    // Public callers may bypass the argument parser, so enforce the shared
    // write limit here as well as before authorization.
    check_write_size(text.len())?;
    #[cfg(not(target_os = "linux"))]
    let roots = real_roots(roots);
    #[cfg(not(target_os = "linux"))]
    let target = resolve_real(path, &roots, Purpose::Write)?;
    #[cfg(target_os = "linux")]
    let target = resolve(path, roots, Purpose::Write)?;
    let Target::Entry { parent, name } = target else {
        return Err(Denial::invalid("a write resolves to a directory entry"));
    };
    #[cfg(not(target_os = "linux"))]
    let dir = open_parent(&parent, &name, &roots)?;
    #[cfg(target_os = "linux")]
    let dir = linux::open_parent(&parent, &name, roots, KernelCapability::Openat2)?;
    let dirfd = dir.as_raw_fd();
    let mode = match stat_at(dirfd, &name)? {
        Some(st) if st.st_mode & libc::S_IFMT == libc::S_IFLNK => {
            return Err(Denial::denied(format!(
                "{} is a symlink; fs.write does not replace symlinks",
                parent.join(&name).display()
            )));
        }
        Some(st) if st.st_mode & libc::S_IFMT != libc::S_IFREG => {
            return Err(Denial::invalid(format!(
                "{} is not a regular file",
                parent.join(&name).display()
            )));
        }
        // Preserve access permissions, never privilege bits on a new inode.
        Some(st) => replacement_mode(st.st_mode),
        None => 0o644,
    };
    // A bounded basename leaves room even when the target uses NAME_MAX bytes.
    let temp_name = OsString::from(format!(
        ".basal-{}-{}.tmp",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let temp = c_name(&temp_name)?;
    let target = c_name(&name)?;
    // SAFETY: `temp` is NUL-terminated and `dirfd` is the verified parent
    // directory; O_EXCL|O_NOFOLLOW create a new file and never follow a
    // symlink.
    let raw = unsafe {
        libc::openat(
            dirfd,
            temp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            libc::c_uint::from(mode),
        )
    };
    if raw == -1 {
        let e = std::io::Error::last_os_error();
        return Err(io_denial(&parent.join(&temp_name), &e));
    }
    // SAFETY: openat returned a new descriptor this function owns.
    let mut file = File::from(unsafe { OwnedFd::from_raw_fd(raw) });
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let renamed = written.and_then(|()| {
        // SAFETY: both names are NUL-terminated and relative to the same
        // verified directory descriptor.
        let r = unsafe { libc::renameat(dirfd, temp.as_ptr(), dirfd, target.as_ptr()) };
        if r == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    });
    if let Err(e) = renamed {
        // SAFETY: removes the temporary file this call created.
        unsafe { libc::unlinkat(dirfd, temp.as_ptr(), 0) };
        return Err(io_denial(&parent.join(&name), &e));
    }
    // Make the rename itself durable; a failure here leaves the new file
    // in place, so it is not an error.
    let _ = dir.sync_all();
    Ok(json!({ "bytes": text.len() }))
}

/// Linux opens the canonical component list beneath a descriptor whose identity
/// was captured during resolution. Renames after acquisition keep acting on the
/// held object; they cannot redirect a later operation through a symlink.
#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Whether to try openat2 or force the component walk used before Linux 5.6.
    /// Even when openat2 is selected, ENOSYS alone selects the walk.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum KernelCapability {
        Openat2,
        NoOpenat2,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Identity {
        dev: u64,
        ino: u64,
    }

    impl Identity {
        fn of(meta: &std::fs::Metadata) -> Self {
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }

        fn check(&self, file: &File, path: &Path) -> Result<(), Denial> {
            let meta = file.metadata().map_err(|_| outside(path))?;
            if *self == Self::of(&meta) {
                Ok(())
            } else {
                Err(outside(path))
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Root {
        manifest: String,
        path: PathBuf,
        identity: Identity,
        directory: bool,
        // A file grant permits replacement of only its own basename, even
        // though atomic replacement needs a descriptor for the parent.
        parent_identity: Option<Identity>,
    }

    /// A canonical path and the selected grant's identity snapshot. The private
    /// snapshot prevents callers from opening a path without first resolving it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ResolvedPath {
        path: PathBuf,
        root: Root,
    }

    impl AsRef<Path> for ResolvedPath {
        fn as_ref(&self) -> &Path {
            &self.path
        }
    }

    impl std::ops::Deref for ResolvedPath {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.path
        }
    }

    /// A resolved path carries the identity of the root used to authorize it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Target {
        Existing(ResolvedPath),
        Entry {
            parent: ResolvedPath,
            name: OsString,
        },
    }

    fn snapshot_roots(roots: &[String]) -> Vec<Root> {
        roots
            .iter()
            .filter_map(|manifest| {
                let path = std::fs::canonicalize(expand_home(manifest)?).ok()?;
                let meta = std::fs::metadata(&path).ok()?;
                let directory = meta.is_dir();
                let parent_identity = if directory {
                    None
                } else {
                    Some(Identity::of(&std::fs::metadata(path.parent()?).ok()?))
                };
                Some(Root {
                    manifest: manifest.clone(),
                    path,
                    identity: Identity::of(&meta),
                    directory,
                    parent_identity,
                })
            })
            .collect()
    }

    fn select(path: &Path, roots: &[Root]) -> Result<Root, Denial> {
        roots
            .iter()
            .find(|root| path.starts_with(&root.path) && (root.directory || path == root.path))
            .cloned()
            .ok_or_else(|| outside(path))
    }

    pub fn resolve(path: &str, roots: &[String], purpose: Purpose) -> Result<Target, Denial> {
        let path = absolute(path)?;
        let roots = snapshot_roots(roots);
        if purpose == Purpose::Read {
            match std::fs::canonicalize(&path) {
                Ok(real) => {
                    let root = select(&real, &roots)?;
                    return Ok(Target::Existing(ResolvedPath { path: real, root }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(outside(&path)),
            }
        }
        let name = match path.components().next_back() {
            Some(Component::Normal(name)) => name.to_owned(),
            _ => {
                return Err(Denial::invalid(format!(
                    "{} does not name an entry in a directory",
                    path.display()
                )));
            }
        };
        let parent = std::fs::canonicalize(path.parent().unwrap_or(Path::new("/")))
            .map_err(|_| outside(&path))?;
        let root = select(&parent.join(&name), &roots)?;
        Ok(Target::Entry {
            parent: ResolvedPath { path: parent, root },
            name,
        })
    }

    fn flags(directory: bool) -> libc::c_int {
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        if directory {
            flags |= libc::O_DIRECTORY;
        }
        flags
    }

    fn owned(raw: libc::c_int) -> std::io::Result<File> {
        if raw == -1 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a successful open returned a new descriptor owned by this call.
        Ok(File::from(unsafe { OwnedFd::from_raw_fd(raw) }))
    }

    fn walk(dir: RawFd, path: &Path, directory: bool) -> std::io::Result<File> {
        let mut current = None;
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            let name = match component {
                Component::RootDir => OsStr::new("/"),
                Component::Normal(name) => name,
                _ => return Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
            };
            let name = CString::new(name.as_bytes())
                .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
            let fd = current.as_ref().map_or(dir, AsRawFd::as_raw_fd);
            // SAFETY: the name is NUL-terminated and fd is either AT_FDCWD or
            // a held directory. Every component rejects symlinks, not just the last.
            let raw = unsafe {
                libc::openat(
                    fd,
                    name.as_ptr(),
                    flags(directory || components.peek().is_some()),
                )
            };
            current = Some(owned(raw)?);
        }
        current.ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))
    }

    fn open_path(
        dir: RawFd,
        path: &Path,
        directory: bool,
        capability: KernelCapability,
    ) -> std::io::Result<File> {
        if capability == KernelCapability::Openat2 {
            let name = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
            // SAFETY: open_how contains only integer fields; zero selects no
            // creation mode and no additional resolution flags.
            let mut how: libc::open_how = unsafe { std::mem::zeroed() };
            how.flags = flags(directory) as u64;
            how.resolve = libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS;
            if !path.is_absolute() {
                how.resolve |= libc::RESOLVE_BENEATH;
            }
            // RESOLVE_NO_XDEV is deliberately absent: mounts under a grant
            // remain reachable, just as they are on the other platforms.
            // SAFETY: name and how are valid for the syscall, with the exact
            // open_how size; ownership of a successful result is taken below.
            let raw = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    dir,
                    name.as_ptr(),
                    &how,
                    std::mem::size_of::<libc::open_how>(),
                )
            } as libc::c_int;
            match owned(raw) {
                Ok(file) => return Ok(file),
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {}
                Err(e) => return Err(e),
            }
        }
        walk(dir, path, directory)
    }

    fn acquire_root(
        resolved: &ResolvedPath,
        roots: &[String],
        capability: KernelCapability,
    ) -> Result<File, Denial> {
        let root = &resolved.root;
        if !roots.contains(&root.manifest) {
            return Err(outside(&resolved.path));
        }
        let file = open_path(libc::AT_FDCWD, &root.path, root.directory, capability)
            .map_err(|_| outside(&resolved.path))?;
        root.identity.check(&file, &resolved.path)?;
        Ok(file)
    }

    pub fn open_checked(
        resolved: &ResolvedPath,
        roots: &[String],
        directory: bool,
    ) -> Result<File, Denial> {
        open_checked_with_capability(resolved, roots, directory, KernelCapability::Openat2)
    }

    /// The capability is explicit so the old-kernel walk can be exercised on a
    /// new kernel without environment variables or a timing-dependent race.
    pub fn open_checked_with_capability(
        resolved: &ResolvedPath,
        roots: &[String],
        directory: bool,
        capability: KernelCapability,
    ) -> Result<File, Denial> {
        let root = acquire_root(resolved, roots, capability)?;
        let relative = resolved
            .path
            .strip_prefix(&resolved.root.path)
            .map_err(|_| outside(&resolved.path))?;
        if relative.as_os_str().is_empty() {
            if directory && !resolved.root.directory {
                return Err(outside(&resolved.path));
            }
            return Ok(root);
        }
        let parent = relative.parent().ok_or_else(|| outside(&resolved.path))?;
        let dir = if parent.as_os_str().is_empty() {
            root
        } else {
            open_path(root.as_raw_fd(), parent, true, capability)
                .map_err(|_| outside(&resolved.path))?
        };
        let name = relative
            .file_name()
            .ok_or_else(|| outside(&resolved.path))?;
        open_path(dir.as_raw_fd(), Path::new(name), directory, capability).map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                Denial::denied(format!(
                    "{} became a symlink while it was being opened",
                    resolved.display()
                ))
            } else {
                outside(&resolved.path)
            }
        })
    }

    pub(super) fn open_parent(
        parent: &ResolvedPath,
        name: &OsStr,
        roots: &[String],
        capability: KernelCapability,
    ) -> Result<File, Denial> {
        let root = acquire_root(parent, roots, capability)?;
        if parent.root.directory {
            let relative = parent
                .path
                .strip_prefix(&parent.root.path)
                .map_err(|_| outside(&parent.path))?;
            if relative.as_os_str().is_empty() {
                return Ok(root);
            }
            open_path(root.as_raw_fd(), relative, true, capability)
                .map_err(|_| outside(&parent.path))
        } else {
            // Acquiring the parent grants no authority over its other names.
            if parent.path.join(name) != parent.root.path {
                return Err(outside(&parent.path));
            }
            let dir = open_path(libc::AT_FDCWD, &parent.path, true, capability)
                .map_err(|_| outside(&parent.path))?;
            parent
                .root
                .parent_identity
                .as_ref()
                .ok_or_else(|| outside(&parent.path))?
                .check(&dir, &parent.path)?;
            Ok(dir)
        }
    }

    pub fn stat(path: &str, roots: &[String]) -> Result<Value, Denial> {
        let target = resolve(path, roots, Purpose::Read)?;
        let (parent, name) = match target {
            Target::Existing(real) if real.path == real.root.path => {
                let file = acquire_root(&real, roots, KernelCapability::Openat2)?;
                let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
                // SAFETY: file is held open and fstat initializes st on success.
                if unsafe { libc::fstat(file.as_raw_fd(), st.as_mut_ptr()) } != 0 {
                    return Err(outside(&real.path));
                }
                // SAFETY: fstat succeeded, so st is initialized.
                return Ok(stat_value(unsafe { st.assume_init() }));
            }
            Target::Existing(real) => {
                let name = real
                    .path
                    .file_name()
                    .ok_or_else(|| outside(&real.path))?
                    .to_owned();
                let parent = ResolvedPath {
                    path: real
                        .path
                        .parent()
                        .ok_or_else(|| outside(&real.path))?
                        .to_owned(),
                    root: real.root,
                };
                (parent, name)
            }
            Target::Entry { parent, name } => (parent, name),
        };
        let dir = open_parent(&parent, &name, roots, KernelCapability::Openat2)?;
        Ok(match stat_at(dir.as_raw_fd(), &name)? {
            Some(st) => stat_value(st),
            None => json!({ "exists": false }),
        })
    }

    fn stat_value(st: libc::stat) -> Value {
        let mtime_ms = st.st_mtime * 1000 + st.st_mtime_nsec / 1_000_000;
        json!({ "exists": true, "kind": kind_of(st.st_mode), "size": st.st_size, "mtime_ms": mtime_ms })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct FileRoot(PathBuf);

        impl FileRoot {
            fn new() -> Self {
                let base = std::env::temp_dir().join(format!(
                    "basal-fs-file-parent-{}-{}",
                    std::process::id(),
                    TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(base.join("parent")).expect("parent");
                std::fs::write(base.join("parent/file"), "inside").expect("file");
                Self(base)
            }

            fn resolved(&self) -> (ResolvedPath, OsString, Vec<String>) {
                let file = self.0.join("parent/file");
                let roots = vec![file.display().to_string()];
                let Target::Entry { parent, name } =
                    resolve(file.to_str().unwrap(), &roots, Purpose::Write)
                        .expect("resolve file-root write")
                else {
                    panic!("write entry");
                };
                (parent, name, roots)
            }
        }

        impl Drop for FileRoot {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn file_root_parent_identity_is_checked_even_when_the_file_inode_is_unchanged() {
            let t = FileRoot::new();
            let (parent, name, roots) = t.resolved();
            std::fs::rename(t.0.join("parent"), t.0.join("parent-moved")).expect("move parent");
            std::fs::create_dir(t.0.join("parent")).expect("replacement parent");
            // Retain the granted file's inode so only the parent identity check
            // can distinguish the replacement directory from the original.
            std::fs::hard_link(t.0.join("parent-moved/file"), t.0.join("parent/file"))
                .expect("same file inode");
            for capability in [KernelCapability::Openat2, KernelCapability::NoOpenat2] {
                let denial = open_parent(&parent, &name, &roots, capability)
                    .expect_err("replacement parent");
                assert_eq!(denial.code, codes::DENIED);
            }
        }

        #[test]
        fn file_root_parent_descriptor_cannot_authorize_a_sibling_name() {
            let t = FileRoot::new();
            let (parent, name, roots) = t.resolved();
            for capability in [KernelCapability::Openat2, KernelCapability::NoOpenat2] {
                let dir = open_parent(&parent, &name, &roots, capability).expect("own basename");
                assert!(dir.metadata().expect("parent metadata").is_dir());
                let denial = open_parent(&parent, OsStr::new("sibling"), &roots, capability)
                    .expect_err("sibling basename");
                assert_eq!(denial.code, codes::DENIED);
            }
        }
    }
}
