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

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use basal_proto::Primitive;
use serde_json::{Value, json};

use super::{Denial, codes, expand_home, options, string_arg};

/// How long one git command may run.
pub const TIMEOUT: Duration = Duration::from_secs(20);
const STDERR_BYTES: usize = 4096;
const PROCESS_POLL: Duration = Duration::from_millis(2);
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

/// The repository's real path, which must be one of the approved
/// repositories (each resolved the same way). A subdirectory of an approved
/// repository is not itself approved.
pub fn repo(repo: &str, repos: &[String]) -> Result<PathBuf, Denial> {
    let path = expand_home(repo)
        .ok_or_else(|| Denial::invalid(format!("{repo:?} is not an absolute path")))?;
    let refused = || Denial::denied("repository is outside the manifest's repositories or missing");
    let real = std::fs::canonicalize(&path).map_err(|_| refused())?;
    let approved = repos
        .iter()
        .filter_map(|r| expand_home(r))
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|r| r == real);
    if approved { Ok(real) } else { Err(refused()) }
}

/// The only way the built-ins run git: a `git -C <repo>` command that can
/// read the repository and run nothing else.
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
pub fn hardened_command(repo: &Path) -> Command {
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
    if let Some(parent) = repo.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    command
        .arg("-C")
        .arg(repo)
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
    command
}

/// How a git command ended.
pub struct Ran {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs a git command under [`TIMEOUT`], refusing output over
/// [`MAX_OUTPUT_BYTES`].
pub fn run_command(mut command: Command) -> Result<Ran, Denial> {
    command.process_group(0);
    let child = command
        .spawn()
        .map_err(|e| Denial::new(codes::GIT, format!("git could not start: {e}")))?;
    let mut guard = ProcessGroup(child);
    let mut out = guard.0.stdout.take();
    let mut err = guard.0.stderr.take();
    for fd in out
        .as_ref()
        .map(AsRawFd::as_raw_fd)
        .into_iter()
        .chain(err.as_ref().map(AsRawFd::as_raw_fd))
    {
        // SAFETY: the pipes remain owned by this call; only their nonblocking
        // status is changed so descendant-held pipes cannot bypass the deadline.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
            return Err(Denial::new(
                codes::GIT,
                std::io::Error::last_os_error().to_string(),
            ));
        }
    }
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    let mut status = None;
    let status = loop {
        drain_pipe(&mut out, &mut stdout, MAX_OUTPUT_BYTES + 1)?;
        drain_pipe(&mut err, &mut stderr, STDERR_BYTES)?;
        if stdout.len() > MAX_OUTPUT_BYTES {
            return Err(Denial::new(
                codes::TOO_LARGE,
                format!("git's output is larger than {MAX_OUTPUT_BYTES} bytes"),
            ));
        }
        if status.is_none() {
            status = guard
                .0
                .try_wait()
                .map_err(|e| Denial::new(codes::GIT, e.to_string()))?;
            if status.is_some() {
                guard.kill();
            }
        }
        if let Some(status) = status
            && out.is_none()
            && err.is_none()
        {
            break status;
        }
        if Instant::now() >= deadline {
            return Err(Denial::new(
                codes::TIMEOUT,
                format!("git ran longer than {} s", TIMEOUT.as_secs()),
            ));
        }
        thread::sleep(PROCESS_POLL);
    };
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    Ok(Ran {
        success: status.success(),
        code: status.code(),
        stdout,
        stderr,
    })
}

struct ProcessGroup(std::process::Child);
impl ProcessGroup {
    fn kill(&mut self) {
        // SAFETY: process_group(0) made the spawned child's pid its private
        // group id. Negative pid addresses that entire group, not the parent.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
        let _ = self.0.wait();
    }
}
fn drain_pipe<R: Read>(pipe: &mut Option<R>, kept: &mut Vec<u8>, cap: usize) -> Result<(), Denial> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(());
    };
    // Bound each turn so a continuously writing process cannot starve the
    // deadline or the other pipe; stderr is drained even after its keep cap.
    for _ in 0..16 {
        let mut chunk = [0u8; STDERR_BYTES];
        match reader.read(&mut chunk) {
            Ok(0) => {
                *pipe = None;
                break;
            }
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Denial::new(codes::GIT, error.to_string())),
        }
    }
    Ok(())
}

fn git(repo: &Path, args: &[&str]) -> Result<Ran, Denial> {
    let mut command = hardened_command(repo);
    command.args(args);
    run_command(command)
}

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
            let count = format!("--count={MAX_TAGS}");
            let mut list = vec![
                "for-each-ref",
                "--sort=-creatordate",
                "--format=%(refname:strip=2)",
                &count,
            ];
            let filter;
            if let Some(p) = pattern {
                filter = format!("refs/tags/{p}");
                list.push(&filter);
            } else {
                list.push("refs/tags/");
            }
            let ran = git(&dir, &list)?;
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
