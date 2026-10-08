//! Live startup refusals use the production entry path, before any stdin read.
//!
//! Normally the supervisor reads the engine's /proc/<pid>/status without sending stdin.
//! A procfs hidepid mount hides non-dumpable children, preventing that read. Such
//! runners log a fallback: the worker reads its own status before Landlock/seccomp,
//! then reports checked installation, and a raw forbidden call must die with SIGSYS.
//! Set BASAL_REQUIRE_VISIBLE_PROC=1 to reject the fallback and require supervisor
//! status readback plus EACCES from /proc/<pid>/mem.
#![cfg(target_os = "linux")]
mod common;

use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule};
use std::fs::File;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Output, Stdio};

fn command(args: &[&str]) -> Command {
    let mut cmd = Command::new(common::worker_binary());
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}
fn refusal(output: Output, token: &str) {
    assert_eq!(
        output.status.code(),
        Some(70),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.split_whitespace().any(|s| s == token),
        "expected {token}: {text}"
    );
    assert!(output.stdout.is_empty(), "readiness/report before refusal");
}
macro_rules! fixture_test {
    ($name:ident, $arg:expr, $token:expr) => {
        #[test]
        fn $name() {
            refusal(
                command(&["--confinement-probe", $arg]).output().unwrap(),
                $token,
            );
        }
    };
}
fixture_test!(
    refuses_proc_missing,
    "--fixture-proc=missing",
    "proc-missing"
);
fixture_test!(refuses_proc_untrusted, "--fixture-proc=0", "proc-untrusted");
fixture_test!(
    refuses_not_one_thread,
    "--fixture-tasks=2",
    "not-one-thread"
);
fixture_test!(
    refuses_mapping_wx,
    "--fixture-maps=0-1000 rwxp 0 00:00 0",
    "mapping-wx"
);
fixture_test!(
    refuses_mapping_exec_stack,
    "--fixture-maps=0-1000 r-xp 0 00:00 0 [stack]",
    "mapping-exec-stack"
);
fixture_test!(
    refuses_mapping_shared,
    "--fixture-maps=0-1000 r--s 0 00:00 0",
    "mapping-shared"
);

#[test]
fn refuses_landlock_argument_missing() {
    refusal(command(&[]).output().unwrap(), "landlock-argument-missing");
}
#[test]
fn engine_arguments_are_closed_and_probe_options_need_probe_mode() {
    for args in [
        vec!["--no-sandbox"],
        vec!["--landlock=invalid"],
        vec!["--landlock=required", "--bogus"],
        vec!["--fixture-tasks=2"],
        vec!["--layers=landlock"],
        vec!["--attest-descriptors"],
    ] {
        let out = command(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(64), "{args:?}: {out:?}");
    }
}
#[test]
fn refuses_stdio_not_pipe_regular_file_and_named_fifo() {
    let dir = basal_testkit::harness::scratch("linux-startup-stdio");
    let regular = dir.join("regular");
    std::fs::write(&regular, b"not a pipe").unwrap();
    let fifo = dir.join("fifo");
    let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let fifo_fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .unwrap();
    for input in [File::open(&regular).unwrap(), fifo_fd] {
        let mut cmd = command(&["--landlock=required"]);
        cmd.stdin(Stdio::from(input));
        refusal(cmd.output().unwrap(), "stdio-not-pipe");
    }
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn refuses_stdio_wrong_mode() {
    let mut cmd = command(&["--landlock=required"]);
    unsafe {
        cmd.pre_exec(|| {
            let mut fds = [0; 2];
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(fds[1], 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(fds[0]);
            libc::close(fds[1]);
            Ok(())
        });
    }
    refusal(cmd.output().unwrap(), "stdio-wrong-mode");
}
#[test]
fn refuses_read_implies_exec() {
    // Linux ELF loading can clear the pre-exec bit for a non-executable stack.
    // Induce it after exec, before the same production entry and personality decision.
    refusal(
        command(&["--confinement-probe", "--induce-read-implies-exec"])
            .output()
            .unwrap(),
        "read-implies-exec",
    );
}
fn inject(cmd: &mut Command, syscall: i64, errno: i32) {
    inject_rules(cmd, syscall, errno, vec![]);
}
fn inject_rules(cmd: &mut Command, syscall: i64, errno: i32, rules: Vec<SeccompRule>) {
    let program: BpfProgram = SeccompFilter::new(
        [(syscall, rules)].into_iter().collect(),
        SeccompAction::Allow,
        SeccompAction::Errno(errno as u32),
        std::env::consts::ARCH.try_into().unwrap(),
    )
    .unwrap()
    .try_into()
    .unwrap();
    unsafe {
        cmd.pre_exec(move || {
            seccompiler::apply_filter(&program).map_err(|e| std::io::Error::other(e.to_string()))
        });
    }
}
macro_rules! injection_test {
    ($name:ident, $syscall:expr, $errno:expr, $token:expr) => {
        #[test]
        fn $name() {
            let mut cmd = command(&["--landlock=required"]);
            inject(&mut cmd, $syscall, $errno);
            refusal(cmd.output().unwrap(), $token);
        }
    };
}
injection_test!(
    refuses_landlock_unavailable_enosys,
    libc::SYS_landlock_create_ruleset,
    libc::ENOSYS,
    "landlock-unavailable-enosys"
);
injection_test!(
    refuses_landlock_unavailable_eopnotsupp,
    libc::SYS_landlock_create_ruleset,
    libc::EOPNOTSUPP,
    "landlock-unavailable-eopnotsupp"
);
injection_test!(
    refuses_descriptor_close_failed,
    libc::SYS_close_range,
    libc::EACCES,
    "descriptor-close-failed"
);
injection_test!(
    refuses_seccomp_install_failed,
    libc::SYS_seccomp,
    libc::EACCES,
    "seccomp-install-failed"
);
injection_test!(
    refuses_landlock_install_failed,
    libc::SYS_landlock_create_ruleset,
    libc::EACCES,
    "landlock-install-failed"
);
injection_test!(
    refuses_no_new_privs_failed,
    libc::SYS_prctl,
    libc::EACCES,
    "no-new-privs-failed"
);
#[test]
fn refuses_dumpable_failed() {
    use seccompiler::{SeccompCmpArgLen, SeccompCmpOp, SeccompCondition};
    let mut cmd = command(&["--landlock=required"]);
    inject_rules(
        &mut cmd,
        libc::SYS_prctl,
        libc::EACCES,
        vec![
            SeccompRule::new(vec![
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Qword,
                    SeccompCmpOp::Eq,
                    libc::PR_SET_DUMPABLE as u64,
                )
                .unwrap(),
            ])
            .unwrap(),
        ],
    );
    refusal(cmd.output().unwrap(), "dumpable-failed");
}
#[test]
fn optional_mode_is_only_used_for_injected_unavailable_landlock() {
    use basal_proto::*;
    for errno in [libc::ENOSYS, libc::EOPNOTSUPP] {
        let mut cmd = command(&["--landlock=optional"]);
        inject(&mut cmd, libc::SYS_landlock_create_ruleset, errno);
        let mut child = cmd.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        write_parent_message(
            &mut input,
            &ParentMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let message = read_worker_message(&mut child.stdout.take().unwrap()).unwrap();
        assert!(
            matches!(
                message,
                WorkerMessage::Welcome(Welcome {
                    confinement: Confinement::Linux {
                        seccomp: true,
                        landlock: None
                    },
                    ..
                })
            ),
            "{message:?}"
        );
        write_parent_message(&mut input, &ParentMessage::Shutdown).unwrap();
        drop(input);
        assert!(child.wait().unwrap().success());
    }
}

#[test]
fn seccomp_kills_raw_open_even_with_a_sigsys_handler() {
    for handler in [false, true] {
        let mut args = vec!["--confinement-probe", "--syscall=open"];
        if handler {
            args.push("--sigsys-handler");
        }
        let out = command(&args).output().unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("ready: open"),
            "{out:?}"
        );
        assert_eq!(out.status.signal(), Some(libc::SIGSYS), "{out:?}");
    }
    let out = command(&["--confinement-probe", "--no-sandbox", "--syscall=open"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("ready: open"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("result: -1"));
}

#[test]
fn landlock_only_denies_raw_open_and_reports_its_applied_abi() {
    let out = command(&["--confinement-probe", "--layers=landlock", "--syscall=open"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("ready: open"), "{text}");
    assert!(text.contains("applied_abi"), "{text}");
    assert!(
        text.contains("result: -1 errno: 13") || text.contains("result: -1 errno: 1"),
        "{text}"
    );
}

fn plant_extra_fds(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
            if fd < 0 || libc::dup2(fd, 64) < 0 || libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if fd != 3 && fd != 64 {
                libc::close(fd);
            }
            Ok(())
        });
    }
}

#[test]
fn descriptor_inventory_alone_reports_only_stdio_after_inherited_close() {
    for legacy in [false, true] {
        let mut args = vec!["--confinement-probe"];
        if legacy {
            args.push("--legacy-close");
        }
        let mut cmd = command(&args);
        plant_extra_fds(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["open_descriptors"], serde_json::json!([0, 1, 2]));
    }
}

#[test]
fn descriptor_inventory_and_independent_sweep_revoke_planted_fds() {
    for legacy in [false, true] {
        let mut args = vec!["--confinement-probe", "--attest-descriptors"];
        if legacy {
            args.push("--legacy-close");
        }
        let mut cmd = command(&args);
        plant_extra_fds(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("\"open_descriptors\":[0, 1, 2]"),
            "{out:?}"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("descriptor-sweep: EBADF 3..1023"),
            "{out:?}"
        );
    }
}

fn hidden_proc() -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    let option = mounts
        .lines()
        .filter(|line| line.contains(" - proc "))
        .flat_map(|line| line.split_whitespace())
        .flat_map(|field| field.split(','))
        .find(|field| {
            field.starts_with("hidepid=") && *field != "hidepid=0" && *field != "hidepid=off"
        })
        .map(str::to_owned);
    if let Some(option) = &option {
        assert_ne!(
            std::env::var("BASAL_REQUIRE_VISIBLE_PROC").ok().as_deref(),
            Some("1"),
            "exact proc attestation required, but mount has {option}"
        );
    }
    option
}

#[test]
fn engine_is_single_threaded_no_new_privs_and_seccomp_before_first_read() {
    if let Some(option) = hidden_proc() {
        eprintln!(
            "proc attestation fallback ({option}): self-status before Landlock/seccomp plus checked TSYNC report and raw-open SIGSYS proof"
        );
        let out = command(&["--confinement-probe", "--attest-status", "--syscall=open"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("self-status before Landlock/seccomp: NoNewPrivs=1 Threads=1 Dumpable=0"),
            "{out:?}"
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("\"seccomp\":true"),
            "{out:?}"
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("ready: open"),
            "{out:?}"
        );
        assert_eq!(out.status.signal(), Some(libc::SIGSYS), "{out:?}");
        return;
    }
    let inherited = std::fs::read_to_string("/proc/self/status").unwrap();
    let count = |status: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix("Seccomp_filters:\t"))
            .unwrap()
            .parse::<usize>()
            .unwrap()
    };
    let before = count(&inherited);
    let mut child = command(&["--landlock=required"]).spawn().unwrap();
    // The additional filter is installed last, so its count is a readiness condition
    // even when the test runner already inherited a seccomp policy.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        assert!(
            child.try_wait().unwrap().is_none(),
            "worker died before first read"
        );
        let status = std::fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap();
        if count(&status) > before {
            assert!(status.lines().any(|l| l == "NoNewPrivs:\t1"), "{status}");
            assert!(status.lines().any(|l| l == "Seccomp:\t2"), "{status}");
            assert!(status.lines().any(|l| l == "Threads:\t1"), "{status}");
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("no worker seccomp installation before first read");
        }
        std::thread::yield_now();
    }
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

#[test]
fn dumpable_denies_parent_ptrace_and_proc_mem_with_unconfined_control() {
    let hidden = hidden_proc();
    if let Some(option) = &hidden {
        eprintln!(
            "dumpable attestation fallback ({option}): supervisor ptrace EPERM and invisible proc mem ENOENT, with live unconfined control"
        );
    }
    for confined in [false, true] {
        let mut args = vec!["--confinement-probe"];
        if !confined {
            args.push("--no-sandbox");
        }
        // Hold the probe at a raw stdin read after its installation report.
        args.push("--wait");
        let mut child = command(&args).spawn().unwrap();
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        use std::io::BufRead;
        reader.read_line(&mut line).unwrap();
        assert!(
            line.contains("open_descriptors"),
            "missing confinement readiness: {line}"
        );
        let pid = child.id() as i32;
        let rc = unsafe { libc::ptrace(libc::PTRACE_ATTACH, pid, 0, 0) };
        if confined {
            assert_eq!(rc, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
            assert_eq!(
                File::open(format!("/proc/{pid}/mem"))
                    .unwrap_err()
                    .raw_os_error(),
                Some(if hidden.is_some() {
                    libc::ENOENT
                } else {
                    libc::EACCES
                })
            );
        } else {
            assert_eq!(
                rc,
                0,
                "parent attach control: {}",
                std::io::Error::last_os_error()
            );
            unsafe {
                let mut status = 0;
                assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
                assert_eq!(libc::ptrace(libc::PTRACE_DETACH, pid, 0, 0), 0);
            }
            File::open(format!("/proc/{pid}/mem")).expect("unconfined proc mem control");
        }
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
    }
}
