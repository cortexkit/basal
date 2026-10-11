// Compiled natively by the Windows tests. This child uses Rust's real Windows
// argv parser, independently of the encoder in the parent.
use std::io::{Read, Write};
use std::os::windows::{ffi::OsStrExt, process::CommandExt};
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let mode = arguments.next().unwrap();
    match mode.to_str().unwrap() {
        "argv" => {
            for argument in arguments {
                for unit in argument.encode_wide() {
                    print!("{unit:04x}");
                }
                println!();
            }
        }
        "mark" => std::fs::write(arguments.next().unwrap(), "worked").unwrap(),
        "stdin" => {
            let mut bytes = Vec::new();
            std::io::stdin().read_to_end(&mut bytes).unwrap();
            assert!(bytes.is_empty());
            println!("inert");
        }
        "hold" => {
            std::fs::write(arguments.next().unwrap(), std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(300));
        }
        "spawn" | "post-exit" => {
            let pid = arguments.next().unwrap();
            let ready = arguments.next().unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .arg("hold")
                .arg(&pid)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            while !std::path::Path::new(&pid).exists() {
                std::thread::sleep(Duration::from_millis(2));
            }
            std::fs::write(&ready, "ready").unwrap();
            if mode == "post-exit" {
                while !std::path::Path::new(&ready)
                    .with_extension("release")
                    .exists()
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
            } else {
                let _ = child.wait();
            }
        }
        "lines" => {
            let width: usize = arguments.next().unwrap().to_str().unwrap().parse().unwrap();
            let mut stdout = std::io::stdout().lock();
            // More bytes than the cap even with only 199 completed lines when
            // width is large. An unbounded collector never reaches process exit.
            for _ in 0..4000 {
                writeln!(stdout, "{}", "x".repeat(width)).unwrap();
            }
            stdout.flush().unwrap();
            std::thread::sleep(Duration::from_secs(300));
        }
        "breakaway" => {
            let marker = arguments.next().unwrap();
            let flags: u32 = arguments.next().unwrap().to_str().unwrap().parse().unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("mark")
                .arg(marker)
                .creation_flags(flags)
                .status();
            assert!(status.is_err(), "breakaway child was allowed");
            println!("breakaway refused");
        }
        other => panic!("unknown mode {other}"),
    }
}
