//! Each forbidden file, IPC, process, network or executable-memory operation
//! runs in a fresh worker. Its readiness marker immediately precedes the attempt;
//! a crash before that marker is a harness failure, not evidence of denial.
//! Weaker launcher controls prove the operations reach the same real fixtures.
#![cfg(windows)]

#[path = "windows_common/fixtures.rs"]
mod fixtures;
#[path = "windows_common/threads.rs"]
mod threads;
mod windows_common;

use basal_launch::{ConfinedProcess, Deviation, LaunchOptions, launch};
use basal_proto::FLOW_JOB_COMMIT_BYTES;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::os::windows::io::AsRawHandle;
use std::sync::{Mutex, mpsc};
use std::time::Duration;
use windows_common::Handle;

// Inheritable fixtures must not race another spawn in this test process.
static SPAWNS: Mutex<()> = Mutex::new(());
struct Probe {
    child: ConfinedProcess,
    lines: mpsc::Receiver<String>,
    stderr: String,
}
struct Ended {
    code: u32,
    stderr: String,
    stdout: String,
}
impl Ended {
    fn codes(&self, probe: &str) -> Vec<u32> {
        let prefix = format!("windows-probe-result:{probe}:");
        let line = self
            .stdout
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("{}", self.stdout));
        line.trim()
            .split(',')
            .map(|code| code.parse().expect("numeric result"))
            .collect()
    }
    fn denial(&self, probe: &str, codes: &[u32], fatal: Option<u32>) -> bool {
        if !self
            .stderr
            .lines()
            .any(|line| line == format!("windows-probe-ready:{probe}"))
        {
            return false;
        }
        if let Some(fatal) = fatal {
            return self.code == fatal && self.stdout.is_empty();
        }
        self.code == 0 && self.codes(probe) == codes
    }
}
impl Probe {
    fn start(deviation: Deviation, probe: &str, target: &str, number: usize) -> Self {
        let mut options = LaunchOptions::new(
            windows_common::placed_worker(windows_common::dev_binary(env!(
                "CARGO_BIN_EXE_ck-basal-worker"
            ))),
            FLOW_JOB_COMMIT_BYTES,
        )
        .deviation(deviation)
        .arg("--confinement-probe")
        .arg(format!("--probe={probe}"))
        .arg(format!("--target={target}"))
        .arg(format!("--number={number}"));
        if deviation != Deviation::Full && deviation != Deviation::HandleNotAllowed {
            options = options.arg("--no-sandbox");
        }
        Self::from_child(
            launch(&options).unwrap_or_else(|error| panic!("launch {deviation}: {error}")),
        )
    }
    fn from_child(mut child: ConfinedProcess) -> Self {
        let stderr = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if tx.send(line.expect("stderr read")).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            stderr: String::new(),
        }
    }
    fn marker(&mut self, expected: &str) {
        loop {
            match self.lines.recv_timeout(Duration::from_secs(60)) {
                Ok(line) => {
                    self.stderr.push_str(&line);
                    self.stderr.push('\n');
                    if line == format!("windows-probe-ready:{expected}") {
                        return;
                    }
                }
                Err(error) => panic!(
                    "worker did not reach {expected}: {error}; stderr: {}",
                    self.stderr
                ),
            }
        }
    }
    fn release(&mut self) {
        self.child.stdin.as_mut().unwrap().write_all(b"x").unwrap();
    }
    fn end(mut self) -> Ended {
        drop(self.child.stdin.take());
        loop {
            match self.lines.recv_timeout(Duration::from_secs(60)) {
                Ok(line) => {
                    self.stderr.push_str(&line);
                    self.stderr.push('\n');
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(error) => panic!("worker did not exit: {error}; stderr: {}", self.stderr),
            }
        }
        let code = self.child.wait().unwrap();
        let mut stdout = String::new();
        self.child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        Ended {
            code,
            stderr: self.stderr,
            stdout,
        }
    }
}

fn pair(probe: &str, target: &str, number: usize, denied: u32, control: Deviation) {
    let confined = Probe::start(Deviation::Full, probe, target, number).end();
    assert!(
        confined.denial(probe, &[denied], None),
        "{probe}: exit {:#x}; stderr {}; stdout {}",
        confined.code,
        confined.stderr,
        confined.stdout
    );
    let positive = Probe::start(control, probe, target, number).end();
    assert!(
        positive.denial(probe, &[0], None),
        "{probe} {control}: exit {:#x}; stderr {}; stdout {}",
        positive.code,
        positive.stderr,
        positive.stdout
    );
    println!("{probe}: confined {denied:#x}; {control} succeeds");
}

#[test]
fn system_image_data_read_and_stat_are_denied_with_lpac_controls() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let target = format!(
        "{}\\System32\\kernel32.dll",
        std::env::var("SystemRoot").unwrap()
    );
    for probe in ["read", "stat"] {
        pair(probe, &target, 0, 5, Deviation::LpacOnly);
    }
}
#[test]
fn package_folder_create_is_denied_with_lpac_control() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = fixtures::Package::new();
    let target = fixture.create_path();
    pair("create", &target, 0, 5, Deviation::LpacOnly);
    std::fs::remove_file(target).unwrap();
}
#[test]
fn native_package_hive_write_is_denied_with_lpac_control() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = fixtures::Package::new();
    pair(
        "registry",
        &fixture.hive,
        0,
        0xc0000022,
        Deviation::LpacOnly,
    );
    fixture.remove_value();
}
#[test]
fn new_alpc_epmapper_and_package_section_are_denied_with_lpac_controls() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    pair(
        "alpc",
        "\\RPC Control\\epmapper",
        0,
        0xc0000022,
        Deviation::LpacOnly,
    );
    let fixture = fixtures::Package::new();
    let (_section, name) = fixture.section();
    pair("section", &name, 0, 0xc0000022, Deviation::LpacOnly);
}
#[test]
fn child_creation_and_parent_process_opens_are_denied_with_plain_controls() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let cmd = format!(
        "{}\\System32\\cmd.exe",
        std::env::var("SystemRoot").unwrap()
    );
    pair("process", &cmd, 0, 5, Deviation::Plain);
    for probe in ["parent-query", "parent-read"] {
        pair(probe, "", std::process::id() as usize, 5, Deviation::Plain);
    }
}
#[test]
fn a_real_named_pipe_is_denied_with_plain_control() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let (_pipe, path) = fixtures::pipe();
    pair("pipe", &path, 0, 5, Deviation::Plain);
}
#[test]
fn executable_allocation_and_rw_to_x_are_denied_with_lpac_controls() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    for probe in ["allocate-executable", "protect-executable"] {
        pair(probe, "", 0, 1655, Deviation::LpacOnly);
    }
}
#[test]
fn native_winsock_tcp_udp_and_loopback_are_denied_and_parent_observes_controls() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    for probe in ["tcp", "loopback", "udp"] {
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        tcp.set_nonblocking(true).unwrap();
        udp.set_nonblocking(true).unwrap();
        let port = if probe == "udp" {
            udp.local_addr().unwrap().port()
        } else {
            tcp.local_addr().unwrap().port()
        };
        let confined = Probe::start(Deviation::Full, probe, "", port as usize).end();
        assert!(
            confined.denial(probe, &[10107, 10093], None),
            "{probe}: {} {}",
            confined.stderr,
            confined.stdout
        );
        assert_eq!(
            tcp.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            udp.recv(&mut [0; 16]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let positive = Probe::start(Deviation::Plain, probe, "", port as usize).end();
        assert!(
            positive.denial(probe, &[0, 0, 0], None),
            "{probe}: {} {}",
            positive.stderr,
            positive.stdout
        );
        if probe == "udp" {
            let mut bytes = [0; 16];
            let n = udp.recv(&mut bytes).unwrap();
            assert_eq!(&bytes[..n], b"probe");
        } else {
            drop(
                tcp.accept()
                    .expect("parent accepts plain loopback connection"),
            );
        }
        println!(
            "{probe}: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed"
        );
    }
}
#[test]
fn create_thread_succeeds_inside_full_confinement() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let ended = Probe::start(Deviation::Full, "thread", "", 0).end();
    assert!(
        ended.denial("thread", &[0], None),
        "{} {}",
        ended.stderr,
        ended.stdout
    );
}
#[test]
fn fatal_exit_before_readiness_is_not_a_denial() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let ended = Probe::start(Deviation::Full, "crash-before-ready", "", 0).end();
    assert_eq!(ended.code, 0xc0000008);
    assert!(!ended.denial("crash-before-ready", &[], Some(0xc0000008)));
    let wrong_status = Ended {
        code: 0xc0000005,
        stderr: "windows-probe-ready:excluded-handle\n".into(),
        stdout: String::new(),
    };
    assert!(!wrong_status.denial("excluded-handle", &[], Some(0xc0000008)));
}
#[test]
fn excluded_inheritable_handle_is_fatal_only_after_readiness_and_plain_dropped_list_writes() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = fixtures::Planted::new();
    let number = fixture.file.as_raw_handle() as usize;
    let confined = Probe::start(Deviation::Full, "excluded-handle", "", number).end();
    assert!(
        confined.denial("excluded-handle", &[], Some(0xc0000008)),
        "{} {}",
        confined.stderr,
        confined.stdout
    );
    assert!(fixture.bytes().is_empty());
    let excluded_plain = Probe::start(Deviation::Plain, "excluded-handle", "", number).end();
    assert!(excluded_plain.denial("excluded-handle", &[6], None));
    assert!(fixture.bytes().is_empty());
    let included_plain = Probe::start(
        Deviation::PlainWithoutHandleList,
        "excluded-handle",
        "",
        number,
    )
    .end();
    assert!(
        included_plain.denial("excluded-handle", &[0], None),
        "{} {}",
        included_plain.stderr,
        included_plain.stdout
    );
    assert_eq!(fixture.bytes(), b"escaped");
}
#[test]
fn planted_handle_startup_refuses_and_the_allowlist_mutant_writes_the_fixture() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = fixtures::Planted::new();
    let ended = Probe::start(
        Deviation::HandleNotAllowed,
        "excluded-handle",
        "",
        fixture.file.as_raw_handle() as usize,
    )
    .end();
    assert_eq!(
        ended.code,
        70,
        "{} {}; fixture bytes {:?}",
        ended.stderr,
        ended.stdout,
        fixture.bytes()
    );
    assert!(
        ended
            .stderr
            .contains("refusing to run unconfined: handle-not-allowed:"),
        "{}",
        ended.stderr
    );
    assert!(!ended.stderr.contains("windows-probe-ready:"));
    assert!(fixture.bytes().is_empty());
}
#[test]
fn thread_barrier_checks_every_thread_before_and_during_held_pool_callbacks() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let options = LaunchOptions::new(
        windows_common::placed_worker(windows_common::dev_binary(env!(
            "CARGO_BIN_EXE_ck-basal-worker"
        ))),
        FLOW_JOB_COMMIT_BYTES,
    )
    .arg("--confinement-probe")
    .arg("--thread-barrier");
    let mut probe = Probe::from_child(launch(&options).unwrap());
    probe.marker("thread-barrier");
    threads::assert_all_threads(probe.child.id(), "before-pool");
    probe.release();
    probe.marker("callbacks-held");
    let tids = threads::assert_all_threads(probe.child.id(), "callbacks-held");
    let callbacks: BTreeMap<_, _> = probe
        .stderr
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("windows-pool-callback:")?;
            let (kind, tid) = rest.split_once(':')?;
            Some((kind.to_owned(), tid.parse::<u32>().unwrap()))
        })
        .collect();
    assert_eq!(
        callbacks.keys().map(String::as_str).collect::<Vec<_>>(),
        ["legacy", "timer", "wait", "work"]
    );
    for tid in callbacks.values() {
        assert!(tids.contains(tid), "callback TID {tid} not in snapshot");
    }
    println!("every callback TID is in the snapshot: {callbacks:?}");
    probe.release();
    let ended = probe.end();
    assert!(ended.denial("thread-barrier", &[0], None));
    assert!(
        ended
            .stderr
            .contains("windows-pool-complete:work,wait,timer,legacy")
    );
}
#[test]
fn forced_pool_work_wait_timer_and_legacy_work_complete() {
    let _lock = SPAWNS.lock().unwrap_or_else(|error| error.into_inner());
    let ended = Probe::start(Deviation::Full, "pool", "", 0).end();
    assert!(
        ended.denial("pool", &[0], None),
        "{} {}",
        ended.stderr,
        ended.stdout
    );
    assert!(
        ended
            .stderr
            .contains("windows-pool-complete:work,wait,timer,legacy")
    );
    println!("windows-pool-complete:work,wait,timer,legacy");
}
