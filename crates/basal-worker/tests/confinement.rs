//! OS confinement: the worker's own attempts to open a file, open a socket
//! or run a program fail once it has confined itself, and succeed without
//! the sandbox. The probe confines itself through the same function the
//! engine uses before reading its first frame.

mod common;

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use serde_json::Value;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("basal-confinement-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn probe(extra: &[&str], read: &Path, write: &Path, connect: &str) -> Value {
    let mut args = vec!["--confinement-probe"];
    args.extend_from_slice(extra);
    let read = read.to_string_lossy().into_owned();
    let write = write.to_string_lossy().into_owned();
    args.extend_from_slice(&[
        "--read",
        &read,
        "--write",
        &write,
        "--connect",
        connect,
        "--exec",
        "/bin/echo",
    ]);
    let output = Command::new(common::worker_binary())
        .args(&args)
        .env_clear()
        .output()
        .expect("run probe");
    assert!(
        output.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("probe prints JSON")
}

#[test]
fn sandbox_denies_files_sockets_and_exec() {
    let dir = scratch("probe");
    let secret = dir.join("secret.txt");
    std::fs::write(&secret, "outside the worker's allowance").expect("write secret");
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let addr = listener.local_addr().expect("addr").to_string();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 16];
            let _ = stream.read(&mut buf);
        }
    });

    let confined_write = dir.join("confined.txt");
    let confined = probe(&[], &secret, &confined_write, &addr);
    assert_eq!(confined["confinement"], "seatbelt");
    for attempt in ["read", "write", "connect", "exec"] {
        assert_eq!(
            confined[attempt]["ok"], false,
            "{attempt} succeeded inside the sandbox: {confined}"
        );
    }
    assert!(
        !confined_write.exists(),
        "the confined write created a file"
    );
    // Nothing but stdio is left open once confined.
    assert_eq!(confined["open_descriptors"], serde_json::json!([0, 1, 2]));

    // The control: without the sandbox the very same attempts succeed, so
    // the denials above are the sandbox's doing.
    let open_write = dir.join("unconfined.txt");
    let open = probe(&["--no-sandbox"], &secret, &open_write, &addr);
    assert_eq!(open["confinement"], "none");
    for attempt in ["read", "write", "connect", "exec"] {
        assert_eq!(
            open[attempt]["ok"], true,
            "{attempt} failed without the sandbox: {open}"
        );
    }
    assert!(open_write.exists());
    let _ = std::fs::remove_dir_all(&dir);
}
