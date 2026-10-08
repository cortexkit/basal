//! The confinement probe: evidence that the sandbox denies what it should.
//!
//! `ck-basal-worker --confinement-probe` confines itself exactly as the
//! engine does (the same confinement entry). On Linux `--syscall=` selects one
//! raw call in a fresh child, with readiness before the attempt; seccomp denial
//! kills that child with SIGSYS. On macOS it tries to read a file, create a file,
//! connect a TCP socket and execute a program, and prints one JSON line saying
//! how each attempt went. Tests and the signing gate run the real binary.
//!
//! `--no-sandbox` skips confinement so the same attempts can be shown to
//! succeed without it. It is accepted only in probe mode: the engine itself
//! never runs unconfined.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::run;

#[cfg(not(target_os = "linux"))]
use std::io::Write;
#[cfg(not(target_os = "linux"))]
use std::net::{SocketAddr, TcpStream};
#[cfg(not(target_os = "linux"))]
use std::time::Duration;

#[cfg(not(target_os = "linux"))]
use crate::confinement;

#[cfg(not(target_os = "linux"))]
struct Attempt {
    name: &'static str,
    result: Result<(), String>,
}

#[cfg(not(target_os = "linux"))]
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Runs the probe with the arguments after `--confinement-probe` and returns
/// the process exit code.
#[cfg(not(target_os = "linux"))]
pub fn run(args: &[String]) -> u8 {
    let mut sandbox = true;
    let mut read = None;
    let mut write = None;
    let mut connect = None;
    let mut exec = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--no-sandbox" => sandbox = false,
            "--read" => read = iter.next().cloned(),
            "--write" => write = iter.next().cloned(),
            "--connect" => connect = iter.next().cloned(),
            "--exec" => exec = iter.next().cloned(),
            other => {
                eprintln!("confinement probe: unknown argument {other}");
                return 64;
            }
        }
    }

    let (confinement, descriptors) = if sandbox {
        match confinement::enter() {
            Ok(entered) => ("seatbelt", entered.descriptors),
            Err(e) => {
                eprintln!("confinement probe: {e}");
                return 70;
            }
        }
    } else {
        if let Err(e) = confinement::close_inherited_descriptors() {
            eprintln!("confinement probe: {e}");
            return 70;
        }
        ("none", confinement::open_descriptors())
    };

    let mut attempts = Vec::new();
    if let Some(path) = read {
        attempts.push(Attempt {
            name: "read",
            result: std::fs::read(&path).map(drop).map_err(|e| e.to_string()),
        });
    }
    if let Some(path) = write {
        attempts.push(Attempt {
            name: "write",
            result: std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .and_then(|mut f| f.write_all(b"escaped"))
                .map_err(|e| e.to_string()),
        });
    }
    if let Some(addr) = connect {
        let result = match addr.parse::<SocketAddr>() {
            Ok(addr) => TcpStream::connect_timeout(&addr, Duration::from_secs(2))
                .map(drop)
                .map_err(|e| e.to_string()),
            Err(e) => Err(format!("bad address: {e}")),
        };
        attempts.push(Attempt {
            name: "connect",
            result,
        });
    }
    if let Some(path) = exec {
        attempts.push(Attempt {
            name: "exec",
            result: std::process::Command::new(&path)
                .output()
                .map(drop)
                .map_err(|e| e.to_string()),
        });
    }

    let mut line = format!("{{\"confinement\":{}", json_string(confinement));
    let fds: Vec<String> = descriptors.iter().map(|fd| fd.to_string()).collect();
    line.push_str(&format!(",\"open_descriptors\":[{}]", fds.join(",")));
    for attempt in &attempts {
        match &attempt.result {
            Ok(()) => line.push_str(&format!(",{}:{{\"ok\":true}}", json_string(attempt.name))),
            Err(e) => line.push_str(&format!(
                ",{}:{{\"ok\":false,\"error\":{}}}",
                json_string(attempt.name),
                json_string(e)
            )),
        }
    }
    line.push('}');
    println!("{line}");
    0
}
