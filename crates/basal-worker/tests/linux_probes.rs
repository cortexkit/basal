//! Destructive probes use fresh children, raw calls and readiness before the attempted call.
#![cfg(target_os = "linux")]
mod common;

use std::fs;
use std::net::{TcpListener, UdpSocket};
use std::os::unix::{fs::symlink, net::UnixListener, process::ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    root: PathBuf,
    tcp: TcpListener,
    udp: UdpSocket,
    _unix: UnixListener,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "basal-linux-probes-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("outside"), b"outside worker authority").unwrap();
        symlink("outside", root.join("link")).unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let unix = UnixListener::bind(root.join("unix")).unwrap();
        Self {
            root,
            tcp,
            udp,
            _unix: unix,
        }
    }
    fn probe(&self, call: &str, extra: &[&str]) -> Output {
        let path = self.root.join(match call {
            "create" => "created",
            "readlink" => "link",
            "unix-connect" => "unix",
            _ => "outside",
        });
        let port = if call == "udp-send" {
            self.udp.local_addr().unwrap().port()
        } else {
            self.tcp.local_addr().unwrap().port()
        };
        Command::new(common::worker_binary())
            .args(["--confinement-probe", "--landlock=required"])
            .arg(format!("--syscall={call}"))
            .arg(format!("--path={}", path.display()))
            .arg(format!("--port={port}"))
            .arg(format!("--pid={}", std::process::id()))
            .args(extra)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn readiness(out: &Output, call: &str) {
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines()
            .any(|line| line.starts_with(&format!("ready: {call} syscall: "))),
        "missing attempted-call readiness for {call}: {out:?}"
    );
    assert_ne!(
        out.status.code(),
        Some(70),
        "startup/setup refusal is not a denial: {out:?}"
    );
}
fn result(out: &Output) -> (i64, i32) {
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("result: "))
        .unwrap_or_else(|| panic!("missing raw-call result: {out:?}"));
    let (rc, errno) = line.split_once(" errno: ").unwrap();
    (rc.parse().unwrap(), errno.parse().unwrap())
}
fn landlock_report(out: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(report["confinement"], "linux", "{out:?}");
    assert_eq!(report["seccomp"], false, "Landlock-only probe: {out:?}");
    let runtime = report["landlock"]["runtime_abi"].as_u64().unwrap();
    let applied = report["landlock"]["applied_abi"].as_u64().unwrap();
    assert!(
        applied >= 4,
        "landlock-tcp-scoping-unavailable: applied_abi={applied}, runtime_abi={runtime}; ABI 4 required"
    );
    assert_eq!(applied, runtime.min(basal_proto::LANDLOCK_ABI as u64));
    eprintln!("Landlock readback: {}", report["landlock"]);
    report
}

macro_rules! production_probe {
    ($name:ident, $call:literal) => {
        #[test]
        fn $name() {
            let fixture = Fixture::new();
            for handler in [false, true] {
                let args: &[&str] = if handler { &["--sigsys-handler"] } else { &[] };
                let out = fixture.probe($call, args);
                readiness(&out, $call);
                let text = String::from_utf8_lossy(&out.stdout);
                let report: serde_json::Value =
                    serde_json::from_str(text.lines().next().unwrap()).unwrap();
                assert_eq!(report["sigsys_handler"], handler, "{out:?}");
                assert_eq!(out.status.signal(), Some(libc::SIGSYS), "{out:?}");
                assert!(
                    !String::from_utf8_lossy(&out.stdout).contains("result:"),
                    "syscall returned instead of dying at entry: {out:?}"
                );
            }
            // The raw kill probe targets this test process; confirm that pid still exists.
            assert_eq!(unsafe { libc::kill(std::process::id() as _, 0) }, 0);
        }
    };
}
production_probe!(production_open_dies_with_sigsys, "open");
production_probe!(production_create_dies_with_sigsys, "create");
production_probe!(production_stat_dies_with_sigsys, "stat");
production_probe!(production_list_dies_with_sigsys, "list");
production_probe!(production_readlink_dies_with_sigsys, "readlink");
production_probe!(production_tcp_socket_dies_with_sigsys, "tcp");
production_probe!(production_udp_socket_dies_with_sigsys, "udp");
production_probe!(production_unix_socket_dies_with_sigsys, "unix");
production_probe!(production_netlink_socket_dies_with_sigsys, "netlink");
production_probe!(production_exec_dies_with_sigsys, "exec");
production_probe!(production_clone_dies_with_sigsys, "clone");
production_probe!(production_fork_dies_with_sigsys, "fork");
production_probe!(production_pthread_create_dies_with_sigsys, "pthread-create");
production_probe!(production_signal_parent_dies_with_sigsys, "signal-parent");
production_probe!(production_ptrace_parent_dies_with_sigsys, "ptrace");
production_probe!(
    production_process_vm_readv_parent_dies_with_sigsys,
    "process-vm-readv"
);
production_probe!(production_mmap_exec_dies_with_sigsys, "mmap-exec");
production_probe!(production_mprotect_exec_dies_with_sigsys, "mprotect-exec");
production_probe!(production_mmap_shared_dies_with_sigsys, "mmap-shared");
production_probe!(production_io_uring_dies_with_sigsys, "io-uring");
production_probe!(production_userfaultfd_dies_with_sigsys, "userfaultfd");

macro_rules! positive_control {
    ($name:ident, $call:literal) => {
        #[test]
        fn $name() {
            let fixture = Fixture::new();
            let out = fixture.probe($call, &["--no-sandbox"]);
            readiness(&out, $call);
            assert_eq!(out.status.code(), Some(0), "{out:?}");
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("\"confinement\":\"none\""),
                "{out:?}"
            );
            if $call == "exec" {
                assert!(
                    !String::from_utf8_lossy(&out.stdout).contains("result:"),
                    "execve returned: {out:?}"
                );
            } else {
                let (rc, errno) = result(&out);
                assert!(rc >= 0, "raw call failed with errno {errno}: {out:?}");
            }
            if $call == "create" {
                assert!(fixture.root.join("created").is_file());
            }
        }
    };
}
positive_control!(unconfined_open_control_succeeds, "open");
positive_control!(unconfined_create_control_succeeds, "create");
positive_control!(unconfined_tcp_control_succeeds, "tcp");
positive_control!(unconfined_udp_control_succeeds, "udp");
positive_control!(unconfined_unix_control_succeeds, "unix");
positive_control!(unconfined_loopback_connect_control_succeeds, "connect");
positive_control!(unconfined_exec_control_succeeds, "exec");
positive_control!(unconfined_clone_control_succeeds, "clone");
positive_control!(unconfined_fork_control_succeeds, "fork");
positive_control!(unconfined_pthread_control_succeeds, "pthread-create");
positive_control!(unconfined_mmap_exec_control_succeeds, "mmap-exec");
positive_control!(unconfined_mprotect_exec_control_succeeds, "mprotect-exec");

#[test]
fn landlock_only_outside_open_is_denied_with_tcp_scoping_abi() {
    let fixture = Fixture::new();
    let out = fixture.probe("open", &["--layers=landlock"]);
    landlock_report(&out);
    readiness(&out, "open");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let (rc, errno) = result(&out);
    assert_eq!(rc, -1, "{out:?}");
    assert!([libc::EACCES, libc::EPERM].contains(&errno), "{out:?}");
}

#[test]
fn landlock_only_live_loopback_connect_is_denied_with_tcp_scoping_abi() {
    let fixture = Fixture::new();
    let control = fixture.probe("connect", &["--no-sandbox"]);
    readiness(&control, "connect");
    assert_eq!(control.status.code(), Some(0), "{control:?}");
    assert_eq!(result(&control).0, 0, "live loopback control: {control:?}");
    let out = fixture.probe("connect", &["--layers=landlock"]);
    landlock_report(&out);
    readiness(&out, "connect");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let (rc, errno) = result(&out);
    assert_eq!(rc, -1, "{out:?}");
    assert!([libc::EACCES, libc::EPERM].contains(&errno), "{out:?}");
}

#[test]
fn landlock_only_records_stat_readlink_udp_and_path_unix_gaps() {
    let fixture = Fixture::new();
    for call in ["stat", "readlink", "udp-send", "unix-connect"] {
        let out = fixture.probe(call, &["--layers=landlock"]);
        landlock_report(&out);
        readiness(&out, call);
        assert_eq!(out.status.code(), Some(0), "{call}: {out:?}");
        let (rc, errno) = result(&out);
        assert!(
            rc >= 0,
            "Landlock gap {call} unexpectedly denied with errno {errno}: {out:?}"
        );
        if call == "udp-send" {
            // Read the datagram, not just a socket-creation result, to prove network access.
            fixture.udp.set_nonblocking(true).unwrap();
            let mut bytes = [0u8; 16];
            let (len, _) = fixture
                .udp
                .recv_from(&mut bytes)
                .expect("gap datagram arrived");
            assert_eq!(&bytes[..len], b"probe");
        } else if call == "unix-connect" {
            fixture._unix.set_nonblocking(true).unwrap();
            fixture
                ._unix
                .accept()
                .expect("gap path-based Unix connection arrived");
        }
        eprintln!("documented Landlock gap: {call} succeeded (result={rc})");
    }
}
