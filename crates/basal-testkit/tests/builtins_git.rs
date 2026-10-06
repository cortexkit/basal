//! The git built-ins against real repositories built in temp directories:
//! the answers, the approved-repository check, argument checks, and that a
//! repository's own configuration cannot make a built-in run a program.

use basal_host::builtins::git::{self, Op, hardened_command, run_command};
use basal_host::builtins::{Denial, codes};
use basal_proto::Primitive;
use basal_testkit::git::git_command;
use basal_testkit::harness::scratch;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Runs git to build a fixture, with no outside configuration.
fn setup_git(dir: &Path, args: &[&str], date: &str) {
    let status = git_command()
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

/// A repository with three commits and two tags.
struct Repo {
    base: PathBuf,
    dir: PathBuf,
}

impl Repo {
    fn new(tag: &str) -> Self {
        let base = scratch(tag);
        let dir = base.join("repo");
        std::fs::create_dir_all(&dir).expect("dir");
        let date = |s: i64| format!("@{s} +0000");
        setup_git(&dir, &["init", "-q", "-b", "main"], &date(0));
        std::fs::write(dir.join("notes.md"), "one\n").expect("file");
        setup_git(&dir, &["add", "notes.md"], &date(0));
        setup_git(&dir, &["commit", "-q", "-m", "first"], &date(1_700_000_000));
        setup_git(&dir, &["tag", "v1.0.0"], &date(1_700_000_000));
        std::fs::write(dir.join("notes.md"), "one\ntwo\n").expect("file");
        setup_git(
            &dir,
            &["commit", "-q", "-am", "second"],
            &date(1_700_000_100),
        );
        setup_git(
            &dir,
            &["tag", "-a", "v1.1.0", "-m", "release"],
            &date(1_700_000_100),
        );
        std::fs::write(dir.join("other.txt"), "x\n").expect("file");
        setup_git(&dir, &["add", "other.txt"], &date(1_700_000_200));
        setup_git(&dir, &["commit", "-q", "-m", "third"], &date(1_700_000_200));
        Self { base, dir }
    }

    fn repos(&self) -> Vec<String> {
        vec![self.dir.display().to_string()]
    }

    fn path(&self) -> String {
        self.dir.display().to_string()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn run(repo: &Repo, primitive: Primitive, args: Value) -> Result<Value, Denial> {
    let op = git::parse(primitive, &args)?;
    git::run(&repo.path(), &repo.repos(), &op)
}

#[test]
fn git_reads_answer_log_rev_parse_tags_show_and_diff() {
    let repo = Repo::new("git-reads");
    let log = run(&repo, Primitive::GitLog, json!({})).expect("log");
    let subjects: Vec<&str> = log
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["subject"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(subjects, ["third", "second", "first"]);
    assert_eq!(log[0]["author_ms"], 1_700_000_200_000_i64);
    assert_eq!(log[0]["sha"].as_str().map(str::len), Some(40));

    let limited = run(
        &repo,
        Primitive::GitLog,
        json!({ "options": { "path": "notes.md", "sinceMs": 1_700_000_050_000_i64, "maxCount": 5 } }),
    )
    .expect("log");
    assert_eq!(limited.as_array().map(Vec::len), Some(1), "{limited}");
    assert_eq!(limited[0]["subject"], "second");

    let head = run(&repo, Primitive::GitRevParse, json!({ "ref": "HEAD" })).expect("rev-parse");
    assert_eq!(head, log[0]["sha"]);
    // An annotated tag answers the commit it points at.
    let tagged = run(&repo, Primitive::GitRevParse, json!({ "ref": "v1.1.0" })).expect("tag");
    assert_eq!(tagged, log[1]["sha"]);
    let missing =
        run(&repo, Primitive::GitRevParse, json!({ "ref": "nope" })).expect_err("missing");
    assert_eq!(missing.code, codes::NOT_FOUND);

    let tags = run(&repo, Primitive::GitDescribeTags, json!({})).expect("tags");
    assert_eq!(tags["nearest"], "v1.1.0");
    assert_eq!(tags["tags"], json!(["v1.1.0", "v1.0.0"]));
    let matched = run(
        &repo,
        Primitive::GitDescribeTags,
        json!({ "options": { "pattern": "v1.0.*" } }),
    )
    .expect("tags");
    assert_eq!(matched, json!({ "nearest": "v1.0.0", "tags": ["v1.0.0"] }));

    let blob = run(
        &repo,
        Primitive::GitShow,
        json!({ "rev": "v1.0.0", "path": "notes.md" }),
    )
    .expect("show");
    assert_eq!(blob, json!("one\n"));

    let diff = run(
        &repo,
        Primitive::GitDiff,
        json!({ "repo": repo.path(), "from": "v1.0.0", "to": "HEAD", "options": { "path": "notes.md" } }),
    )
    .expect("diff");
    let diff = diff.as_str().expect("text");
    assert!(diff.contains("+two"), "{diff}");
    assert!(!diff.contains("other.txt"), "{diff}");
}

#[test]
fn git_reads_only_approved_repositories() {
    let approved = Repo::new("git-approved");
    let other = Repo::new("git-other");
    let op = Op::RevParse {
        reference: "HEAD".into(),
    };
    let refused = git::run(&other.path(), &approved.repos(), &op).expect_err("refused");
    assert_eq!(refused.code, codes::DENIED);
    // A directory inside an approved repository is not itself approved.
    std::fs::create_dir_all(approved.dir.join("sub")).expect("sub");
    let sub = approved.dir.join("sub").display().to_string();
    assert_eq!(
        git::run(&sub, &approved.repos(), &op)
            .expect_err("sub")
            .code,
        codes::DENIED
    );
    // The same repository reached through `..` resolves to the approved
    // path and is allowed.
    let around = approved.dir.join("sub/..").display().to_string();
    assert!(git::run(&around, &approved.repos(), &op).is_ok());
}

#[test]
fn arguments_that_git_could_read_as_options_are_refused() {
    for (primitive, args) in [
        (Primitive::GitRevParse, json!({ "ref": "--output=/tmp/x" })),
        (
            Primitive::GitShow,
            json!({ "rev": "HEAD", "path": "../../etc/passwd" }),
        ),
        (
            Primitive::GitShow,
            json!({ "rev": "HEAD", "path": "/etc/passwd" }),
        ),
        (Primitive::GitDiff, json!({ "from": "-p", "to": "HEAD" })),
        (
            Primitive::GitDescribeTags,
            json!({ "options": { "pattern": "--all" } }),
        ),
        (Primitive::GitLog, json!({ "options": { "maxCount": 0 } })),
        (Primitive::GitLog, json!({ "options": { "colour": true } })),
    ] {
        let refused = git::parse(primitive, &args).expect_err("refused");
        assert_eq!(refused.code, codes::INVALID_ARGUMENTS, "{args}");
    }
}

/// A script `name` in `dir` that appends its name to `marker` when run.
fn plant(dir: &Path, name: &str, marker: &Path) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho {name} >> '{}'\nexit 0\n", marker.display()),
    )
    .expect("script");
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

/// A repository whose `.git/config` names programs for `core.fsmonitor`,
/// an external diff, a pager and a hooks directory. None of them runs
/// through a built-in, nor through any command `hardened_command` builds,
/// while plain git in the same repository does run the fsmonitor program,
/// which shows the plant works.
#[test]
fn a_repository_config_cannot_make_a_built_in_run_a_program() {
    let repo = Repo::new("git-fsmonitor");
    let marker = repo.base.join("marker");
    let fsmonitor = plant(&repo.base, "fsmonitor", &marker);
    let ext_diff = plant(&repo.base, "extdiff", &marker);
    let pager = plant(&repo.base, "pager", &marker);
    let hooks = repo.base.join("hooks");
    std::fs::create_dir_all(&hooks).expect("hooks");
    plant(&hooks, "post-checkout", &marker);
    plant(&hooks, "reference-transaction", &marker);
    let config = repo.dir.join(".git/config");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "[core]\n\tfsmonitor = {}\n\tpager = {}\n\thooksPath = {}\n[diff]\n\texternal = {}\n",
        fsmonitor.display(),
        pager.display(),
        hooks.display(),
        ext_diff.display(),
    ));
    std::fs::write(&config, text).expect("plant config");

    // The plant works: git still honors repository-local fsmonitor settings.
    let status = git_command()
        .current_dir(&repo.dir)
        .args(["status", "--porcelain"])
        .output()
        .expect("git status");
    assert!(status.status.success());
    assert!(
        std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .contains("fsmonitor"),
        "plain git did not run the planted fsmonitor"
    );
    std::fs::remove_file(&marker).expect("reset marker");

    for (primitive, args) in [
        (
            Primitive::GitLog,
            json!({ "options": { "path": "notes.md" } }),
        ),
        (Primitive::GitRevParse, json!({ "ref": "HEAD" })),
        (Primitive::GitDescribeTags, json!({})),
        (
            Primitive::GitShow,
            json!({ "rev": "HEAD", "path": "notes.md" }),
        ),
        (
            Primitive::GitDiff,
            json!({ "from": "v1.0.0", "to": "HEAD" }),
        ),
    ] {
        run(&repo, primitive, args).expect("git read");
    }
    // The same command factory with a command that reads the work tree,
    // the kind `core.fsmonitor` exists for.
    let mut command = hardened_command(&repo.dir);
    command.args(["status", "--porcelain"]);
    let ran = run_command(command).expect("ran");
    assert!(ran.success, "{}", ran.stderr);
    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(ran, "", "a planted program ran");
}
