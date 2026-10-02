//! The real worker must discard inherited authority even in probe mode.

mod common;

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

#[test]
fn worker_closes_extra_inherited_descriptors_at_startup() {
    let mut pipe = [-1; 2];
    // SAFETY: pipe points to space for the two returned descriptors.
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    // SAFETY: successful pipe returned two newly owned descriptors.
    let (_read, write) = unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
    let write_fd = write.as_raw_fd();
    let mut command = Command::new(common::worker_binary());
    command
        .args(["--confinement-probe", "--no-sandbox"])
        .env_clear();
    // SAFETY: the child callback uses only async-signal-safe descriptor calls.
    unsafe {
        command.pre_exec(move || {
            for target in [3, 64] {
                if libc::dup2(write_fd, target) < 0 || libc::fcntl(target, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let output = command.output().expect("spawn real worker");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let probe: serde_json::Value = serde_json::from_slice(&output.stdout).expect("probe JSON");
    assert_eq!(probe["confinement"], "none");
    assert_eq!(probe["open_descriptors"], serde_json::json!([0, 1, 2]));
}
