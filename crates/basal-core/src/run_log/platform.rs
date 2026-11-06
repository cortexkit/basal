//! Stable-storage and lifetime-lock primitives, kept separate from the log format.

use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

pub(super) fn sync(file: &File) -> io::Result<()> {
    observe_io!(super::measurement::Operation::FileSync, sync_inner(file))
}

fn sync_inner(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the borrowed file descriptor remains open throughout fcntl.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Rust uses fdatasync on Linux and FlushFileBuffers on Windows.
        file.sync_data()
    }
}

pub(super) fn sync_dir(path: &Path) -> io::Result<()> {
    observe_io!(
        super::measurement::Operation::DirectorySync,
        sync_dir_inner(path)
    )
}

fn sync_dir_inner(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    } // Windows has no supported directory-flush equivalent.
}

pub(super) fn create(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    observe_io!(super::measurement::Operation::Create, options.open(path))
}

pub(super) fn mkdir(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        result => result,
    }
}

pub(super) struct DirectoryLock {
    _file: File,
}

#[cfg(all(test, unix))]
#[path = "directory_lock_tests.rs"]
mod directory_lock_tests;

impl DirectoryLock {
    pub(super) fn acquire(path: &Path) -> io::Result<Self> {
        #[cfg(test)]
        {
            Self::acquire_inner(path, &mut || {})
        }
        #[cfg(not(test))]
        {
            Self::acquire_inner(path)
        }
    }

    fn acquire_inner(path: &Path, #[cfg(test)] blocked: &mut dyn FnMut()) -> io::Result<Self> {
        let started = std::time::Instant::now();
        loop {
            match Self::try_acquire(path) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    #[cfg(test)]
                    blocked();
                    if started.elapsed() >= DIRECTORY_LOCK_WAIT {
                        return Err(error);
                    }
                    // A concurrent spawn can inherit this flock between fork
                    // and exec even with FD_CLOEXEC. The deterministic
                    // fork_to_exec_child_temporarily_inherits_the_directory_flock
                    // test demonstrates that window. Never bypass exclusivity;
                    // a real live holder still refuses after this short bound.
                    std::thread::sleep(
                        std::time::Duration::from_millis(10)
                            .min(DIRECTORY_LOCK_WAIT.saturating_sub(started.elapsed())),
                    );
                }
                result => return result,
            }
        }
    }

    fn try_acquire(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock takes an open descriptor and no pointers. Closing
            // the owned file releases the lock, including on process death.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::{
                Storage::FileSystem::{
                    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
                },
                System::IO::OVERLAPPED,
            };
            // SAFETY: the handle is live, OVERLAPPED is initialized, and the
            // synchronous call does not retain a pointer to this stack value.
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            if unsafe {
                LockFileEx(
                    file.as_raw_handle() as _,
                    LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                    0,
                    1,
                    0,
                    &mut overlapped,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { _file: file })
    }
}

const DIRECTORY_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
