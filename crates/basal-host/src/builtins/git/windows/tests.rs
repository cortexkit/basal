use super::*;
use std::sync::Arc;
use windows_sys::Win32::{
    Foundation::STILL_ACTIVE,
    System::{
        JobObjects::QueryInformationJobObject,
        Threading::{
            CREATE_BREAKAWAY_FROM_JOB, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE,
        },
    },
};

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        let mut random = [0u8; 16];
        assert!(
            unsafe {
                BCryptGenRandom(
                    std::ptr::null_mut(),
                    random.as_mut_ptr(),
                    random.len() as u32,
                    BCRYPT_USE_SYSTEM_PREFERRED_RNG,
                )
            } >= 0
        );
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("basal-git-test-{name}"));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn p(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn child_image() -> &'static Path {
    static IMAGE: OnceLock<PathBuf> = OnceLock::new();
    IMAGE.get_or_init(|| {
        let tree = Tree::new();
        let source = tree.p("child.rs");
        std::fs::write(&source, include_str!("../fixtures/windows_child.rs")).unwrap();
        let image = tree.p("argv-echo.exe");
        let output = Command::new("rustc")
            .args(["--edition=2024", "-Dwarnings"])
            .arg(&source)
            .arg("-o")
            .arg(&image)
            .output()
            .expect("native rustc is required");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Keep the compiled child executable available to concurrent tests.
        std::mem::forget(tree);
        image
    })
}
fn child(mode: &str) -> Command {
    let mut command = Command::new(child_image());
    command.arg(mode);
    command
}
fn succeeded(result: Ran) -> Ran {
    assert!(result.success, "{}", result.stderr);
    result
}
fn runner(command: Command, lines: Option<usize>, timeout: Duration) -> Result<Ran, Denial> {
    run_until(command.into(), lines, timeout, Faults::default())
}

#[test]
fn windows_crt_argv_round_trip_preserves_quotes_and_utf16() {
    let arguments = [
        OsString::from(""),
        OsString::from("HEAD:a\"b"),
        OsString::from("a\\\"b"),
        OsString::from("a\\\\\"b"),
        OsString::from("with space\\"),
        OsString::from("\t\n"),
        OsString::from("C:\\résumé\\日本語"),
        OsString::from_wide(&[0xd800, b'"' as u16, b'\\' as u16]),
    ];
    let mut command = child("argv");
    command.args(&arguments);
    let result = succeeded(run_command(command).unwrap());
    let actual: Vec<Vec<u16>> = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            line.as_bytes()
                .chunks(4)
                .map(|unit| u16::from_str_radix(std::str::from_utf8(unit).unwrap(), 16).unwrap())
                .collect()
        })
        .collect();
    let expected: Vec<Vec<u16>> = arguments
        .iter()
        .map(|arg| arg.encode_wide().collect())
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn windows_drive_paths_refuse_unc_and_preserve_utf16() {
    for path in [
        r"\\?\UNC\server\share\repo",
        r"\\?\Volume{abc}\repo",
        r"\\.\C:\repo",
        r"\\server\share\repo",
        r"C:repo",
        r"\repo",
    ] {
        assert!(drive_path(Path::new(path)).is_err(), "accepted {path}");
    }
    let input = OsString::from_wide(&[92, 92, 63, 92, 67, 58, 92, 0xd800]);
    assert_eq!(
        drive_path(Path::new(&input))
            .unwrap()
            .as_os_str()
            .encode_wide()
            .collect::<Vec<_>>(),
        [67, 58, 92, 0xd800]
    );
}

#[test]
fn windows_git_resolution_ignores_relative_path_and_cwd_images() {
    let tree = Tree::new();
    std::fs::write(tree.p("git.exe"), b"MZ fixture, never executed").unwrap();
    let paths = std::env::join_paths([
        Path::new(""),
        Path::new("relative-bin"),
        Path::new(r"C:bin"),
        Path::new(r"\bin"),
        &tree.0,
    ])
    .unwrap();
    let resolved = resolve_git_binary(OsStr::new("git"), Some(&paths)).unwrap();
    assert!(resolved.is_absolute());
    assert_eq!(
        resolved,
        drive_path(&std::fs::canonicalize(tree.p("git.exe")).unwrap()).unwrap()
    );
    let only_relative = OsStr::new(";relative-bin;C:bin;\\bin");
    assert!(resolve_git_binary(OsStr::new("git"), Some(only_relative)).is_err());
    assert!(resolve_git_binary(OsStr::new("relative-bin\\git.exe"), None).is_err());
    for extension in ["bat", "CMD"] {
        assert!(resolve_git_binary(tree.p(&format!("git.{extension}")).as_os_str(), None).is_err());
    }
    let image = tree.p("git.exe");
    std::fs::remove_file(&image).unwrap();
    std::fs::write(tree.p("git.cmd"), "exit /b 0").unwrap();
    assert!(resolve_git_binary(OsStr::new("git"), Some(&paths)).is_err());
}

#[test]
fn windows_explicit_application_is_independent_of_current_directory() {
    let tree = Tree::new();
    // A script and a fake executable in the CWD must not shadow the resolved image.
    std::fs::write(tree.p("argv-echo.exe"), "not an executable").unwrap();
    std::fs::write(tree.p("git.exe"), "not an executable").unwrap();
    let mut command = child("argv");
    command.current_dir(&tree.0).arg("stable image");
    assert_eq!(
        succeeded(run_command(command).unwrap()).stdout,
        b"0073007400610062006c006500200069006d006100670065\n"
    );
    let marker = tree.p("application-marker");
    let mut command = child("mark");
    command.arg(&marker).current_dir(&tree.0);
    let input = command.into();
    // Deliberately misleading argv[0] distinguishes explicit application selection
    // from CreateProcess's fallback command-line executable search.
    let wrong_image = tree.p("git.exe");
    let (guard, _, _) =
        spawn_with_argv0(&input, |_| Ok(()), Some(wrong_image.as_os_str())).unwrap();
    assert_eq!(
        unsafe { WaitForSingleObject(guard.process.0, 5000) },
        WAIT_OBJECT_0
    );
    assert!(marker.exists());
}

#[test]
fn windows_private_isolation_is_fresh_exclusive_and_pinned() {
    let first = Isolation::new().unwrap();
    let second = Isolation::new().unwrap();
    assert_ne!(first.directory, second.directory);
    let config = first.directory.join("global.config");
    assert_eq!(std::fs::metadata(&config).unwrap().len(), 0);
    assert!(std::fs::write(&config, "[alias]\npwn = !whoami\n").is_err());
    assert!(std::fs::rename(&config, first.directory.join("swapped.config")).is_err());
    assert!(std::fs::rename(&first.directory, first.directory.with_extension("swap")).is_err());
    let hooks = first.directory.join("hooks");
    let config_path = config.clone();
    let hooks_path = hooks.clone();
    thread::spawn(move || {
        for _ in 0..20 {
            assert!(std::fs::write(&config_path, "[alias]\npwn = !whoami\n").is_err());
            assert!(std::fs::rename(&hooks_path, hooks_path.with_extension("swap")).is_err());
        }
    })
    .join()
    .unwrap();
    let old_predictable =
        std::env::temp_dir().join(format!("basal-git-isolation-{}", std::process::id()));
    // The obsolete name is untrusted; no production helper consults it.
    std::fs::create_dir_all(&old_predictable).unwrap();
    std::fs::write(
        old_predictable.join("empty.config"),
        "[alias]\npwn = !whoami\n",
    )
    .unwrap();
    let third = Isolation::new().unwrap();
    assert_ne!(third.directory, old_predictable);
    assert_eq!(
        std::fs::metadata(third.directory.join("global.config"))
            .unwrap()
            .len(),
        0
    );
    std::fs::remove_dir_all(old_predictable).unwrap();
    let directory = first.directory.clone();
    drop(first);
    assert!(!directory.exists());
    assert_eq!(
        succeeded(run_command(child("stdin")).unwrap()).stdout,
        b"inert\n"
    );
}

fn private_dacl(path: &Path) -> String {
    use windows_sys::Win32::Security::{
        Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
            SE_FILE_OBJECT,
        },
        DACL_SECURITY_INFORMATION,
    };
    let mut descriptor = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            GetNamedSecurityInfoW(
                wide(path).unwrap().as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        },
        0
    );
    let descriptor = SecurityDescriptor(descriptor);
    let mut sddl = std::ptr::null_mut();
    let mut length = 0;
    assert_ne!(
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0,
                1,
                DACL_SECURITY_INFORMATION,
                &mut sddl,
                &mut length,
            )
        },
        0
    );
    let text = unsafe {
        String::from_utf16(std::slice::from_raw_parts(sddl, length as usize - 1)).unwrap()
    };
    unsafe {
        LocalFree(sddl.cast());
    }
    text
}

#[test]
fn windows_isolation_trims_host_temp_separator_and_has_private_dacl() {
    let tree = Tree::new();
    let mut base = tree.0.as_os_str().to_owned();
    base.push("\\");
    let isolation = Isolation::in_base(Path::new(&base)).unwrap();
    assert_eq!(isolation.directory.parent(), Some(tree.0.as_path()));
    for path in [
        &isolation.directory,
        &isolation.directory.join("hooks"),
        &isolation.directory.join("global.config"),
    ] {
        let sddl = private_dacl(path);
        // Both explicit and inherited ACLs must grant only SYSTEM and the owner.
        assert!(sddl.starts_with("D:P"), "DACL is not protected: {sddl}");
        let trustees: Vec<&str> = sddl
            .split(";;;")
            .skip(1)
            .map(|ace| ace.split(')').next().unwrap())
            .collect();
        assert_eq!(trustees.len(), 2, "unexpected ACL entries: {sddl}");
        assert!(
            trustees.contains(&"SY") && trustees.contains(&"OW"),
            "not private: {sddl}"
        );
    }
    let link = tree.p("temp-junction");
    junction(&link, &tree.0);
    assert!(
        Isolation::in_base(&link).is_err(),
        "temp junction was accepted"
    );
    std::fs::remove_dir(link).unwrap();
}

#[test]
fn windows_attribute_payloads_and_job_membership_before_work() {
    let tree = Tree::new();
    let marker = tree.p("work");
    let mut command = child("mark");
    command.arg(&marker);
    let input: CommandInput = command.into();
    let (mut guard, _, _) = spawn_job_command(&input, |guard| {
        assert!(!marker.exists(), "child ran before membership verification");
        let job = guard.job.as_ref().unwrap().0;
        let mut in_job = 0;
        assert_ne!(
            unsafe { IsProcessInJob(guard.process.0, job, &mut in_job) },
            0
        );
        assert_ne!(in_job, 0);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        assert_ne!(
            unsafe {
                QueryInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        assert_eq!(
            limits.BasicLimitInformation.LimitFlags,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        );
        // Moving an attribute owner must not move either retained payload.
        let inherit = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let (_read_a, write_a) = pipe(&inherit).unwrap();
        let (_read_b, write_b) = pipe(&inherit).unwrap();
        let (_read_c, write_c) = pipe(&inherit).unwrap();
        let attributes = AttributeList::new(job, [write_a.0, write_b.0, write_c.0]).unwrap();
        let jobs = attributes.jobs.as_ptr();
        let handles = attributes.handles.as_ptr();
        let moved = Box::new(attributes);
        assert_eq!(moved.jobs.as_ptr(), jobs);
        assert_eq!(moved.handles.as_ptr(), handles);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        unsafe { WaitForSingleObject(guard.process.0, 5000) },
        WAIT_OBJECT_0
    );
    assert!(marker.exists());
    guard.stop_tree();
    let mut command = child("breakaway");
    command
        .arg(tree.p("escaped"))
        .arg(CREATE_BREAKAWAY_FROM_JOB.to_string());
    succeeded(run_command(command).unwrap());
    assert!(!tree.p("escaped").exists());
}

fn await_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "child did not create {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn descendant(pid_path: &Path) -> OwnedHandle {
    await_file(pid_path);
    let pid: u32 = std::fs::read_to_string(pid_path).unwrap().parse().unwrap();
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    assert!(
        !handle.is_null(),
        "cannot retain live descendant: {}",
        std::io::Error::last_os_error()
    );
    let handle = OwnedHandle(handle);
    assert_eq!(unsafe { WaitForSingleObject(handle.0, 0) }, WAIT_TIMEOUT);
    let mut code = 0;
    assert_ne!(unsafe { GetExitCodeProcess(handle.0, &mut code) }, 0);
    assert_eq!(code, STILL_ACTIVE as u32);
    handle
}
fn require_dead(handle: &OwnedHandle) {
    assert_eq!(
        unsafe { WaitForSingleObject(handle.0, 5000) },
        WAIT_OBJECT_0,
        "descendant survived cleanup"
    );
    let mut code = 0;
    assert_ne!(unsafe { GetExitCodeProcess(handle.0, &mut code) }, 0);
    assert_ne!(code, STILL_ACTIVE as u32);
}

#[test]
fn windows_timeout_proves_live_descendant_died() {
    let tree = Arc::new(Tree::new());
    let worker_tree = tree.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut command = child("spawn");
        command
            .arg(worker_tree.p("pid"))
            .arg(worker_tree.p("ready"));
        tx.send(runner(command, None, Duration::from_secs(3)).map(|_| ()))
            .unwrap();
    });
    let handle = descendant(&tree.p("pid"));
    let error = rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, codes::TIMEOUT);
    require_dead(&handle);
}

#[test]
fn windows_post_exit_kills_descendant_holding_both_pipes() {
    let tree = Arc::new(Tree::new());
    let worker_tree = tree.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut command = child("post-exit");
        command
            .arg(worker_tree.p("pid"))
            .arg(worker_tree.p("ready"));
        tx.send(run_command(command).map(|_| ())).unwrap();
    });
    let handle = descendant(&tree.p("pid"));
    await_file(&tree.p("ready"));
    std::fs::write(tree.p("ready.release"), "release").unwrap();
    rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    require_dead(&handle);
}

#[test]
fn windows_tags_stop_while_reading_and_enforce_byte_cap() {
    let mut command = child("lines");
    command.arg("80");
    let result = succeeded(runner(command, Some(MAX_TAGS), Duration::from_secs(3)).unwrap());
    assert_eq!(
        result.stdout.iter().filter(|b| **b == b'\n').count(),
        MAX_TAGS
    );
    let mut command = child("lines");
    command.arg((MAX_OUTPUT_BYTES / (MAX_TAGS - 1) + 1).to_string());
    assert_eq!(
        runner(command, Some(MAX_TAGS), Duration::from_secs(3))
            .err()
            .unwrap()
            .code,
        codes::TOO_LARGE
    );
}

#[test]
fn windows_failed_termination_is_retryable_and_scope_unwind_is_bounded() {
    let tree = Arc::new(Tree::new());
    let worker_tree = tree.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut command = child("hold");
        command.arg(worker_tree.p("pid"));
        let input = command.into();
        let (mut guard, _, _) = spawn_job_command(&input, |_| Ok(())).unwrap();
        guard.fail_termination_once = true;
        assert!(guard.terminate().is_err());
        assert!(!guard.terminated);
        guard.terminate().unwrap();
        assert!(guard.terminated);
        assert_eq!(
            unsafe { WaitForSingleObject(guard.process.0, 5000) },
            WAIT_OBJECT_0
        );
        let mut command = child("hold");
        command.arg(worker_tree.p("panic-pid"));
        let panicked = std::panic::catch_unwind(|| {
            run_until(
                command.into(),
                None,
                Duration::from_secs(2),
                Faults {
                    fail_termination_once: true,
                    panic_after_waiter: true,
                },
            )
        });
        assert!(panicked.is_err());
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("scope cleanup hung after startup failure");
}

#[test]
fn windows_only_stdio_handles_are_inherited() {
    let inherit = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (read, write) = pipe(&inherit).unwrap();
    let tree = Tree::new();
    let mut command = child("hold");
    command.arg(tree.p("pid"));
    let input = command.into();
    let (mut guard, _, _) = spawn_job_command(&input, |_| Ok(())).unwrap();
    await_file(&tree.p("pid"));
    drop(write);
    let mut available = 0;
    assert_eq!(
        unsafe {
            PeekNamedPipe(
                read.0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        },
        0
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(ERROR_BROKEN_PIPE as i32),
        "unrelated inheritable writer leaked to child"
    );
    guard.stop_tree();
}

fn repository(tree: &Tree) -> PathBuf {
    let repo = tree.p("repo");
    std::fs::create_dir(&repo).unwrap();
    setup_git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("file.txt"), "one\n").unwrap();
    setup_git(&repo, &["add", "."]);
    setup_git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "-qm",
            "first",
        ],
    );
    std::fs::write(repo.join("file.txt"), "two\n").unwrap();
    setup_git(&repo, &["add", "."]);
    setup_git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "-qm",
            "second",
        ],
    );
    repo
}
fn plain_git(repo: &Path) -> Command {
    let mut command = Command::new(
        resolve_git().expect("native git.exe is required for Windows confinement tests"),
    );
    command
        .arg("-C")
        .arg(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "NUL");
    command
}
fn setup_git(repo: &Path, args: &[&str]) {
    let output = plain_git(repo).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "setup git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn junction(link: &Path, target: &Path) {
    let output = Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "junction creation failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn windows_repository_walk_refuses_junctions_and_pins_all_ancestors() {
    let tree = Tree::new();
    let parent = tree.p("parent");
    std::fs::create_dir(&parent).unwrap();
    let repo = parent.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let approved = [repo.to_str().unwrap().to_owned()];
    // Positive control: the same names really can be replaced without a pin.
    let moved = tree.p("moved");
    std::fs::rename(&parent, &moved).unwrap();
    std::fs::rename(&moved, &parent).unwrap();
    let pin = super::super::repo(repo.to_str().unwrap(), &approved).unwrap();
    assert!(
        std::fs::rename(&parent, &moved).is_err(),
        "ancestor replaced while git is authorized"
    );
    assert!(
        std::fs::rename(&repo, parent.join("old-repo")).is_err(),
        "repo replaced while git is authorized"
    );
    let mut command = child("argv");
    command.current_dir(pin.path()).arg("pinned");
    succeeded(run_command(command).unwrap());
    drop(pin);
    std::fs::rename(&parent, &moved).unwrap();
    junction(&parent, &moved);
    assert!(super::super::repo(repo.to_str().unwrap(), &approved).is_err());
    std::fs::remove_dir(&parent).unwrap();
    junction(&tree.p("root-link"), &moved.join("repo"));
    let link = tree.p("root-link").to_str().unwrap().to_owned();
    assert!(super::super::repo(&link, std::slice::from_ref(&link)).is_err());
    std::fs::remove_dir(tree.p("root-link")).unwrap();
    let sub = moved.join("repo").join("sub");
    std::fs::create_dir(&sub).unwrap();
    assert!(
        super::super::repo(
            sub.to_str().unwrap(),
            &[moved.join("repo").to_str().unwrap().to_owned()]
        )
        .is_err()
    );
}

fn marker_script(tree: &Tree, name: &str) -> (PathBuf, PathBuf) {
    let marker = tree.p(&format!("{name}.marker"));
    let script = tree.p(&format!("{name}.sh"));
    let marker_shell = marker
        .to_str()
        .unwrap()
        .replace('\\', "/")
        .replace('\'', "'\\''");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf ran >> '{marker_shell}'\nprintf 'converted\\n'\n"),
    )
    .unwrap();
    (script, marker)
}
fn helper_value(script: &Path) -> String {
    format!(
        "sh '{}'",
        script
            .to_str()
            .unwrap()
            .replace('\\', "/")
            .replace('\'', "'\\''")
    )
}
fn hard_git(repo: &Path, args: &[&str]) -> Ran {
    let mut command = hardened_command(repo).unwrap();
    command.args(args);
    run_command(command).unwrap()
}
fn positive_marker(repo: &Path, args: &[&str], marker: &Path) {
    let mut command = plain_git(repo);
    command.args(args);
    succeeded(run_command(command).unwrap());
    assert!(
        marker.exists(),
        "positive control did not execute the planted helper: {args:?}"
    );
    std::fs::remove_file(marker).unwrap();
}

#[test]
fn windows_hook_positive_control_and_hardened_suppression() {
    let tree = Tree::new();
    let repo = repository(&tree);
    let (script, marker) = marker_script(&tree, "hook");
    // Hooks have no .bat suffix: Git for Windows executes the shebang file.
    std::fs::copy(
        script,
        repo.join(".git").join("hooks").join("post-checkout"),
    )
    .unwrap();
    positive_marker(&repo, &["checkout", "--detach", "HEAD~"], &marker);
    succeeded(hard_git(&repo, &["checkout", "--detach", "HEAD"]));
    assert!(!marker.exists(), "hook ran through hardened hooksPath");
}

#[test]
fn windows_fsmonitor_positive_control_and_hardened_suppression() {
    let tree = Tree::new();
    let repo = repository(&tree);
    let (script, marker) = marker_script(&tree, "fsmonitor");
    setup_git(&repo, &["config", "core.fsmonitor", &helper_value(&script)]);
    positive_marker(&repo, &["status", "--porcelain"], &marker);
    succeeded(hard_git(&repo, &["status", "--porcelain"]));
    assert!(!marker.exists(), "fsmonitor ran through hardened command");
}

#[test]
fn windows_external_diff_positive_control_and_builtin_suppression() {
    let tree = Tree::new();
    let repo = repository(&tree);
    let (script, marker) = marker_script(&tree, "external-diff");
    setup_git(&repo, &["config", "diff.external", &helper_value(&script)]);
    positive_marker(&repo, &["diff", "HEAD~", "HEAD", "--", "file.txt"], &marker);
    let path = repo.to_str().unwrap().to_owned();
    let op = super::super::Op::Diff {
        from: "HEAD~".into(),
        to: "HEAD".into(),
        path: Some("file.txt".into()),
    };
    super::super::run(&path, std::slice::from_ref(&path), &op).unwrap();
    assert!(!marker.exists(), "external diff ran through builtin diff");
}

#[test]
fn windows_textconv_positive_control_and_builtin_suppression() {
    let tree = Tree::new();
    let repo = repository(&tree);
    let (script, marker) = marker_script(&tree, "textconv");
    std::fs::write(repo.join(".gitattributes"), "file.txt diff=marker\n").unwrap();
    setup_git(
        &repo,
        &["config", "diff.marker.textconv", &helper_value(&script)],
    );
    positive_marker(&repo, &["diff", "HEAD~", "HEAD", "--", "file.txt"], &marker);
    let path = repo.to_str().unwrap().to_owned();
    let op = super::super::Op::Diff {
        from: "HEAD~".into(),
        to: "HEAD".into(),
        path: Some("file.txt".into()),
    };
    super::super::run(&path, std::slice::from_ref(&path), &op).unwrap();
    assert!(!marker.exists(), "textconv ran through builtin diff");
}

#[test]
fn windows_contaminated_global_config_positive_control_and_isolation() {
    let tree = Tree::new();
    let repo = repository(&tree);
    let (script, marker) = marker_script(&tree, "global");
    let config = tree.p("hostile-global.config");
    let alias = format!("!{}", helper_value(&script));
    let output = Command::new(resolve_git().unwrap())
        .args(["config", "--file"])
        .arg(&config)
        .args(["alias.basal-poison", &alias])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut command = plain_git(&repo);
    command
        .env("GIT_CONFIG_GLOBAL", &config)
        .arg("basal-poison");
    succeeded(run_command(command).unwrap());
    assert!(
        marker.exists(),
        "contaminated global alias positive control did not run"
    );
    std::fs::remove_file(&marker).unwrap();
    // Supply the poison as inherited host environment to the spawn recipe without
    // altering this test process's environment (other tests run concurrently).
    let mut command = hardened_command(&repo).unwrap();
    command.arg("basal-poison");
    let block = environment(&command.command, true).unwrap();
    assert!(!String::from_utf16_lossy(&block).contains(config.to_str().unwrap()));
    let result = run_command(command).unwrap();
    assert!(
        !result.success,
        "global alias survived isolated global config"
    );
    assert!(!marker.exists());
}

#[test]
fn windows_hostile_environment_child() {
    let Some(repo) = std::env::var_os("BASAL_GIT_ENV_REPO") else {
        return;
    };
    let config =
        std::env::var_os("GIT_CONFIG_GLOBAL").expect("parent supplied contaminated global config");
    let mut positive = plain_git(Path::new(&repo));
    positive
        .env_remove("GIT_DIR")
        .env_remove("GIT_CONFIG_COUNT")
        .env("GIT_CONFIG_GLOBAL", &config)
        .arg("basal-poison");
    succeeded(run_command(positive).unwrap());
    let marker = PathBuf::from(std::env::var_os("BASAL_GIT_ENV_MARKER").unwrap());
    assert!(marker.exists());
    std::fs::remove_file(&marker).unwrap();
    let result = hard_git(Path::new(&repo), &["basal-poison"]);
    assert!(!result.success);
    assert!(
        !marker.exists(),
        "inherited global config executed a helper"
    );
    let mut command = hardened_command(Path::new(&repo)).unwrap();
    command.args(["rev-parse", "--show-toplevel"]);
    let result = succeeded(run_command(command).unwrap());
    assert!(
        String::from_utf8(result.stdout).unwrap().contains("repo"),
        "inherited GIT_DIR redirected the repository"
    );
}

#[test]
fn windows_inherited_git_environment_is_not_imported() {
    let tree = Tree::new();
    let repo = repository(&tree);
    let (script, marker) = marker_script(&tree, "inherited-global");
    let config = tree.p("global.config");
    let alias = format!("!{}", helper_value(&script));
    let output = Command::new(resolve_git().unwrap())
        .args(["config", "--file"])
        .arg(&config)
        .args(["alias.basal-poison", &alias])
        .output()
        .unwrap();
    assert!(output.status.success());
    // A subprocess is the real host with hostile inherited settings. No unsafe
    // process-wide environment mutation races this suite's concurrent tests.
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "builtins::git::windows::tests::windows_hostile_environment_child",
            "--nocapture",
        ])
        .env("BASAL_GIT_ENV_REPO", &repo)
        .env("BASAL_GIT_ENV_MARKER", &marker)
        .env("GIT_CONFIG_GLOBAL", &config)
        .env("GIT_DIR", tree.p("not-a-repository"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "alias.basal-poison")
        .env("GIT_CONFIG_VALUE_0", &alias)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child control never executed"
    );
    assert!(!marker.exists());
}
