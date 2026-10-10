//! The `fs` built-ins against real files: a path must resolve (symlinks
//! followed, `..` removed) under an approved root of the call's kind; a
//! missing path answers `{exists: false}` only when its parent is in
//! scope; a symlink swapped in between the check and the open is refused;
//! reads refuse text that is not UTF-8 or is over the cap; writes replace
//! the whole file and never through a symlink.

#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use basal_host::builtins::fs;
#[cfg(unix)]
use basal_host::builtins::fs::{Purpose, Target};
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
        let path = if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        };
        if cfg!(windows) {
            path.display().to_string().replace('/', "\\")
        } else {
            path.display().to_string()
        }
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
    #[cfg(unix)]
    assert_eq!(
        fs::read(&t.p("sub/../a.txt"), &roots, 1024).expect("read"),
        json!({ "text": "inside" })
    );
    let escape = t.p("../outside/secret.txt");
    let escape_code = if cfg!(windows) {
        codes::INVALID_ARGUMENTS
    } else {
        codes::DENIED
    };
    #[cfg(windows)]
    assert_eq!(
        code(fs::read(&t.p("sub/../a.txt"), &roots, 1024)),
        codes::INVALID_ARGUMENTS
    );
    assert_eq!(code(fs::read(&escape, &roots, 1024)), escape_code);
    assert_eq!(code(fs::stat(&escape, &roots)), escape_code);
    assert_eq!(code(fs::list(&t.p(".."), &roots)), escape_code);
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
#[cfg(unix)]
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
    // An unresolved parent uses the same privacy refusal, not an existence
    // signal that could distinguish outside missing paths from outside files.
    assert_eq!(code(fs::stat(&t.p("nodir/missing"), &roots)), codes::DENIED);
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
    #[cfg(unix)]
    symlink(t.root.join("a.txt"), t.root.join("link")).expect("link");
    #[cfg(unix)]
    assert_eq!(
        fs::list(&t.p(""), &roots).expect("list"),
        json!([
            { "name": "a.txt", "kind": "file" },
            { "name": "link", "kind": "symlink" },
            { "name": "sub", "kind": "dir" },
        ])
    );
    #[cfg(windows)]
    assert_eq!(
        fs::list(&t.p(""), &roots).expect("list"),
        json!([
            { "name": "a.txt", "kind": "file" }, { "name": "sub", "kind": "dir" }
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

    // Raw relative components are refused on Windows before resolution.
    #[cfg(windows)]
    assert_eq!(
        code(fs::write(&t.p("../outside/new.txt"), &roots, "pwned")),
        codes::INVALID_ARGUMENTS
    );
    // Unix permits symlink creation without an elevated test account.
    #[cfg(unix)]
    {
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
    }
    assert_eq!(
        code(fs::write(
            t.outside.join("new.txt").to_str().unwrap(),
            &roots,
            "pwned"
        )),
        codes::DENIED
    );
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
#[cfg(unix)]
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
#[cfg(unix)]
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
    assert_eq!(
        refused.message,
        "path is outside the manifest's roots or missing"
    );
    assert!(
        !refused.message.contains(t.outside.to_str().unwrap()),
        "{refused:?}"
    );
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

#[cfg(target_os = "linux")]
#[test]
fn linux_resolution_snapshot_refuses_a_replacement_root_directory() {
    let t = Tree::new("fs-root-identity");
    let roots = t.roots();
    let Target::Existing(real) =
        fs::resolve(&t.p("a.txt"), &roots, Purpose::Read).expect("resolve")
    else {
        panic!("existing file");
    };
    std::fs::rename(&t.root, t.base.join("root-moved")).expect("move root");
    std::fs::create_dir(&t.root).expect("replacement root");
    std::fs::write(t.root.join("a.txt"), "replacement").expect("replacement file");
    for capability in [
        fs::KernelCapability::Openat2,
        fs::KernelCapability::NoOpenat2,
    ] {
        let denial = fs::open_checked_with_capability(&real, &roots, false, capability)
            .expect_err("the replacement root has a different identity");
        assert_eq!(denial.code, codes::DENIED);
        assert_eq!(
            denial.message,
            "path is outside the manifest's roots or missing"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_outside_symlink_and_missing_outside_path_have_the_same_denial() {
    let t = Tree::new("fs-linux-privacy");
    let roots = t.roots();
    symlink(t.outside.join("secret.txt"), t.root.join("link")).expect("link");
    let link = fs::read(&t.p("link"), &roots, 1024).expect_err("outside link");
    let missing = fs::read(
        &t.outside.join("missing").display().to_string(),
        &roots,
        1024,
    )
    .expect_err("outside missing");
    assert_eq!(link.code, codes::DENIED);
    assert_eq!(link.code, missing.code);
    assert_eq!(link.message, missing.message);
    assert!(!link.message.contains(t.outside.to_str().unwrap()));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_file_root_write_replaces_only_its_basename_and_leaves_no_temp() {
    let t = Tree::new("fs-file-root");
    let path = t.p("a.txt");
    let roots = vec![path.clone()];
    let before: Vec<_> = std::fs::read_dir(&t.root)
        .expect("entries")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(
        fs::read(&path, &roots, 1024).expect("read file root"),
        json!({ "text": "inside" })
    );
    assert_eq!(
        fs::stat(&path, &roots).expect("stat file root")["kind"],
        "file"
    );
    fs::write(&path, &roots, "replacement").expect("replace file root");
    assert_eq!(
        std::fs::read_to_string(&path).expect("replacement"),
        "replacement"
    );
    assert_eq!(
        code(fs::write(&t.p("sub/b.txt"), &roots, "changed")),
        codes::DENIED
    );
    assert_eq!(
        code(fs::write(&t.p("sibling.txt"), &roots, "changed")),
        codes::DENIED
    );
    assert_eq!(
        std::fs::read_to_string(t.root.join("sub/b.txt")).expect("sibling"),
        "nested"
    );
    let after: Vec<_> = std::fs::read_dir(&t.root)
        .expect("entries")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(before.len(), after.len());
    assert!(before.iter().all(|name| after.contains(name)), "{after:?}");
    assert!(
        after
            .iter()
            .all(|name| !name.to_string_lossy().contains(".basal-"))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_stat_of_a_directory_root_does_not_require_a_parent_grant() {
    let t = Tree::new("fs-stat-root");
    let roots = t.roots();
    let stat = fs::stat(&t.p(""), &roots).expect("stat root");
    assert_eq!(stat["exists"], true);
    assert_eq!(stat["kind"], "dir");
    assert_eq!(
        code(fs::stat(t.base.to_str().unwrap(), &roots)),
        codes::DENIED
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_forced_walk_refuses_a_symlinked_intermediate_component() {
    let t = Tree::new("fs-walk-intermediate");
    let roots = t.roots();
    let Target::Existing(real) =
        fs::resolve(&t.p("sub/b.txt"), &roots, Purpose::Read).expect("resolve")
    else {
        panic!("existing file");
    };
    std::fs::rename(t.root.join("sub"), t.root.join("sub-moved")).expect("move sub");
    symlink(t.root.join("sub-moved"), t.root.join("sub")).expect("swap with in-root link");
    let denial =
        fs::open_checked_with_capability(&real, &roots, false, fs::KernelCapability::NoOpenat2)
            .expect_err("the walk must not follow even an in-root intermediate symlink");
    assert_eq!(denial.code, codes::DENIED);
    assert_eq!(
        denial.message,
        "path is outside the manifest's roots or missing"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_forced_walk_refuses_a_symlinked_ancestor_of_the_canonical_root() {
    let t = Tree::new("fs-walk-ancestor");
    let ancestor = t.base.join("ancestor");
    let canonical_root = ancestor.join("root");
    std::fs::create_dir_all(&canonical_root).expect("root");
    std::fs::write(canonical_root.join("file"), "inside").expect("file");
    let roots = vec![canonical_root.display().to_string()];
    let Target::Existing(real) = fs::resolve(
        canonical_root.join("file").to_str().unwrap(),
        &roots,
        Purpose::Read,
    )
    .expect("resolve") else {
        panic!("existing file");
    };
    let moved = t.base.join("ancestor-moved");
    std::fs::rename(&ancestor, &moved).expect("move ancestor");
    symlink(&moved, &ancestor).expect("same-inode root through a new ancestor link");
    let denial =
        fs::open_checked_with_capability(&real, &roots, false, fs::KernelCapability::NoOpenat2)
            .expect_err("every canonical ancestor must reject symlinks");
    assert_eq!(denial.code, codes::DENIED);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_forced_walk_uses_canonical_components_not_the_manifest_spelling() {
    let t = Tree::new("fs-walk-manifest-link");
    let alias = t.base.join("alias");
    symlink(&t.root, &alias).expect("manifest alias");
    let roots = vec![alias.display().to_string()];
    let Target::Existing(real) = fs::resolve(
        alias.join("sub/b.txt").to_str().unwrap(),
        &roots,
        Purpose::Read,
    )
    .expect("resolve") else {
        panic!("existing file");
    };
    let mut file =
        fs::open_checked_with_capability(&real, &roots, false, fs::KernelCapability::NoOpenat2)
            .expect("walk canonical components");
    let mut text = String::new();
    std::io::Read::read_to_string(&mut file, &mut text).expect("read");
    assert_eq!(text, "nested");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_forced_walk_refuses_a_last_component_symlink() {
    let t = Tree::new("fs-walk-last");
    let roots = t.roots();
    let Target::Existing(real) =
        fs::resolve(&t.p("a.txt"), &roots, Purpose::Read).expect("resolve")
    else {
        panic!("existing file");
    };
    std::fs::remove_file(&real).expect("remove");
    symlink(t.outside.join("secret.txt"), &real).expect("swap");
    let denial =
        fs::open_checked_with_capability(&real, &roots, false, fs::KernelCapability::NoOpenat2)
            .expect_err("last component link");
    assert_eq!(denial.code, codes::DENIED);
    assert!(denial.message.contains("became a symlink"));
}
