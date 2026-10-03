//! The `fs` built-ins against real files: a path must resolve (symlinks
//! followed, `..` removed) under an approved root of the call's kind; a
//! missing path answers `{exists: false}` only when its parent is in
//! scope; a symlink swapped in between the check and the open is refused;
//! reads refuse text that is not UTF-8 or is over the cap; writes replace
//! the whole file and never through a symlink.

use std::os::unix::fs::symlink;
use std::path::PathBuf;

use basal_host::builtins::fs::{self, Purpose, Target};
use basal_host::builtins::{BuiltinHost, Denial, Failure, Grant, codes, envelope};
use basal_proto::Primitive;
use basal_testkit::harness::scratch;
use serde_json::{Value, json};

/// A root with a file, and a sibling directory outside it holding a
/// secret.
struct Tree {
    base: PathBuf,
    root: PathBuf,
    outside: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Self {
        let base = scratch(tag);
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("sub")).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(root.join("a.txt"), "inside").expect("a.txt");
        std::fs::write(root.join("sub/b.txt"), "nested").expect("b.txt");
        std::fs::write(outside.join("secret.txt"), "secret").expect("secret");
        Self {
            base,
            root,
            outside,
        }
    }

    fn roots(&self) -> Vec<String> {
        vec![self.root.display().to_string()]
    }

    fn p(&self, rel: &str) -> String {
        self.root.join(rel).display().to_string()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn code(result: Result<Value, Denial>) -> &'static str {
    match result {
        Ok(v) => panic!("expected a refusal, got {v}"),
        Err(d) => d.code,
    }
}

#[test]
fn reads_inside_a_root_and_refuses_a_dotdot_escape() {
    let t = Tree::new("fs-dotdot");
    let roots = t.roots();
    assert_eq!(
        fs::read(&t.p("a.txt"), &roots, 1024).expect("read"),
        json!({ "text": "inside" })
    );
    assert_eq!(
        fs::read(&t.p("sub/../a.txt"), &roots, 1024).expect("read"),
        json!({ "text": "inside" })
    );
    let escape = t.p("../outside/secret.txt");
    assert_eq!(code(fs::read(&escape, &roots, 1024)), codes::DENIED);
    assert_eq!(code(fs::stat(&escape, &roots)), codes::DENIED);
    assert_eq!(code(fs::list(&t.p(".."), &roots)), codes::DENIED);
    // A sibling whose name only starts with the root's is not under it.
    let sibling = t.base.join("rootx");
    std::fs::create_dir_all(&sibling).expect("sibling");
    std::fs::write(sibling.join("f"), "x").expect("f");
    assert_eq!(
        code(fs::read(
            &sibling.join("f").display().to_string(),
            &roots,
            1024
        )),
        codes::DENIED
    );
    // A relative path is not a path into any root.
    assert_eq!(
        code(fs::read("root/a.txt", &roots, 1024)),
        codes::INVALID_ARGUMENTS
    );
}

#[test]
fn a_symlink_resolving_outside_the_root_is_refused() {
    let t = Tree::new("fs-symlink");
    let roots = t.roots();
    symlink(t.outside.join("secret.txt"), t.root.join("out-link")).expect("link");
    symlink(&t.outside, t.root.join("out-dir")).expect("dir link");
    symlink(t.root.join("a.txt"), t.root.join("in-link")).expect("in link");
    assert_eq!(
        code(fs::read(&t.p("out-link"), &roots, 1024)),
        codes::DENIED
    );
    assert_eq!(
        code(fs::read(&t.p("out-dir/secret.txt"), &roots, 1024)),
        codes::DENIED
    );
    assert_eq!(code(fs::list(&t.p("out-dir"), &roots)), codes::DENIED);
    assert_eq!(
        code(fs::stat(&t.p("out-dir/missing"), &roots)),
        codes::DENIED
    );
    // A symlink that stays inside is followed.
    assert_eq!(
        fs::read(&t.p("in-link"), &roots, 1024).expect("read"),
        json!({ "text": "inside" })
    );
}

#[test]
fn stat_of_a_missing_path_is_exists_false_only_when_its_parent_is_in_scope() {
    let t = Tree::new("fs-stat");
    let roots = t.roots();
    assert_eq!(
        fs::stat(&t.p("missing"), &roots).expect("stat"),
        json!({ "exists": false })
    );
    assert_eq!(
        fs::stat(&t.p("sub/missing"), &roots).expect("stat"),
        json!({ "exists": false })
    );
    // The parent is outside: no answer at all, so a script cannot probe
    // what exists beyond its roots.
    let outside_missing = t.outside.join("missing").display().to_string();
    assert_eq!(code(fs::stat(&outside_missing, &roots)), codes::DENIED);
    // The parent does not exist either: an error, not `exists: false`.
    assert_eq!(
        code(fs::stat(&t.p("nodir/missing"), &roots)),
        codes::NOT_FOUND
    );
    let file = fs::stat(&t.p("a.txt"), &roots).expect("stat");
    assert_eq!(file["exists"], true);
    assert_eq!(file["kind"], "file");
    assert_eq!(file["size"], 6);
    assert!(file["mtime_ms"].as_i64().unwrap_or(0) > 1_600_000_000_000);
    assert_eq!(fs::stat(&t.p("sub"), &roots).expect("stat")["kind"], "dir");
}

#[test]
fn list_names_entries_and_their_kinds() {
    let t = Tree::new("fs-list");
    let roots = t.roots();
    symlink(t.root.join("a.txt"), t.root.join("link")).expect("link");
    assert_eq!(
        fs::list(&t.p(""), &roots).expect("list"),
        json!([
            { "name": "a.txt", "kind": "file" },
            { "name": "link", "kind": "symlink" },
            { "name": "sub", "kind": "dir" },
        ])
    );
    for i in 0..=fs::MAX_LIST_ENTRIES {
        std::fs::write(t.root.join(format!("sub/f{i}")), "").expect("file");
    }
    assert_eq!(code(fs::list(&t.p("sub"), &roots)), codes::TOO_LARGE);
}

#[test]
fn read_refuses_text_that_is_not_utf8_or_is_over_the_cap() {
    let t = Tree::new("fs-caps");
    let roots = t.roots();
    std::fs::write(t.root.join("bin"), [0xff, 0xfe, 0x00]).expect("bin");
    assert_eq!(code(fs::read(&t.p("bin"), &roots, 1024)), codes::NOT_UTF8);
    std::fs::write(t.root.join("big"), "x".repeat(2048)).expect("big");
    assert_eq!(code(fs::read(&t.p("big"), &roots, 2047)), codes::TOO_LARGE);
    assert_eq!(
        fs::read(&t.p("big"), &roots, 2048).expect("read")["text"]
            .as_str()
            .map(str::len),
        Some(2048)
    );
    // The cap a script may ask for is bounded.
    let host = BuiltinHost::default();
    let grant = Grant {
        roots: roots.clone(),
        hosts: Vec::new(),
    };
    let over = json!({ "path": t.p("big"), "options": { "maxBytes": fs::MAX_READ_BYTES + 1 } });
    match host.run(Primitive::FsRead, &envelope(&over, &grant)) {
        Err(Failure::Refused(d)) => assert_eq!(d.code, codes::INVALID_ARGUMENTS),
        other => panic!("{other:?}"),
    }
    let default = json!({ "path": t.p("big") });
    assert!(
        host.run(Primitive::FsRead, &envelope(&default, &grant))
            .is_ok()
    );
}

#[test]
fn write_replaces_the_whole_file_inside_a_write_root_only() {
    let t = Tree::new("fs-write");
    let roots = t.roots();
    let target = t.p("out.txt");
    assert_eq!(
        fs::write(&target, &roots, "first version").expect("write"),
        json!({ "bytes": 13 })
    );
    fs::write(&target, &roots, "second").expect("write");
    assert_eq!(
        std::fs::read_to_string(&target).expect("read back"),
        "second"
    );
    // Writing the same text again leaves the same file.
    fs::write(&target, &roots, "second").expect("write again");
    assert_eq!(std::fs::read_to_string(&target).expect("read"), "second");
    // No temporary file is left behind.
    let names: Vec<String> = std::fs::read_dir(&t.root)
        .expect("dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.contains(".basal-")), "{names:?}");

    // Outside the root, through `..`, through a directory symlink, or onto
    // a symlink: refused, and nothing outside changes.
    symlink(&t.outside, t.root.join("out-dir")).expect("dir link");
    symlink(t.outside.join("secret.txt"), t.root.join("out-link")).expect("link");
    for path in [
        t.p("../outside/new.txt"),
        t.p("out-dir/new.txt"),
        t.p("out-link"),
    ] {
        assert_eq!(
            code(fs::write(&path, &roots, "pwned")),
            codes::DENIED,
            "{path}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(t.outside.join("secret.txt")).expect("secret"),
        "secret"
    );
    assert!(!t.outside.join("new.txt").exists());
    assert_eq!(
        code(fs::write(
            &t.p("big"),
            &roots,
            &"x".repeat(fs::MAX_WRITE_BYTES + 1)
        )),
        codes::TOO_LARGE
    );
}

/// The race the open closes: a path resolved inside the root, whose last
/// component is then swapped for a symlink to a file outside. The open
/// refuses to follow it.
#[test]
fn a_symlink_swapped_into_the_last_component_after_the_check_is_not_followed() {
    let t = Tree::new("fs-swap-last");
    let roots = t.roots();
    let Target::Existing(real) =
        fs::resolve(&t.p("a.txt"), &roots, Purpose::Read).expect("resolve")
    else {
        panic!("a.txt exists");
    };
    std::fs::remove_file(&real).expect("remove");
    symlink(t.outside.join("secret.txt"), &real).expect("swap");
    let refused = fs::open_checked(&real, &roots, false).expect_err("refused");
    assert_eq!(refused.code, codes::DENIED);
    assert!(refused.message.contains("became a symlink"), "{refused:?}");
}

/// The other half: a directory higher up swapped for a symlink to a
/// directory outside after the check. The open succeeds, but the opened
/// file's real path lies outside the root, so it is refused.
#[test]
fn a_directory_swapped_for_a_symlink_after_the_check_is_caught_after_the_open() {
    let t = Tree::new("fs-swap-dir");
    let roots = t.roots();
    let Target::Existing(real) =
        fs::resolve(&t.p("sub/b.txt"), &roots, Purpose::Read).expect("resolve")
    else {
        panic!("sub/b.txt exists");
    };
    std::fs::create_dir_all(t.outside.join("sub")).expect("outside sub");
    std::fs::write(t.outside.join("sub/b.txt"), "secret").expect("outside b");
    std::fs::rename(t.root.join("sub"), t.root.join("sub-moved")).expect("move");
    symlink(t.outside.join("sub"), t.root.join("sub")).expect("swap");
    let refused = fs::open_checked(&real, &roots, false).expect_err("refused");
    assert_eq!(refused.code, codes::DENIED);
    assert!(refused.message.contains("moved outside"), "{refused:?}");
}

/// The host checks the scope carried with the call, not anything the
/// script says: a grant of another root does not reach this one.
#[test]
fn the_host_checks_the_grant_carried_with_the_call() {
    let t = Tree::new("fs-grant");
    let host = BuiltinHost::default();
    let other = Grant {
        roots: vec![t.outside.display().to_string()],
        hosts: Vec::new(),
    };
    match host.run(
        Primitive::FsRead,
        &envelope(&json!({ "path": t.p("a.txt") }), &other),
    ) {
        Err(Failure::Refused(d)) => assert_eq!(d.code, codes::DENIED),
        other => panic!("{other:?}"),
    }
    match host.run(
        Primitive::FsRead,
        &envelope(&json!({ "path": t.p("a.txt") }), &Grant::default()),
    ) {
        Err(Failure::Refused(d)) => assert_eq!(d.code, codes::DENIED),
        other => panic!("{other:?}"),
    }
}
