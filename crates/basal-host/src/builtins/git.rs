//! The git reads: `git.log`, `git.revParse`, `git.describeTags`,
//! `git.show` and `git.diff`, over the manifest's `git.read` repositories.
//!
//! They run the `git` program rather than an in-process reader such as
//! gix. git itself gives exactly git's answers for revision syntax,
//! `describe`, pathspecs and unified diffs, which an in-process reader would
//! have to reimplement, and it adds no large dependency tree to the parent.
//! The cost is that a repository's own configuration can make git run a
//! program (`core.fsmonitor`, hooks, an external diff, a text conversion
//! filter, signature checks, a pager, a lazy fetch in a partial clone), and
//! the person who controls a repository's `.git/config` is not necessarily
//! the operator who approved the flow. Every command therefore goes through
//! [`hardened_command`], which turns each of those off.

#[cfg(test)]
mod tests;

#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
use std::collections::HashSet;
#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::fs::File;
#[cfg(not(windows))]
use std::io::Read;
#[cfg(not(windows))]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(not(windows))]
use std::os::unix::process::CommandExt;
#[cfg(not(windows))]
use std::path::PathBuf;
use std::path::{Component, Path};
#[cfg(not(windows))]
use std::process::Command;
#[cfg(not(windows))]
use std::process::{Child, ExitStatus, Stdio};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(not(windows))]
use std::sync::mpsc::{self, Sender};
#[cfg(not(windows))]
use std::thread::{self, JoinHandle};
use std::time::Duration;
#[cfg(not(windows))]
use std::time::Instant;

use basal_proto::Primitive;
use serde_json::{Value, json};

use super::{Denial, codes, expand_home, options, string_arg};

/// How long one git command may run.
pub const TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const STDERR_BYTES: usize = 4096;
/// The most output one git command may produce: a log, a blob or a diff.
pub const MAX_OUTPUT_BYTES: usize = super::MAX_TEXT_RESULT_BYTES;
/// `git.log`'s default and largest entry counts.
pub const DEFAULT_LOG_COUNT: u64 = 20;
pub const MAX_LOG_COUNT: u64 = 500;
/// The most tags `git.describeTags` lists.
pub const MAX_TAGS: usize = 200;
/// The longest revision, path or pattern accepted.
const MAX_ARG_BYTES: usize = 1024;

/// One git read, its arguments checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Log {
        reference: Option<String>,
        path: Option<String>,
        since_ms: Option<u64>,
        max_count: u64,
    },
    RevParse {
        reference: String,
    },
    DescribeTags {
        pattern: Option<String>,
    },
    Show {
        rev: String,
        path: String,
    },
    Diff {
        from: String,
        to: String,
        path: Option<String>,
    },
}

/// A revision: letters, digits and `._/-~^@{}+`, not starting with `-`, so
/// it can never be read as an option.
fn check_rev(field: &str, rev: &str) -> Result<(), Denial> {
    let ok = !rev.is_empty()
        && rev.len() <= MAX_ARG_BYTES
        && !rev.starts_with('-')
        && rev
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-~^@{}+".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Denial::invalid(format!(
            "{field} {rev:?} is not a revision basal accepts"
        )))
    }
}

/// A path inside the repository: relative, no `..`, no NUL.
fn check_repo_path(path: &str) -> Result<(), Denial> {
    let p = Path::new(path);
    let ok = !path.is_empty()
        && path.len() <= MAX_ARG_BYTES
        && !path.contains('\0')
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if ok {
        Ok(())
    } else {
        Err(Denial::invalid(format!(
            "path {path:?} must be relative to the repository, without `..`"
        )))
    }
}

/// A tag pattern (a shell glob), not starting with `-`.
fn check_pattern(pattern: &str) -> Result<(), Denial> {
    let ok = !pattern.is_empty()
        && pattern.len() <= MAX_ARG_BYTES
        && !pattern.starts_with('-')
        && pattern
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-*?[]+".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Denial::invalid(format!(
            "pattern {pattern:?} is not a tag pattern basal accepts"
        )))
    }
}

fn opt_string(
    o: Option<&serde_json::Map<String, Value>>,
    key: &str,
) -> Result<Option<String>, Denial> {
    match o.and_then(|o| o.get(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(Denial::invalid(format!("{key} must be a string"))),
    }
}

/// Reads a git built-in's arguments (other than the repository).
pub fn parse(primitive: Primitive, args: &Value) -> Result<Op, Denial> {
    match primitive {
        Primitive::GitLog => {
            let o = options(args, "options", &["ref", "path", "sinceMs", "maxCount"])?;
            let reference = opt_string(o, "ref")?;
            if let Some(r) = &reference {
                check_rev("ref", r)?;
            }
            let path = opt_string(o, "path")?;
            if let Some(p) = &path {
                check_repo_path(p)?;
            }
            let since_ms = match o.and_then(|o| o.get("sinceMs")) {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().ok_or_else(|| {
                    Denial::invalid("sinceMs must be a whole number of milliseconds")
                })?),
            };
            let max_count = match o.and_then(|o| o.get("maxCount")) {
                None | Some(Value::Null) => DEFAULT_LOG_COUNT,
                Some(v) => match v.as_u64() {
                    Some(n) if (1..=MAX_LOG_COUNT).contains(&n) => n,
                    _ => {
                        return Err(Denial::invalid(format!(
                            "maxCount must be a whole number from 1 to {MAX_LOG_COUNT}"
                        )));
                    }
                },
            };
            Ok(Op::Log {
                reference,
                path,
                since_ms,
                max_count,
            })
        }
        Primitive::GitRevParse => {
            let reference = string_arg(args, "ref")?;
            check_rev("ref", &reference)?;
            Ok(Op::RevParse { reference })
        }
        Primitive::GitDescribeTags => {
            let o = options(args, "options", &["pattern"])?;
            let pattern = opt_string(o, "pattern")?;
            if let Some(p) = &pattern {
                check_pattern(p)?;
            }
            Ok(Op::DescribeTags { pattern })
        }
        Primitive::GitShow => {
            let rev = string_arg(args, "rev")?;
            check_rev("rev", &rev)?;
            let path = string_arg(args, "path")?;
            check_repo_path(&path)?;
            Ok(Op::Show { rev, path })
        }
        Primitive::GitDiff => {
            let from = string_arg(args, "from")?;
            check_rev("from", &from)?;
            let to = string_arg(args, "to")?;
            check_rev("to", &to)?;
            let o = options(args, "options", &["path"])?;
            let path = opt_string(o, "path")?;
            if let Some(p) = &path {
                check_repo_path(p)?;
            }
            Ok(Op::Diff { from, to, path })
        }
        other => Err(Denial::invalid(format!(
            "{} is not a git read",
            other.name()
        ))),
    }
}

/// A verified repository directory held open through child creation.
#[cfg(unix)]
pub struct Repository {
    path: PathBuf,
    directory: Arc<File>,
}

#[cfg(windows)]
type Repository = super::fs::windows::PinnedDirectory;

#[cfg(all(test, target_os = "macos"))]
pub(super) fn before_verified_open() {
    tests::before_open();
}

/// Opens a repository whose real path is one of the approved repositories
/// (each resolved the same way). A subdirectory is not itself approved.
#[cfg(not(windows))]
pub fn repo(repo: &str, repos: &[String]) -> Result<Repository, Denial> {
    let path = expand_home(repo)
        .ok_or_else(|| Denial::invalid(format!("{repo:?} is not an absolute path")))?;
    let refused = || Denial::denied("repository is outside the manifest's repositories or missing");
    let real = std::fs::canonicalize(&path).map_err(|_| refused())?;
    let approved = repos
        .iter()
        .filter_map(|r| expand_home(r))
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|r| r == real);
    if !approved {
        return Err(refused());
    }
    let real_str = real.to_str().ok_or_else(refused)?;
    let super::fs::Target::Existing(resolved) =
        super::fs::resolve(real_str, repos, super::fs::Purpose::Read)?
    else {
        return Err(refused());
    };
    if AsRef::<Path>::as_ref(&resolved) != real {
        return Err(refused());
    }
    let directory = super::fs::open_checked(&resolved, repos, true)?;
    let pinned = Repository {
        path: real,
        directory: Arc::new(directory),
    };
    // Indirection targets are checked at call start, not frozen. A local
    // process can redirect mutable metadata between this check and Git's read;
    // flow code cannot, and paths inside approved roots are already readable.
    check_indirection(&pinned, repos)?;
    Ok(pinned)
}

#[cfg(unix)]
fn outside_root() -> Denial {
    Denial::denied("path is outside the manifest's roots or missing")
}

/// Inspect `.git`, `commondir` and alternates through open repository, metadata
/// and object-directory descriptors. Git can redirect metadata and object reads
/// independently of its working directory, so pinning that directory is not
/// enough to check the scope of these reads.
#[cfg(unix)]
fn check_indirection(repo: &Repository, roots: &[String]) -> Result<(), Denial> {
    let canonical_roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| expand_home(root))
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    let git_entry = checked_entry(&repo.directory, ".git", &canonical_roots)?;
    let git_dir = match git_entry {
        Some(file) if file.metadata().map_err(|_| outside_root())?.is_file() => {
            let base =
                super::fs::fd_path(repo.directory.as_raw_fd()).map_err(|_| outside_root())?;
            let text = indirection_text(file)?;
            let target = text
                .strip_prefix("gitdir: ")
                .ok_or_else(outside_root)?
                .trim_end_matches(['\n', '\r']);
            if target.is_empty() {
                return Err(outside_root());
            }
            open_inside(&base.join(target), roots, &canonical_roots)?
        }
        Some(file) => file,
        None => repo.directory.try_clone().map_err(|_| outside_root())?,
    };
    let common = match checked_entry(&git_dir, "commondir", &canonical_roots)? {
        Some(file) => {
            let base = super::fs::fd_path(git_dir.as_raw_fd()).map_err(|_| outside_root())?;
            open_inside(
                &base.join(indirection_text(file)?.trim_end_matches(['\n', '\r'])),
                roots,
                &canonical_roots,
            )?
        }
        None => git_dir,
    };
    let objects = checked_entry(&common, "objects", &canonical_roots)?
        .ok_or_else(|| Denial::new(codes::GIT, "repository has no object directory"))?;
    check_alternates(objects, roots, &canonical_roots, &mut HashSet::new())
}

#[cfg(unix)]
fn checked_entry(dir: &File, name: &str, roots: &[PathBuf]) -> Result<Option<File>, Denial> {
    let name = CString::new(name).map_err(|_| outside_root())?;
    // SAFETY: dir is held and name is NUL-terminated. Metadata symlinks may be
    // followed only because the resulting descriptor is checked below.
    let raw = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if raw == -1 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(outside_root())
        };
    }
    // SAFETY: openat returned a new descriptor owned by this call.
    let file = unsafe { File::from_raw_fd(raw) };
    let path = super::fs::fd_path(file.as_raw_fd()).map_err(|_| outside_root())?;
    if !super::fs::inside(&path, roots) {
        return Err(outside_root());
    }
    Ok(Some(file))
}

#[cfg(unix)]
fn open_inside(path: &Path, roots: &[String], canonical_roots: &[PathBuf]) -> Result<File, Denial> {
    let real = std::fs::canonicalize(path).map_err(|_| outside_root())?;
    if !super::fs::inside(&real, canonical_roots) {
        return Err(outside_root());
    }
    let super::fs::Target::Existing(resolved) = super::fs::resolve(
        real.to_str().ok_or_else(outside_root)?,
        roots,
        super::fs::Purpose::Read,
    )?
    else {
        return Err(outside_root());
    };
    let file = super::fs::open_checked(&resolved, roots, true)?;
    let actual = super::fs::fd_path(file.as_raw_fd()).map_err(|_| outside_root())?;
    if actual != real || !super::fs::inside(&actual, canonical_roots) {
        return Err(outside_root());
    }
    Ok(file)
}

#[cfg(unix)]
fn indirection_text(file: File) -> Result<String, Denial> {
    const LIMIT: u64 = 64 * 1024;
    if !file.metadata().map_err(|_| outside_root())?.is_file() {
        return Err(outside_root());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| outside_root())?;
    if bytes.len() as u64 > LIMIT || bytes.contains(&0) {
        return Err(outside_root());
    }
    String::from_utf8(bytes).map_err(|_| outside_root())
}

#[cfg(unix)]
fn check_alternates(
    objects: File,
    roots: &[String],
    canonical_roots: &[PathBuf],
    visited: &mut HashSet<PathBuf>,
) -> Result<(), Denial> {
    let base = super::fs::fd_path(objects.as_raw_fd()).map_err(|_| outside_root())?;
    if !visited.insert(base.clone()) {
        return Ok(());
    }
    // Limit the number of recursively referenced object directories so a small
    // alternates file cannot cause an unbounded metadata walk.
    if visited.len() > 128 {
        return Err(outside_root());
    }
    let Some(info) = checked_entry(&objects, "info", canonical_roots)? else {
        return Ok(());
    };
    let Some(alternates) = checked_entry(&info, "alternates", canonical_roots)? else {
        return Ok(());
    };
    for line in indirection_text(alternates)?
        .split('\n')
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let target = alternate_path(line);
        let alternate = open_inside(&base.join(target), roots, canonical_roots)?;
        check_alternates(alternate, roots, canonical_roots, visited)?;
    }
    Ok(())
}

#[cfg(unix)]
fn alternate_path(line: &str) -> PathBuf {
    if line.starts_with('"')
        && let Some(path) = unquote_alternate(line.as_bytes())
    {
        use std::os::unix::ffi::OsStringExt;
        return PathBuf::from(std::ffi::OsString::from_vec(path));
    }
    // Git treats an invalid C-style quote as part of the literal pathname.
    PathBuf::from(line)
}

#[cfg(unix)]
fn unquote_alternate(line: &[u8]) -> Option<Vec<u8>> {
    let mut path = Vec::new();
    let mut i = 1;
    while i < line.len() {
        let byte = line[i];
        i += 1;
        match byte {
            b'"' => return (i == line.len()).then_some(path),
            b'\\' => {
                let escaped = *line.get(i)?;
                i += 1;
                let byte = match escaped {
                    b'a' => 7,
                    b'b' => 8,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 11,
                    b'f' => 12,
                    b'r' => b'\r',
                    b'\\' | b'"' => escaped,
                    b'0'..=b'3' => {
                        let second = *line.get(i)?;
                        let third = *line.get(i + 1)?;
                        if !(b'0'..=b'7').contains(&second) || !(b'0'..=b'7').contains(&third) {
                            return None;
                        }
                        i += 2;
                        (escaped - b'0') * 64 + (second - b'0') * 8 + third - b'0'
                    }
                    _ => return None,
                };
                path.push(byte);
            }
            _ => path.push(byte),
        }
    }
    None
}

#[cfg(windows)]
pub(crate) fn repo(
    repo: &str,
    repos: &[String],
) -> Result<super::fs::windows::PinnedDirectory, Denial> {
    use super::fs::windows::pin_directory;
    let path = expand_home(repo).ok_or_else(|| Denial::invalid("repository must be absolute"))?;
    let path = path
        .to_str()
        .ok_or_else(|| Denial::invalid("repository is not Unicode"))?;
    let refused = || Denial::denied("repository is outside the manifest's repositories or missing");
    let pinned = pin_directory(path).map_err(|_| refused())?;
    for approved in repos.iter().filter_map(|r| expand_home(r)) {
        if let Some(approved) = approved.to_str()
            && let Ok(root) = pin_directory(approved)
            && pinned.same_identity(&root)
        {
            return Ok(pinned);
        }
    }
    Err(refused())
}

/// The only way the built-ins run git: a `git -C .` command in the held
/// repository directory that can read the repository and run nothing else.
///
/// - The environment is cleared except `PATH` and `HOME`, so no `GIT_DIR`,
///   `GIT_CONFIG_*`, `GIT_EXTERNAL_DIFF` or editor setting comes in from
///   basal's own environment.
/// - No system or global configuration (`GIT_CONFIG_NOSYSTEM=1`,
///   `GIT_CONFIG_GLOBAL=/dev/null`); the repository's own `.git/config` is
///   still read, so everything in it that can start a program is
///   overridden on the command line, which takes precedence:
///   `core.fsmonitor`, `core.hooksPath`, `core.pager`, `log.showSignature`
///   (which would run gpg) and `protocol.allow` (a partial clone's lazy
///   fetch would run ssh or a remote helper; `GIT_NO_LAZY_FETCH` stops it
///   too). The commands that could run an external diff or a text
///   conversion filter pass `--no-ext-diff --no-textconv` themselves.
/// - `--no-optional-locks` keeps reads from writing the index;
///   `--literal-pathspecs` keeps a path from being read as pathspec magic;
///   `GIT_TERMINAL_PROMPT=0` and a null stdin keep git from asking anyone
///   anything; `GIT_CEILING_DIRECTORIES` stops git from walking up to a
///   repository above the approved one.
///
/// Repositories must belong to the service uid. Global `safe.directory`
/// exceptions are deliberately ignored, rather than trusting foreign config.
#[cfg(not(windows))]
pub fn hardened_command(repo: &Repository) -> Command {
    let mut command = Command::new("git");
    command.env_clear();
    for var in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(var) {
            command.env(var, value);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1");
    if let Some(parent) = repo.path.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    command
        .arg("-C")
        .arg(".")
        .args([
            "--no-optional-locks",
            "--no-pager",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.pager=cat",
            "-c",
            "log.showSignature=false",
            "-c",
            "protocol.allow=never",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let directory = Arc::clone(&repo.directory);
    // SAFETY: the closure owns the directory descriptor through spawn and only
    // calls async-signal-safe fchdir in the child. CLOEXEC closes it at exec;
    // changing the child's cwd never changes the parent's cwd.
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(directory.as_raw_fd()) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

#[cfg(windows)]
pub use windows::hardened_command;

/// How a git command ended.
pub struct Ran {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs a git command under [`TIMEOUT`], refusing output over
/// [`MAX_OUTPUT_BYTES`].
#[cfg(not(windows))]
pub fn run_command(command: Command) -> Result<Ran, Denial> {
    run_command_until(command, None, TIMEOUT)
}

#[cfg(windows)]
pub use windows::run_command;

/// Every production caller passes [`TIMEOUT`]; only a test passes a shorter
/// `timeout`, so it can check the timeout cleanup without waiting 20 s.
#[cfg(not(windows))]
fn run_command_until(
    mut command: Command,
    max_lines: Option<usize>,
    timeout: Duration,
) -> Result<Ran, Denial> {
    command.process_group(0);
    let child = command
        .spawn()
        .map_err(|e| Denial::new(codes::GIT, format!("git could not start: {e}")))?;
    thread::scope(|scope| {
        let mut guard = ProcessGroup::new(child);
        let out = guard.child.as_mut().unwrap().stdout.take();
        let err = guard.child.as_mut().unwrap().stderr.take();
        for fd in out
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .into_iter()
            .chain(err.as_ref().map(AsRawFd::as_raw_fd))
        {
            // SAFETY: these pipes remain owned by this call. Nonblocking reads
            // and a timed poll keep descendant-held pipes within the deadline.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
                return Err(Denial::new(
                    codes::GIT,
                    std::io::Error::last_os_error().to_string(),
                ));
            }
        }
        let deadline = Instant::now() + timeout;
        let (tx, completed) = mpsc::channel();
        guard.waiter(tx.clone());
        let output_tx = tx.clone();
        scope.spawn(move || {
            let _ = output_tx.send(Completion::Stdout(drain_pipe(
                out,
                MAX_OUTPUT_BYTES + 1,
                max_lines,
                deadline,
            )));
        });
        scope.spawn(move || {
            let _ = tx.send(Completion::Stderr(drain_pipe(
                err,
                STDERR_BYTES,
                None,
                deadline,
            )));
        });
        let (mut status, mut stdout, mut stderr) = (None, None, None);
        let mut limit_reached = false;
        while status.is_none() || stdout.is_none() || stderr.is_none() {
            match completed.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Completion::Exit(result)) => {
                    status = Some(result.map_err(|e| Denial::new(codes::GIT, e.to_string()))?);
                    // A finished parent can leave descendants holding its pipes.
                    guard.kill();
                }
                Ok(Completion::Stdout(result)) => {
                    let output = result?;
                    limit_reached = output.limit_reached;
                    if limit_reached {
                        guard.kill();
                    }
                    stdout = Some(output.bytes);
                }
                Ok(Completion::Stderr(result)) => stderr = Some(result?.bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(timeout_denial()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Denial::new(codes::GIT, "git waiter disconnected"));
                }
            }
        }
        let status = status.unwrap();
        Ok(Ran {
            success: status.success() || limit_reached,
            code: if limit_reached {
                Some(0)
            } else {
                status.code()
            },
            stdout: stdout.unwrap(),
            stderr: String::from_utf8_lossy(&stderr.unwrap()).into_owned(),
        })
    })
}

#[cfg(not(windows))]
fn timeout_denial() -> Denial {
    Denial::new(
        codes::TIMEOUT,
        format!("git ran longer than {} s", TIMEOUT.as_secs()),
    )
}

#[cfg(not(windows))]
enum Completion {
    Exit(std::io::Result<ExitStatus>),
    Stdout(Result<PipeOutput, Denial>),
    Stderr(Result<PipeOutput, Denial>),
}

#[cfg(not(windows))]
struct ProcessGroup {
    pid: u32,
    child: Option<Child>,
    waiter: Option<JoinHandle<()>>,
}
#[cfg(not(windows))]
impl ProcessGroup {
    fn new(child: Child) -> Self {
        Self {
            pid: child.id(),
            child: Some(child),
            waiter: None,
        }
    }

    fn waiter(&mut self, completed: Sender<Completion>) {
        let mut child = self.child.take().unwrap();
        self.waiter = Some(thread::spawn(move || {
            let _ = completed.send(Completion::Exit(child.wait()));
        }));
    }

    fn kill(&mut self) {
        // SAFETY: process_group(0) made the spawned child's pid its private
        // group id. Negative pid addresses that entire group, not the parent.
        unsafe {
            libc::kill(-(self.pid as i32), libc::SIGKILL);
        }
    }
}
#[cfg(not(windows))]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
        if let Some(child) = &mut self.child {
            let _ = child.wait();
        }
        if let Some(waiter) = self.waiter.take() {
            let _ = waiter.join();
        }
    }
}

#[cfg(not(windows))]
struct PipeOutput {
    bytes: Vec<u8>,
    limit_reached: bool,
}

#[cfg(not(windows))]
fn drain_pipe<R: Read + AsRawFd>(
    mut pipe: Option<R>,
    cap: usize,
    max_lines: Option<usize>,
    deadline: Instant,
) -> Result<PipeOutput, Denial> {
    let mut output = PipeOutput {
        bytes: Vec::new(),
        limit_reached: false,
    };
    let Some(reader) = pipe.as_mut() else {
        return Ok(output);
    };
    loop {
        if Instant::now() >= deadline {
            return Err(timeout_denial());
        }
        let mut chunk = [0u8; STDERR_BYTES];
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(output),
            Ok(n) => {
                let room = cap.saturating_sub(output.bytes.len());
                output.bytes.extend_from_slice(&chunk[..n.min(room)]);
                if let Some(limit) = max_lines
                    && let Some((last, _)) = output
                        .bytes
                        .iter()
                        .enumerate()
                        .filter(|(_, byte)| **byte == b'\n')
                        .nth(limit - 1)
                {
                    // Bound tag output while reading, retaining git's pattern
                    // matching semantics without materializing the entire list.
                    output.bytes.truncate(last + 1);
                    output.limit_reached = true;
                    return Ok(output);
                }
                if cap > STDERR_BYTES && output.bytes.len() > MAX_OUTPUT_BYTES {
                    return Err(Denial::new(
                        codes::TOO_LARGE,
                        format!("git's output is larger than {MAX_OUTPUT_BYTES} bytes"),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let mut fd = libc::pollfd {
                    fd: reader.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                let timeout = remaining
                    .as_millis()
                    .saturating_add(1)
                    .min(i32::MAX as u128) as i32;
                // SAFETY: fd names the live reader owned by this thread, and
                // poll receives one initialized pollfd for at most the deadline.
                let result = unsafe { libc::poll(&mut fd, 1, timeout) };
                if result == 0 {
                    return Err(timeout_denial());
                }
                if result < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(Denial::new(codes::GIT, error.to_string()));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Denial::new(codes::GIT, error.to_string())),
        }
    }
}

fn git(repo: &Repository, args: &[&str]) -> Result<Ran, Denial> {
    #[cfg(not(windows))]
    let mut command = hardened_command(repo);
    #[cfg(windows)]
    let mut command = hardened_command(repo)?;
    command.args(args);
    run_command(command)
}

#[cfg(not(windows))]
fn run_tags_command(command: Command) -> Result<Ran, Denial> {
    run_command_until(command, Some(MAX_TAGS), TIMEOUT)
}

#[cfg(windows)]
use windows::run_tags_command;

fn failed(ran: &Ran) -> Denial {
    Denial::new(codes::GIT, ran.stderr.trim().to_owned())
}

fn utf8(bytes: Vec<u8>, what: &str) -> Result<String, Denial> {
    String::from_utf8(bytes)
        .map_err(|_| Denial::new(codes::NOT_UTF8, format!("{what} is not UTF-8")))
}

fn parse_log(text: &str) -> Result<Vec<Value>, Denial> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let bad = || {
                Denial::new(
                    codes::GIT,
                    "git log returned invalid fields or an out-of-range timestamp",
                )
            };
            let mut fields = line.splitn(3, '\0');
            let sha = fields.next().ok_or_else(bad)?;
            let at: i64 = fields.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
            let subject = fields.next().ok_or_else(bad)?;
            let author_ms = at.checked_mul(1000).ok_or_else(bad)?;
            Ok(json!({"sha":sha, "subject":subject, "author_ms":author_ms}))
        })
        .collect()
}

fn show_failure(ran: &Ran) -> Denial {
    // LC_ALL=C fixes git's diagnostic vocabulary. Only recognized missing
    // objects/paths are not_found; ownership, permissions and corruption fail.
    if ran.stderr.contains("Not a valid object name")
        || ran.stderr.contains("does not exist in")
        || ran.stderr.contains("exists on disk, but not in")
    {
        Denial::new(codes::NOT_FOUND, ran.stderr.trim())
    } else {
        failed(ran)
    }
}

/// Runs one git read in an approved repository.
pub fn run(repo_arg: &str, repos: &[String], op: &Op) -> Result<Value, Denial> {
    let dir = repo(repo_arg, repos)?;
    #[cfg(all(test, unix))]
    tests::before_exec();
    match op {
        Op::Log {
            reference,
            path,
            since_ms,
            max_count,
        } => {
            let count = format!("--max-count={max_count}");
            let mut args = vec![
                "log",
                "--no-ext-diff",
                "--no-textconv",
                "--no-show-signature",
                "--no-color",
                "--format=%H%x00%at%x00%s",
                count.as_str(),
            ];
            let since = since_ms.map(|ms| format!("--max-age={}", ms / 1000));
            if let Some(since) = &since {
                args.push(since);
            }
            args.push("--end-of-options");
            args.push(reference.as_deref().unwrap_or("HEAD"));
            args.push("--");
            if let Some(p) = path {
                args.push(p);
            }
            let ran = git(&dir, &args)?;
            if !ran.success {
                return Err(failed(&ran));
            }
            let text = utf8(ran.stdout, "the log")?;
            let entries = parse_log(&text)?;
            Ok(Value::Array(entries))
        }
        Op::RevParse { reference } => {
            let peeled = format!("{reference}^{{commit}}");
            let ran = git(
                &dir,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &peeled,
                ],
            )?;
            if !ran.success {
                return Err(if ran.stderr.trim().is_empty() {
                    Denial::new(
                        codes::NOT_FOUND,
                        format!("{reference} does not name a commit"),
                    )
                } else {
                    failed(&ran)
                });
            }
            Ok(Value::String(
                utf8(ran.stdout, "the sha")?.trim().to_owned(),
            ))
        }
        Op::DescribeTags { pattern } => {
            let mut describe = vec!["describe", "--tags", "--abbrev=0"];
            if let Some(p) = pattern {
                describe.push("--match");
                describe.push(p);
            }
            // No tag reachable from HEAD (or no commit at all) is an answer,
            // not a failure: there is no nearest tag.
            let ran = git(&dir, &describe)?;
            let nearest = if ran.success {
                Some(utf8(ran.stdout, "the tag")?.trim().to_owned())
            } else if ran.stderr.contains("No names found")
                || ran.stderr.contains("No tags can describe")
                || ran.stderr.contains("Not a valid object name HEAD")
            {
                None
            } else {
                return Err(failed(&ran));
            };
            let mut list = vec!["tag", "--list", "--sort=-creatordate"];
            if let Some(p) = pattern {
                list.push(p);
            }
            #[cfg(not(windows))]
            let mut command = hardened_command(&dir);
            #[cfg(windows)]
            let mut command = hardened_command(&dir)?;
            command.args(&list);
            let ran = run_tags_command(command)?;
            if !ran.success {
                return Err(failed(&ran));
            }
            let tags: Vec<String> = utf8(ran.stdout, "the tag list")?
                .lines()
                .filter(|l| !l.is_empty())
                .take(MAX_TAGS)
                .map(str::to_owned)
                .collect();
            Ok(json!({ "nearest": nearest, "tags": tags }))
        }
        Op::Show { rev, path } => {
            let spec = format!("{rev}:{path}");
            let ran = git(&dir, &["cat-file", "blob", &spec])?;
            if !ran.success {
                return Err(show_failure(&ran));
            }
            Ok(Value::String(utf8(ran.stdout, "the blob")?))
        }
        Op::Diff { from, to, path } => {
            let mut args = vec![
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--end-of-options",
                from.as_str(),
                to.as_str(),
                "--",
            ];
            if let Some(p) = path {
                args.push(p);
            }
            let ran = git(&dir, &args)?;
            if !ran.success {
                return Err(failed(&ran));
            }
            Ok(Value::String(utf8(ran.stdout, "the diff")?))
        }
    }
}
