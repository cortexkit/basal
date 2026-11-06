use super::DirectoryLock;
use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(format!(
                "directory-lock-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join(".lock")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pipe() -> (File, File) {
    let mut fds = [-1; 2];
    // SAFETY: pipe writes two descriptors to the provided two-element array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: each successful pipe descriptor is transferred to exactly one File.
    let files = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
    for fd in [files.0.as_raw_fd(), files.1.as_raw_fd()] {
        // SAFETY: fcntl only inspects/sets flags on these live owned descriptors.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) },
            0
        );
    }
    files
}

fn read_byte(file: &mut File) -> io::Result<()> {
    let mut poll = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll receives a live one-element pollfd array and a bounded wait.
    let ready = unsafe { libc::poll(&mut poll, 1, 10_000) };
    if ready == 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "child did not reach pre_exec",
        ));
    }
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    file.read_exact(&mut [0u8; 1])
}

#[test]
fn directory_lock_fd_is_close_on_exec() {
    let scratch = Scratch::new();
    let lock = DirectoryLock::acquire(&scratch.path()).unwrap();
    // SAFETY: the lock owns this open file descriptor for the entire query.
    let flags = unsafe { libc::fcntl(lock._file.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0);
    assert_ne!(
        flags & libc::FD_CLOEXEC,
        0,
        "a worker must not retain the lock past exec"
    );
}

#[test]
fn fork_to_exec_child_temporarily_inherits_the_directory_flock() {
    let scratch = Scratch::new();
    let lock = DirectoryLock::acquire(&scratch.path()).unwrap();
    let (mut entered, signal) = pipe();
    let (release, mut unblock) = pipe();
    let signal_fd = signal.as_raw_fd();
    let release_fd = release.as_raw_fd();
    let mut command = Command::new("/bin/sleep");
    command.arg("30");
    // SAFETY: the child callback only uses async-signal-safe read/write on raw
    // descriptors opened before fork. It takes no Rust lock and allocates nothing.
    unsafe {
        command.pre_exec(move || {
            let byte = 1u8;
            if libc::write(signal_fd, (&byte as *const u8).cast(), 1) != 1 {
                return Err(io::Error::last_os_error());
            }
            let mut byte = 0u8;
            if libc::read(release_fd, (&mut byte as *mut u8).cast(), 1) != 1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // spawn waits for exec, so another thread must own it while pre_exec parks.
    let spawning = std::thread::spawn(move || command.spawn());
    let reached = read_byte(&mut entered);
    drop(lock);
    let blocked = DirectoryLock::try_acquire(&scratch.path()).err();
    // Always release/reap the child before asserting, even if a precondition failed.
    let released = unblock.write_all(&[1]);
    let mut child = spawning.join().unwrap().unwrap();
    let still_alive = child.try_wait().unwrap().is_none();
    let after_exec = DirectoryLock::acquire(&scratch.path());
    let _ = child.kill();
    child.wait().unwrap();
    reached.unwrap();
    released.unwrap();
    assert_eq!(
        blocked
            .expect("forked child must hold the inherited flock")
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(
        still_alive,
        "exec should not need the child to exit to release FD_CLOEXEC"
    );
    assert!(
        after_exec.is_ok(),
        "exec must close the child's inherited lock descriptor"
    );
}

#[test]
fn directory_lock_retries_a_proven_transient_holder_without_overlapping_writers() {
    let scratch = Scratch::new();
    let holder = DirectoryLock::acquire(&scratch.path()).unwrap();
    let path = scratch.path();
    let (first_block, observed) = std::sync::mpsc::channel();
    let acquiring = std::thread::spawn(move || {
        let mut first_block = Some(first_block);
        DirectoryLock::acquire_inner(&path, &mut || {
            if let Some(first_block) = first_block.take() {
                first_block.send(()).unwrap();
            }
        })
    });
    let reached = observed.recv_timeout(std::time::Duration::from_secs(10));
    // Releasing only after a real WouldBlock makes this fail without retry,
    // independently of scheduling speed or a guessed sleep before lock release.
    drop(holder);
    let result = acquiring.join().unwrap();
    reached.unwrap();
    assert!(
        result.is_ok(),
        "a transient inherited holder must not reject startup"
    );
    assert_eq!(
        DirectoryLock::try_acquire(&scratch.path())
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn directory_lock_still_refuses_a_live_holder_after_the_retry_bound() {
    let scratch = Scratch::new();
    let holder = DirectoryLock::acquire(&scratch.path()).unwrap();
    let path = scratch.path();
    let (finished, result) = std::sync::mpsc::channel();
    let acquiring = std::thread::spawn(move || {
        let began = std::time::Instant::now();
        let attempt = DirectoryLock::acquire(&path);
        finished
            .send((began.elapsed(), attempt.err().map(|e| e.kind())))
            .unwrap();
    });
    let outcome = result.recv_timeout(std::time::Duration::from_secs(10));
    // If a broken retry loop waits forever, unlock it before joining/diagnosing.
    drop(holder);
    acquiring.join().unwrap();
    let (elapsed, error) = outcome.expect("a live holder must refuse within the bounded wait");
    assert_eq!(error, Some(io::ErrorKind::WouldBlock));
    assert!(
        elapsed >= super::DIRECTORY_LOCK_WAIT,
        "the admission wait ended before its bound"
    );
}
