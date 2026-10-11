//! The NATS fixture shares the launcher's spawn lock with workers. Its job
//! also ends the server if a test panics before normal fixture cleanup.

use basal_launch::{OwnedProcess, PlainCommand, PlainStdio, spawn_plain};
use std::fs::File;
use std::io;
use std::path::Path;

pub struct Child(OwnedProcess);

impl Child {
    pub fn try_wait(&mut self) -> io::Result<Option<u32>> {
        self.0.try_wait()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.0.kill()
    }

    pub fn wait(&mut self) -> io::Result<u32> {
        self.0.wait()
    }
}

pub fn start_server(dir: &Path, port: u16, log: File) -> Child {
    let program = std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|entry| entry.join("nats-server.exe"))
                .find(|candidate| candidate.is_file())
        })
        .expect(
            "REAL NATS TEST REQUIRES nats-server ON PATH; install pinned v2.15.0 (no silent skip)",
        )
        .canonicalize()
        .unwrap();
    let mut command = PlainCommand::new(program);
    command.args = ["-js", "-a", "127.0.0.1", "-p", &port.to_string(), "-sd"]
        .map(Into::into)
        .into();
    command.args.push(dir.join("jetstream").into());
    command.env = std::env::vars_os().collect();
    command.stdout = PlainStdio::Piped;
    command.stderr = PlainStdio::Piped;
    let mut child = spawn_plain(&command).expect("start real NATS server through the spawn lock");
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    for (mut pipe, mut log) in [(stdout, log.try_clone().unwrap()), (stderr, log)] {
        std::thread::spawn(move || {
            let _ = io::copy(&mut pipe, &mut log);
        });
    }
    Child(child)
}
