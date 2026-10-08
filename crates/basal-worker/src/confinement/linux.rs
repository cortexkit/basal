//! Linux startup checks, descriptor revocation, Landlock and the mandatory seccomp boundary.
use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use basal_proto::{Confinement, LANDLOCK_ABI, LandlockReport};
use landlock::{ABI, Access, AccessFs, AccessNet, Scope};

use super::{ConfinementError, Entered};
pub mod filter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LandlockMode {
    Required,
    Optional,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ArgumentError {
    Missing,
    Usage,
}
pub fn engine_arguments(args: &[String]) -> Result<LandlockMode, ArgumentError> {
    match args {
        [] => Err(ArgumentError::Missing),
        [arg] if arg == "--landlock=required" => Ok(LandlockMode::Required),
        [arg] if arg == "--landlock=optional" => Ok(LandlockMode::Optional),
        _ => Err(ArgumentError::Usage),
    }
}

/// Probe-only command-line fixtures replace procfs magic, task count, maps text
/// or the random-initialization byte count observed by startup. The engine
/// argument parser rejects all fixture flags.
#[derive(Default, Debug)]
pub struct Fixtures {
    pub proc_magic: Option<Option<i64>>,
    pub task_count: Option<usize>,
    pub maps: Option<String>,
    pub legacy_close: bool,
    pub random_result: Option<isize>,
}
#[derive(Debug)]
pub struct Options {
    pub landlock: LandlockMode,
    pub fixtures: Fixtures,
    pub landlock_only: bool,
    pub attest_descriptors: bool,
    pub attest_status: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            landlock: LandlockMode::Required,
            fixtures: Fixtures::default(),
            landlock_only: false,
            attest_descriptors: false,
            attest_status: false,
        }
    }
}

fn refuse(token: &'static str) -> ConfinementError {
    ConfinementError::Linux(token)
}
fn check(rc: libc::c_long, token: &'static str) -> Result<(), ConfinementError> {
    if rc == 0 { Ok(()) } else { Err(refuse(token)) }
}
pub fn decide_proc(magic: Option<i64>) -> Result<(), ConfinementError> {
    match magic {
        None => Err(refuse("proc-missing")),
        Some(0x9fa0) => Ok(()),
        _ => Err(refuse("proc-untrusted")),
    }
}
pub fn decide_threads(count: usize) -> Result<(), ConfinementError> {
    if count == 1 {
        Ok(())
    } else {
        Err(refuse("not-one-thread"))
    }
}
pub fn decide_stdio(magic: i64, mode: i32, fd: i32) -> Result<(), ConfinementError> {
    if magic != 0x50495045 {
        return Err(refuse("stdio-not-pipe"));
    }
    if mode & libc::O_ACCMODE
        != if fd == 0 {
            libc::O_RDONLY
        } else {
            libc::O_WRONLY
        }
    {
        return Err(refuse("stdio-wrong-mode"));
    }
    Ok(())
}
pub fn decide_personality(value: i32) -> Result<(), ConfinementError> {
    if value == -1 {
        return Err(refuse("personality-query-failed"));
    }
    if value & 0x0400000 != 0 {
        Err(refuse("read-implies-exec"))
    } else {
        Ok(())
    }
}
pub fn decide_random_priming(result: isize, expected: usize) -> Result<(), ConfinementError> {
    if usize::try_from(result).ok() == Some(expected) {
        Ok(())
    } else {
        Err(refuse("random-priming-failed"))
    }
}

pub fn decide_maps(maps: &str) -> Result<(), ConfinementError> {
    if maps.is_empty() {
        return Err(refuse("proc-untrusted"));
    }
    for line in maps.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let perms = fields
            .get(1)
            .ok_or_else(|| refuse("proc-untrusted"))?
            .as_bytes();
        if perms.len() != 4 {
            return Err(refuse("proc-untrusted"));
        }
        if perms[1] == b'w' && perms[2] == b'x' {
            return Err(refuse("mapping-wx"));
        }
        if perms[2] == b'x' && fields.iter().any(|f| f.starts_with("[stack")) {
            return Err(refuse("mapping-exec-stack"));
        }
        if perms[3] == b's' {
            return Err(refuse("mapping-shared"));
        }
        if perms[3] != b'p' {
            return Err(refuse("proc-untrusted"));
        }
    }
    Ok(())
}

fn open_at(dir: RawFd, path: &CStr, flags: i32) -> io::Result<File> {
    // SAFETY: a valid path and flags, no variadic mode is needed (no O_CREAT).
    let fd = unsafe { libc::openat(dir, path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: openat just returned an owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn fs_magic(fd: RawFd) -> io::Result<i64> {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: valid writable stat buffer, kernel verifies fd.
    if unsafe { libc::fstatfs(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() }.f_type)
}
fn directory_entries(file: File) -> io::Result<Vec<String>> {
    use std::os::fd::IntoRawFd;
    let fd = file.into_raw_fd();
    // SAFETY: fdopendir takes ownership on success, and readdir's names are valid until next call.
    unsafe {
        let dir = libc::fdopendir(fd);
        if dir.is_null() {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        let mut names = Vec::new();
        loop {
            *libc::__errno_location() = 0;
            let entry = libc::readdir(dir);
            if entry.is_null() {
                let errno = *libc::__errno_location();
                let rc = libc::closedir(dir);
                if errno != 0 {
                    return Err(io::Error::from_raw_os_error(errno));
                }
                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                return Ok(names);
            }
            let name = CStr::from_ptr((*entry).d_name.as_ptr()).to_string_lossy();
            if name != "." && name != ".." {
                names.push(name.into_owned());
            }
        }
    }
}

fn preconditions(fixtures: &Fixtures) -> Result<(), ConfinementError> {
    let proc = open_at(
        libc::AT_FDCWD,
        c"/proc",
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
    .map_err(|_| refuse("proc-missing"))?;
    let actual = fs_magic(proc.as_raw_fd()).map_err(|_| refuse("proc-untrusted"))?;
    decide_proc(fixtures.proc_magic.unwrap_or(Some(actual)))?;
    // Personality precedes maps: READ_IMPLIES_EXEC can itself create writable-executable segments.
    let personality = unsafe { libc::personality(0xffffffff) };
    decide_personality(personality)?;
    let tasks = open_at(
        proc.as_raw_fd(),
        c"self/task",
        libc::O_RDONLY | libc::O_DIRECTORY,
    )
    .and_then(directory_entries)
    .map_err(|_| refuse("proc-untrusted"))?;
    decide_threads(fixtures.task_count.unwrap_or(tasks.len()))?;
    for fd in 0..=2 {
        let magic = fs_magic(fd).map_err(|_| refuse("stdio-not-pipe"))?;
        let mode = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if mode < 0 {
            return Err(refuse("stdio-wrong-mode"));
        }
        decide_stdio(magic, mode, fd)?;
    }
    let maps = if let Some(text) = &fixtures.maps {
        text.clone()
    } else {
        let mut text = String::new();
        open_at(proc.as_raw_fd(), c"self/maps", libc::O_RDONLY)
            .and_then(|mut f| f.read_to_string(&mut text))
            .map_err(|_| refuse("proc-untrusted"))?;
        text
    };
    decide_maps(&maps)
}

pub fn descriptors() -> Result<Vec<i32>, ConfinementError> {
    let file = open_at(
        libc::AT_FDCWD,
        c"/proc/self/fd",
        libc::O_RDONLY | libc::O_DIRECTORY,
    )
    .map_err(|_| refuse("descriptor-close-failed"))?;
    let own = file.as_raw_fd();
    let mut fds: Vec<_> = directory_entries(file)
        .map_err(|_| refuse("descriptor-close-failed"))?
        .into_iter()
        .filter_map(|name| name.parse::<i32>().ok())
        .filter(|fd| *fd != own)
        .collect();
    fds.sort_unstable();
    Ok(fds)
}

/// Before Linux 5.9, ENOSYS from close_range selects a procfs inventory followed
/// by individual closes. The probe's --legacy-close flag passes force_legacy=true
/// to exercise that branch even on newer kernels; the engine always passes false.
pub fn close_descriptors(force_legacy: bool) -> Result<bool, ConfinementError> {
    if !force_legacy {
        let rc = unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) };
        if rc == 0 {
            return Ok(true);
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS) {
            return Err(refuse("descriptor-close-failed"));
        }
    }
    for fd in descriptors()? {
        if fd > 2 {
            check(unsafe { libc::close(fd) } as _, "descriptor-close-failed")?;
        }
    }
    Ok(false)
}

pub fn decide_landlock_errno(
    errno: i32,
    mode: LandlockMode,
) -> Result<Option<LandlockReport>, ConfinementError> {
    match errno {
        libc::ENOSYS | libc::EOPNOTSUPP if mode == LandlockMode::Optional => Ok(None),
        libc::ENOSYS => Err(refuse("landlock-unavailable-enosys")),
        libc::EOPNOTSUPP => Err(refuse("landlock-unavailable-eopnotsupp")),
        _ => Err(refuse("landlock-install-failed")),
    }
}

#[repr(C)]
struct RulesetAttr {
    fs: u64,
    net: u64,
    scopes: u64,
}
fn apply_landlock(mode: LandlockMode) -> Result<Option<LandlockReport>, ConfinementError> {
    // landlock's ABI compatibility detection maps both unavailable-kernel errors
    // to ABI::Unsupported (zero), losing their distinction. Query the syscall
    // directly so required mode reports ENOSYS versus EOPNOTSUPP and optional
    // mode degrades only for those errors, never for another installation failure.
    let runtime = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            1u32,
        )
    };
    if runtime < 0 {
        return decide_landlock_errno(io::Error::last_os_error().raw_os_error().unwrap_or(0), mode);
    }
    if runtime == 0 {
        return Err(refuse("landlock-install-failed"));
    }
    let applied = (runtime as u32).min(LANDLOCK_ABI);
    let abi = match applied {
        1 => ABI::V1,
        2 => ABI::V2,
        3 => ABI::V3,
        4 => ABI::V4,
        5 => ABI::V5,
        6 => ABI::V6,
        7 => ABI::V7,
        8 => ABI::V8,
        9 => ABI::V9,
        _ => return Err(refuse("landlock-install-failed")),
    };
    let attr = RulesetAttr {
        fs: AccessFs::from_all(abi).bits(),
        net: AccessNet::from_all(abi).bits(),
        scopes: Scope::from_all(abi).bits(),
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr,
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(refuse("landlock-install-failed"));
    }
    // No allowing rules are added: every filesystem, TCP and scope right of the
    // negotiated ABI is restricted. Dropping an unsupported right silently would
    // make applied_abi an inaccurate claim about the policy actually installed.
    let rc = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd as i32, 0u32) };
    let close = unsafe { libc::close(fd as i32) };
    check(close as _, "descriptor-close-failed")?;
    check(rc, "landlock-install-failed")?;
    Ok(Some(LandlockReport {
        runtime_abi: runtime as u32,
        applied_abi: applied,
    }))
}

pub fn enter(options: &Options) -> Result<Entered, ConfinementError> {
    // Recent glibc lazily allocates per-thread random-generator state with MAP_DROPPABLE.
    // Initialize the worker's sole thread here so step 0 audits that mapping too. Later
    // activations must not need that allocation: seccomp permits mmap only with flags
    // MAP_PRIVATE | MAP_ANONYMOUS, not glibc's additional mapping mode.
    let mut seed = [0u8; 16];
    let random_result = unsafe { libc::getrandom(seed.as_mut_ptr().cast(), seed.len(), 0) };
    decide_random_priming(
        options.fixtures.random_result.unwrap_or(random_result),
        seed.len(),
    )?;
    preconditions(&options.fixtures)?;
    let close_range = close_descriptors(options.fixtures.legacy_close)?;
    let descriptors = descriptors()?;
    check(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } as _,
        "no-new-privs-failed",
    )?;
    check(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } as _,
        "dumpable-failed",
    )?;
    if options.attest_status {
        let status =
            std::fs::read_to_string("/proc/self/status").map_err(|_| refuse("proc-untrusted"))?;
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .unwrap_or("missing")
        };
        let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
        eprintln!(
            "self-status before Landlock/seccomp: NoNewPrivs={} Threads={} Dumpable={dumpable}",
            field("NoNewPrivs:\t"),
            field("Threads:\t")
        );
    }
    // Finish all filter construction and procfs observations before Landlock denies
    // new opens. Dynamic-loader initialization belongs before confinement; arch_prctl,
    // set_tid_address and rseq are deliberately absent from the post-startup filter.
    let pid = unsafe { libc::getpid() } as u32;
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
    let program = filter::build(
        std::env::consts::ARCH
            .try_into()
            .map_err(|_| refuse("seccomp-install-failed"))?,
        pid,
        tid,
    )
    .map_err(|_| refuse("seccomp-install-failed"))?;
    let landlock = apply_landlock(options.landlock)?;
    if close_range {
        check(
            unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) },
            "descriptor-close-failed",
        )?;
    } else {
        // The procfs inventories close their own directory fds before Landlock. The
        // ruleset is the only fd Landlock creates, and its close was checked above.
        // On kernels without close_range there is nothing left to close individually;
        // reopening procfs would itself violate the empty Landlock filesystem policy.
    }
    if options.attest_descriptors {
        for fd in 3..1024 {
            let rc = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if rc != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::EBADF) {
                return Err(refuse("descriptor-close-failed"));
            }
        }
        eprintln!("descriptor-sweep: EBADF 3..1023");
    }
    if !options.landlock_only {
        seccompiler::apply_filter_all_threads(&program)
            .map_err(|_| refuse("seccomp-install-failed"))?;
    }
    Ok(Entered {
        confinement: Confinement::Linux {
            seccomp: !options.landlock_only,
            landlock,
        },
        descriptors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn token(result: Result<(), ConfinementError>, expected: &str) {
        assert_eq!(result.unwrap_err().to_string(), expected);
    }
    #[test]
    fn engine_argument_decision_requires_explicit_policy_and_rejects_unknowns() {
        assert_eq!(engine_arguments(&[]), Err(ArgumentError::Missing));
        for (argument, expected) in [
            ("--landlock=required", LandlockMode::Required),
            ("--landlock=optional", LandlockMode::Optional),
        ] {
            assert_eq!(engine_arguments(&[argument.into()]), Ok(expected));
        }
        for arguments in [
            vec!["--no-sandbox"],
            vec!["--landlock=no"],
            vec!["--fixture-proc=missing"],
            vec!["--landlock=required", "--landlock=optional"],
        ] {
            assert_eq!(
                engine_arguments(&arguments.into_iter().map(String::from).collect::<Vec<_>>()),
                Err(ArgumentError::Usage)
            );
        }
    }
    #[test]
    fn pure_startup_decisions() {
        token(decide_proc(None), "proc-missing");
        token(decide_proc(Some(0)), "proc-untrusted");
        decide_proc(Some(0x9fa0)).unwrap();
        for count in [0, 2, 100] {
            token(decide_threads(count), "not-one-thread");
        }
        decide_threads(1).unwrap();
        for fd in 0..3 {
            token(decide_stdio(0, 0, fd), "stdio-not-pipe");
            token(
                decide_stdio(0x50495045, libc::O_RDWR, fd),
                "stdio-wrong-mode",
            );
            decide_stdio(
                0x50495045,
                if fd == 0 {
                    libc::O_RDONLY
                } else {
                    libc::O_WRONLY
                },
                fd,
            )
            .unwrap();
        }
        decide_personality(0).unwrap();
        token(decide_personality(-1), "personality-query-failed");
        token(decide_personality(0x0400000), "read-implies-exec");
        decide_maps("0000-1000 rw-p 0 00:00 0 [stack]\n1000-2000 r-xp 0 00:00 0 /worker").unwrap();
        token(decide_maps("0000-1000 rwxp 0 00:00 0"), "mapping-wx");
        token(
            decide_maps("0000-1000 r-xp 0 00:00 0 [stack]"),
            "mapping-exec-stack",
        );
        token(decide_maps("0000-1000 r--s 0 00:00 0"), "mapping-shared");
        token(decide_maps(""), "proc-untrusted");
        token(decide_maps("bad"), "proc-untrusted");
    }
    #[test]
    fn random_priming_requires_a_full_successful_read() {
        decide_random_priming(16, 16).unwrap();
        for result in [-1, 0, 15] {
            token(decide_random_priming(result, 16), "random-priming-failed");
        }
    }
    #[test]
    fn syscall_result_decision_requires_zero_not_positive_success() {
        check(0, "seccomp-install-failed").unwrap();
        for rc in [-1, 1, 42] {
            token(
                check(rc, "seccomp-install-failed"),
                "seccomp-install-failed",
            );
        }
    }
    #[test]
    fn landlock_errno_decision_never_silently_degrades_other_errors() {
        for (errno, name) in [
            (libc::ENOSYS, "landlock-unavailable-enosys"),
            (libc::EOPNOTSUPP, "landlock-unavailable-eopnotsupp"),
        ] {
            assert_eq!(
                decide_landlock_errno(errno, LandlockMode::Required)
                    .unwrap_err()
                    .to_string(),
                name
            );
            assert_eq!(
                decide_landlock_errno(errno, LandlockMode::Optional).unwrap(),
                None
            );
        }
        for mode in [LandlockMode::Required, LandlockMode::Optional] {
            assert!(decide_landlock_errno(libc::EACCES, mode).is_err());
        }
    }
    #[test]
    fn protocol_p_is_the_pinned_crates_highest_abi() {
        assert_eq!(LANDLOCK_ABI, ABI::from(i32::MAX) as u32);
    }
}
