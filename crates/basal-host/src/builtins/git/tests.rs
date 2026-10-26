use super::*;
#[cfg(unix)]
use std::process::Stdio;

#[cfg(unix)]
thread_local! {
    static BEFORE_EXEC: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(unix)]
pub(super) fn before_exec() {
    let hook = BEFORE_EXEC.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(target_os = "macos")]
thread_local! {
    static BEFORE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(target_os = "macos")]
pub(super) fn before_open() {
    let hook = BEFORE_OPEN.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[test]
fn overflowing_or_malformed_log_timestamps_are_refused_not_dropped() {
    assert!(parse_log("abc\0bad\0subject\n").is_err());
    assert!(parse_log("abc\09223372036854775807\0subject\n").is_err());
    assert_eq!(
        parse_log("abc\x001\0subject\n").unwrap()[0]["author_ms"],
        1000
    );
}

#[test]
fn show_distinguishes_missing_blob_from_broken_repository() {
    let ran = Ran {
        success: false,
        code: Some(128),
        stdout: vec![],
        stderr: "fatal: detected dubious ownership in repository".into(),
    };
    assert_eq!(show_failure(&ran).code, codes::GIT);
}

#[test]
#[cfg(unix)]
fn tag_output_is_bounded_while_reading_not_after_completion() {
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "i=0; while [ \"$i\" -lt 4000 ]; do printf 'tag-%080d\\n' \"$i\"; i=$((i+1)); done",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let result = run_tags_command(command).unwrap();
    assert!(result.success);
    assert_eq!(
        String::from_utf8(result.stdout).unwrap().lines().count(),
        MAX_TAGS
    );
}

#[test]
#[cfg(unix)]
fn git_waiter_timeout_kills_the_process_group_and_reaps_the_child() {
    let directory = std::env::temp_dir().join(format!("basal-git-timeout-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let pid_file = directory.join("pid");
    let survivor = directory.join("survivor");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "echo $$ > \"$1\"; (sleep \"$3\"; echo survived > \"$2\") & wait",
            "sh",
        ])
        .arg(&pid_file)
        .arg(&survivor)
        // The descendant outlives the short timeout below by well over a
        // second, so a reap that waited for it would let it write.
        .arg("2")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let error = run_command_until(command, None, Duration::from_millis(500))
        .err()
        .expect("git must time out");
    let pid: i32 = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The bounded descendant writes only if timeout cleanup fails to kill the
    // group. No elapsed-time assertion is needed to observe that failure.
    let survived = survivor.exists();
    std::fs::remove_dir_all(directory).unwrap();
    assert_eq!(error.code, codes::TIMEOUT);
    assert!(!survived, "git's descendant survived the timeout");
    // SAFETY: pid names the child this test spawned. A completed waiter must
    // already have reaped it, leaving no child for waitpid to collect.
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[cfg(unix)]
mod repository {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "basal-git-pin-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(std::fs::canonicalize(dir).unwrap())
        }

        fn git(&self, dir: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim_end().into()
        }

        fn create_repo(&self, name: &str, content: &str) -> PathBuf {
            let dir = self.0.join(name).join("repo");
            std::fs::create_dir_all(&dir).unwrap();
            self.git(&dir, &["init", "-q", "-b", "main"]);
            std::fs::write(dir.join("value"), content).unwrap();
            self.git(&dir, &["add", "value"]);
            self.git(&dir, &["commit", "-q", "-m", content]);
            dir
        }

        fn read(&self, dir: &Path, roots: &[PathBuf]) -> Result<Value, Denial> {
            run(
                dir.to_str().unwrap(),
                &roots
                    .iter()
                    .map(|root| root.to_str().unwrap().to_owned())
                    .collect::<Vec<_>>(),
                &Op::Show {
                    rev: "HEAD".into(),
                    path: "value".into(),
                },
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn ancestor_swap_after_verified_open_never_reads_the_replacement() {
        let fixture = Fixture::new();
        let original = fixture.create_repo("approved", "approved");
        let replacement = fixture.create_repo("replacement", "unapproved");
        let (ready_tx, ready_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        thread::scope(|scope| {
            let reader = scope.spawn(|| {
                BEFORE_EXEC.with(|slot| {
                    *slot.borrow_mut() = Some(Box::new(move || {
                        ready_tx.send(()).unwrap();
                        go_rx.recv().unwrap();
                    }));
                });
                fixture.read(&original, std::slice::from_ref(&original))
            });
            ready_rx.recv().unwrap();
            std::fs::rename(original.parent().unwrap(), fixture.0.join("held")).unwrap();
            symlink(replacement.parent().unwrap(), original.parent().unwrap()).unwrap();
            go_tx.send(()).unwrap();
            match reader.join().unwrap() {
                Ok(value) => assert_eq!(value, json!("approved")),
                Err(error) => assert_eq!(error.code, codes::DENIED),
            }
        });
    }

    #[test]
    fn normal_and_symlinked_roots_read_the_verified_repository() {
        let fixture = Fixture::new();
        let original = fixture.create_repo("approved", "approved");
        assert_eq!(
            fixture
                .read(&original, std::slice::from_ref(&original))
                .unwrap(),
            json!("approved")
        );
        let link = fixture.0.join("link");
        symlink(&original, &link).unwrap();
        assert_eq!(
            fixture.read(&link, std::slice::from_ref(&link)).unwrap(),
            json!("approved")
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn ancestor_swap_before_macos_verified_open_is_refused() {
        let fixture = Fixture::new();
        let original = fixture.create_repo("approved", "approved");
        let replacement = fixture.create_repo("replacement", "unapproved");
        let ancestor = original.parent().unwrap().to_owned();
        let target = replacement.parent().unwrap().to_owned();
        let held = fixture.0.join("held");
        BEFORE_OPEN.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&ancestor, held).unwrap();
                symlink(target, ancestor).unwrap();
            }));
        });
        assert_eq!(
            fixture
                .read(&original, std::slice::from_ref(&original))
                .unwrap_err()
                .code,
            codes::DENIED
        );
    }

    #[test]
    fn gitdir_outside_approved_roots_is_refused() {
        let fixture = Fixture::new();
        let original = fixture.create_repo("approved", "approved");
        let other = fixture.create_repo("other", "unapproved");
        std::fs::remove_dir_all(original.join(".git")).unwrap();
        std::fs::write(
            original.join(".git"),
            format!("gitdir: {}\n", other.join(".git").display()),
        )
        .unwrap();
        assert_eq!(
            fixture
                .read(&original, std::slice::from_ref(&original))
                .unwrap_err()
                .code,
            codes::DENIED
        );
    }

    fn borrow_objects(fixture: &Fixture, original: &Path, donor: &Path, target: &str) {
        let head = fixture.git(donor, &["rev-parse", "HEAD"]);
        std::fs::write(original.join(".git/refs/heads/main"), format!("{head}\n")).unwrap();
        std::fs::write(original.join(".git/objects/info/alternates"), target).unwrap();
        assert_eq!(fixture.git(original, &["show", "HEAD:value"]), "borrowed");
    }

    #[test]
    fn alternates_outside_approved_roots_are_refused() {
        let fixture = Fixture::new();
        let original = fixture.create_repo("approved", "approved");
        let donor = fixture.create_repo("donor", "borrowed");
        borrow_objects(
            &fixture,
            &original,
            &donor,
            &format!("{}\n", donor.join(".git/objects").display()),
        );
        assert_eq!(
            fixture
                .read(&original, std::slice::from_ref(&original))
                .unwrap_err()
                .code,
            codes::DENIED
        );
    }

    #[test]
    fn relative_and_quoted_alternates_inside_approved_roots_are_allowed() {
        let fixture = Fixture::new();
        let original = fixture.create_repo("approved", "approved");
        let donor = fixture.create_repo("approved/repo/donor\tstore", "borrowed");
        borrow_objects(
            &fixture,
            &original,
            &donor,
            "# object store\n\"../../donor\\tstore/repo/.git/objects\"\n",
        );
        assert_eq!(
            fixture
                .read(&original, std::slice::from_ref(&original))
                .unwrap(),
            json!("borrowed")
        );
        let separate = fixture.create_repo("separate", "borrowed");
        borrow_objects(
            &fixture,
            &original,
            &separate,
            &format!("{}\n", separate.join(".git/objects").display()),
        );
        assert_eq!(
            fixture
                .read(&original, &[original.clone(), separate])
                .unwrap(),
            json!("borrowed")
        );
    }

    #[test]
    fn recursive_alternates_cannot_escape_approved_roots() {
        let fixture = Fixture::new();
        let original = fixture.create_repo("approved", "approved");
        let inside = fixture.create_repo("approved/repo/inside", "inside");
        let outside = fixture.create_repo("outside", "borrowed");
        std::fs::write(
            inside.join(".git/objects/info/alternates"),
            format!("{}\n", outside.join(".git/objects").display()),
        )
        .unwrap();
        borrow_objects(
            &fixture,
            &original,
            &outside,
            "../../inside/repo/.git/objects\n",
        );
        assert_eq!(
            fixture
                .read(&original, std::slice::from_ref(&original))
                .unwrap_err()
                .code,
            codes::DENIED
        );
    }

    #[test]
    fn linked_worktree_checks_the_common_object_store() {
        let fixture = Fixture::new();
        let original = fixture.create_repo("approved", "approved");
        let worktree = fixture.0.join("worktree");
        fixture.git(
            &original,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "linked",
                worktree.to_str().unwrap(),
            ],
        );
        let roots = vec![original.clone(), worktree.clone()];
        assert_eq!(fixture.read(&worktree, &roots).unwrap(), json!("approved"));
        let outside = fixture.create_repo("outside", "borrowed");
        std::fs::write(
            original.join(".git/objects/info/alternates"),
            format!("{}\n", outside.join(".git/objects").display()),
        )
        .unwrap();
        assert_eq!(
            fixture.read(&worktree, &roots).unwrap_err().code,
            codes::DENIED
        );
    }

    #[test]
    fn commondir_outside_approved_roots_is_refused() {
        let fixture = Fixture::new();
        let original = fixture.create_repo("approved", "approved");
        let outside = fixture.create_repo("outside", "borrowed");
        let private = original.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::copy(original.join(".git/HEAD"), private.join("HEAD")).unwrap();
        std::fs::write(
            private.join("commondir"),
            format!("{}\n", outside.join(".git").display()),
        )
        .unwrap();
        std::fs::remove_dir_all(original.join(".git")).unwrap();
        std::fs::write(original.join(".git"), "gitdir: private\n").unwrap();
        assert_eq!(fixture.git(&original, &["show", "HEAD:value"]), "borrowed");
        assert_eq!(
            fixture
                .read(&original, std::slice::from_ref(&original))
                .unwrap_err()
                .code,
            codes::DENIED
        );
    }

    #[test]
    fn gitdir_inside_an_approved_root_is_allowed() {
        let fixture = Fixture::new();
        let original = fixture.create_repo("approved", "approved");
        std::fs::rename(original.join(".git"), original.join("metadata")).unwrap();
        std::fs::write(original.join(".git"), "gitdir: metadata\n").unwrap();
        assert_eq!(
            fixture
                .read(&original, std::slice::from_ref(&original))
                .unwrap(),
            json!("approved")
        );
        let other = fixture.create_repo("other", "other-approved");
        std::fs::write(
            original.join(".git"),
            format!("gitdir: {}\n", other.join(".git").display()),
        )
        .unwrap();
        assert_eq!(
            fixture.read(&original, &[original.clone(), other]).unwrap(),
            json!("other-approved")
        );
    }
}
