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
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use super::{Denial, codes, expand_home};

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
pub enum Target {
    /// It exists; this is its real path.
    Existing(PathBuf),
    /// It does not exist (or, for a write, is about to be replaced): the
    /// real path of its parent, and its last component.
    Entry { parent: PathBuf, name: OsString },
}

/// The roots, each resolved to its real path. A root that does not exist
/// now grants nothing.
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
pub fn resolve(path: &str, roots: &[String], purpose: Purpose) -> Result<Target, Denial> {
    let roots = real_roots(roots);
    resolve_real(path, &roots, purpose)
}

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
pub fn open_checked(resolved: &Path, roots: &[String], directory: bool) -> Result<File, Denial> {
    open_checked_real(resolved, &real_roots(roots), directory)
}

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
    let roots = real_roots(roots);
    let max_bytes = max_bytes.min(MAX_READ_BYTES);
    let real = match resolve_real(path, &roots, Purpose::Read)? {
        Target::Existing(real) => real,
        Target::Entry { parent, name } => {
            return Err(Denial::new(
                codes::NOT_FOUND,
                format!("{} does not exist", parent.join(name).display()),
            ));
        }
    };
    let file = open_checked_real(&real, &roots, false)?;
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
    let roots = real_roots(roots);
    let real = match resolve_real(path, &roots, Purpose::Read)? {
        Target::Existing(real) => real,
        Target::Entry { parent, name } => {
            return Err(Denial::new(
                codes::NOT_FOUND,
                format!("{} does not exist", parent.join(name).display()),
            ));
        }
    };
    let dir = open_checked_real(&real, &roots, true)?;
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
    let roots = real_roots(roots);
    let Target::Entry { parent, name } = resolve_real(path, &roots, Purpose::Write)? else {
        return Err(Denial::invalid("a write resolves to a directory entry"));
    };
    let dir = open_parent(&parent, &name, &roots)?;
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
