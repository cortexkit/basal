use basal_host::builtins::{BuiltinHost, Grant, codes, envelope, fs, git, net};
use basal_host::{CallClass, CallRequest, Dispatched, Host};
use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use serde_json::json;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "basal-builtins-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("root")).unwrap();
        Self(path)
    }
    fn roots(&self) -> Vec<String> {
        vec![self.0.join("root").display().to_string()]
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn escaped_800kb_file_is_a_typed_refusal_not_successful_null() {
    let tree = Tree::new();
    let path = tree.0.join("root/escaped");
    std::fs::write(&path, vec![0; 800 * 1024]).unwrap();
    let request = CallRequest {
        flow_id: "f".into(),
        run_id: "r".into(),
        position: 0,
        kind: CallKind::Primitive(Primitive::FsRead),
        args: JsonText::new(
            envelope(
                &json!({"path": path}),
                &Grant {
                    roots: tree.roots(),
                    hosts: vec![],
                },
            )
            .to_string(),
        )
        .unwrap(),
        idempotency_key: "k".into(),
        attempt: 1,
    };
    let Dispatched::Completed(outcome) = BuiltinHost::default().dispatch(&request).unwrap() else {
        panic!("not completed")
    };
    assert_eq!(outcome.settlement, Settlement::Rejected);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(outcome.value.as_str()).unwrap()["code"],
        codes::TOO_LARGE
    );
}

#[test]
fn escaped_listing_exceeds_encoded_cap_as_a_typed_refusal() {
    let tree = Tree::new();
    let dir = tree.0.join("root");
    // Fewer than the entry cap, but valid filenames whose JSON escaping alone
    // exceeds the encoded-result cap. This reaches the dispatch encoder rather
    // than a raw file/body byte cap.
    for n in 0..800 {
        std::fs::write(dir.join(format!("{}{n:04}", "\u{0001}".repeat(248))), "").unwrap();
    }
    let request = CallRequest {
        flow_id: "f".into(),
        run_id: "r".into(),
        position: 0,
        kind: CallKind::Primitive(Primitive::FsList),
        args: JsonText::new(
            envelope(
                &json!({"path":dir}),
                &Grant {
                    roots: tree.roots(),
                    hosts: vec![],
                },
            )
            .to_string(),
        )
        .unwrap(),
        idempotency_key: "k".into(),
        attempt: 1,
    };
    let Dispatched::Completed(outcome) = BuiltinHost::default().dispatch(&request).unwrap() else {
        panic!("not completed")
    };
    assert_eq!(outcome.settlement, Settlement::Rejected);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(outcome.value.as_str()).unwrap()["code"],
        codes::TOO_LARGE
    );
}

#[test]
fn outside_missing_and_symlink_targets_have_identical_refusals() {
    let tree = Tree::new();
    let existing = tree.0.join("secret");
    std::fs::write(&existing, "secret").unwrap();
    let link = tree.0.join("root/link");
    symlink(&existing, &link).unwrap();
    let missing = tree.0.join("absent/child");
    for purpose in [fs::Purpose::Read, fs::Purpose::Write] {
        let a = fs::resolve(existing.to_str().unwrap(), &tree.roots(), purpose).unwrap_err();
        let b = fs::resolve(missing.to_str().unwrap(), &tree.roots(), purpose).unwrap_err();
        assert_eq!(a, b);
        if purpose == fs::Purpose::Read {
            assert_eq!(
                a,
                fs::resolve(link.to_str().unwrap(), &tree.roots(), purpose).unwrap_err()
            );
        }
        assert_eq!(a.code, codes::DENIED);
        assert!(!a.message.contains(tree.0.to_str().unwrap()));
    }
    assert_eq!(
        git::repo(existing.to_str().unwrap(), &tree.roots()).unwrap_err(),
        git::repo(missing.to_str().unwrap(), &tree.roots()).unwrap_err()
    );
}

#[test]
fn atomic_write_drops_special_permission_bits() {
    let tree = Tree::new();
    let path = tree.0.join("root/file");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o6755)).unwrap();
    fs::write(path.to_str().unwrap(), &tree.roots(), "").unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o755
    );
}

#[test]
fn atomic_write_accepts_a_maximum_length_basename() {
    let tree = Tree::new();
    let path = tree.0.join("root").join("x".repeat(255));
    fs::write(path.to_str().unwrap(), &tree.roots(), "new").unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
}

#[test]
fn unreadable_fetch_arguments_use_the_nonkeyed_mutation_class() {
    assert_eq!(
        BuiltinHost::default().classify(&CallKind::Primitive(Primitive::NetFetch)),
        CallClass::Mutation {
            honours_idempotency_keys: false
        }
    );
    assert!(net::method_of(&serde_json::Value::Null).is_err());
}

#[test]
fn method_override_and_compression_headers_are_denied() {
    for header in [
        "X-HTTP-Method-Override",
        "X-HTTP-Method",
        "X-Method-Override",
        "Accept-Encoding",
        "User-Agent",
    ] {
        let args = json!({"url":"https://api.test/", "options":{"headers":{header:"DELETE"}}});
        assert_eq!(
            net::parse_request(&args).unwrap_err().code,
            codes::DENIED,
            "{header}"
        );
    }
}

#[test]
fn git_reaps_descendants_that_keep_its_output_pipe_open() {
    use std::process::{Command, Stdio};
    use std::time::Duration;
    let tree = Tree::new();
    let pid_file = tree.0.join("descendant");
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "sleep 120 & echo $! > \"$1\"; exit 0", "sh"])
        .arg(&pid_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = sender.send(git::run_command(command));
    });
    let result = receiver.recv_timeout(Duration::from_secs(30));
    // The deadline is only a hang stopper. On either path, explicitly reap the
    // test's descendant before asserting so a broken implementation leaks none.
    if !matches!(result, Ok(Ok(ref ran)) if ran.success)
        && let Ok(pid) = std::fs::read_to_string(&pid_file)
            .and_then(|s| s.trim().parse::<i32>().map_err(std::io::Error::other))
    {
        // SAFETY: pid was written by the child launched only by this test.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    worker.join().unwrap();
    assert!(
        result
            .expect("git exceeded its command and pipe deadline")
            .unwrap()
            .success
    );
}

#[test]
fn tag_patterns_use_git_globs_including_hierarchical_names() {
    let tree = Tree::new();
    let repo = tree.0.join("root");
    let git_command = |args: &[&str]| {
        let result = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    git_command(&["init", "--quiet"]);
    git_command(&["commit", "--quiet", "--allow-empty", "-m", "initial"]);
    git_command(&["tag", "v1/child"]);
    let tags = |pattern: &str| {
        git::run(
            repo.to_str().unwrap(),
            &tree.roots(),
            &git::Op::DescribeTags {
                pattern: Some(pattern.into()),
            },
        )
        .unwrap()["tags"]
            .clone()
    };
    assert_eq!(
        tags("v1"),
        json!([]),
        "a literal tag name is not a directory prefix"
    );
    assert_eq!(
        tags("v*"),
        json!(["v1/child"]),
        "git tag globs match across slashes"
    );
}
