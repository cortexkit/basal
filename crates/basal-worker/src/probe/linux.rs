//! One raw forbidden syscall per Linux child; readiness distinguishes denial from setup failure.
use crate::confinement::linux::{self, LandlockMode, Options};
use basal_proto::Confinement;
use std::ffi::CString;
use std::io::Write;

struct Parsed {
    options: Options,
    sandbox: bool,
    call: Option<String>,
    handler: bool,
    wait: bool,
    induce_personality: bool,
    engine_stack_fixture: Option<u64>,
    path: CString,
    port: u16,
    pid: libc::pid_t,
}
fn parse(args: &[String]) -> Result<Parsed, ()> {
    let mut parsed = Parsed {
        options: Options::default(),
        sandbox: true,
        call: None,
        handler: false,
        wait: false,
        induce_personality: false,
        engine_stack_fixture: None,
        path: c"/dev/null".to_owned(),
        port: 0,
        pid: unsafe { libc::getppid() },
    };
    for arg in args {
        if let Some(value) = arg.strip_prefix("--landlock=") {
            parsed.options.landlock = match value {
                "required" => LandlockMode::Required,
                "optional" => LandlockMode::Optional,
                _ => return Err(()),
            };
        } else if let Some(value) = arg.strip_prefix("--fixture-proc=") {
            parsed.options.fixtures.proc_magic = Some(if value == "missing" {
                None
            } else {
                Some(value.parse().map_err(|_| ())?)
            });
        } else if let Some(value) = arg.strip_prefix("--fixture-random-result=") {
            parsed.options.fixtures.random_result = Some(value.parse().map_err(|_| ())?);
        } else if let Some(value) = arg.strip_prefix("--fixture-tasks=") {
            parsed.options.fixtures.task_count = Some(value.parse().map_err(|_| ())?);
        } else if let Some(value) = arg.strip_prefix("--fixture-maps=") {
            parsed.options.fixtures.maps = Some(value.to_owned());
        } else if let Some(value) = arg.strip_prefix("--engine-stack-fixture=") {
            let stack = value.parse().map_err(|_| ())?;
            if ![32768, 131072, 1048576, 4194304].contains(&stack) {
                return Err(());
            }
            parsed.engine_stack_fixture = Some(stack);
        } else if let Some(value) = arg.strip_prefix("--path=") {
            parsed.path = CString::new(value).map_err(|_| ())?;
        } else if let Some(value) = arg.strip_prefix("--port=") {
            parsed.port = value.parse().map_err(|_| ())?;
        } else if let Some(value) = arg.strip_prefix("--pid=") {
            parsed.pid = value.parse().map_err(|_| ())?;
            if parsed.pid <= 0 {
                return Err(());
            }
        } else if let Some(value) = arg.strip_prefix("--syscall=") {
            if ![
                "open",
                "create",
                "socket",
                "stat",
                "list",
                "readlink",
                "tcp",
                "udp",
                "unix",
                "netlink",
                "connect",
                "udp-send",
                "unix-connect",
                "exec",
                "clone",
                "fork",
                "pthread-create",
                "signal-parent",
                "ptrace",
                "process-vm-readv",
                "mmap-exec",
                "mprotect-exec",
                "mmap-shared",
                "io-uring",
                "userfaultfd",
                "panic",
                "abort",
            ]
            .contains(&value)
            {
                return Err(());
            }
            parsed.call = Some(value.to_owned());
        } else {
            match arg.as_str() {
                "--attest-status" => parsed.options.attest_status = true,
                "--induce-read-implies-exec" => parsed.induce_personality = true,
                "--no-sandbox" => parsed.sandbox = false,
                "--layers=landlock" => parsed.options.landlock_only = true,
                "--legacy-close" => parsed.options.fixtures.legacy_close = true,
                "--attest-descriptors" => parsed.options.attest_descriptors = true,
                "--wait" => parsed.wait = true,
                "--sigsys-handler" => parsed.handler = true,
                _ => return Err(()),
            }
        }
    }
    Ok(parsed)
}

extern "C" fn sigsys_handler(_: i32) {
    unsafe {
        libc::_exit(99);
    }
}

pub fn run(args: &[String]) -> u8 {
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(()) => {
            eprintln!("confinement probe: invalid arguments");
            return 64;
        }
    };
    if parsed.induce_personality {
        let current = unsafe { libc::personality(0xffffffff) };
        if current < 0 || unsafe { libc::personality(current as libc::c_ulong | 0x0400000) } < 0 {
            eprintln!("confinement probe: could not induce READ_IMPLIES_EXEC");
            return 70;
        }
    }
    // Install before the filter. KILL_PROCESS cannot be caught by this handler.
    if parsed.handler {
        let previous = unsafe {
            libc::signal(
                libc::SIGSYS,
                sigsys_handler as *const () as libc::sighandler_t,
            )
        };
        if previous == libc::SIG_ERR {
            eprintln!("confinement probe: could not install SIGSYS handler");
            return 70;
        }
    }
    let (confinement, descriptors) = if parsed.sandbox {
        match linux::enter(&parsed.options) {
            Ok(entered) => (Some(entered.confinement), entered.descriptors),
            Err(e) => {
                eprintln!("confinement probe: {e}");
                return 70;
            }
        }
    } else {
        if let Err(e) = linux::close_descriptors(parsed.options.fixtures.legacy_close) {
            eprintln!("confinement probe: {e}");
            return 70;
        }
        (None, linux::descriptors().unwrap_or_default())
    };
    match confinement {
        Some(Confinement::Linux { seccomp, landlock }) => {
            let report = landlock
                .map(|r| {
                    format!(
                        "{{\"runtime_abi\":{},\"applied_abi\":{}}}",
                        r.runtime_abi, r.applied_abi
                    )
                })
                .unwrap_or_else(|| "null".into());
            println!(
                "{{\"confinement\":\"linux\",\"seccomp\":{seccomp},\"landlock\":{report},\"open_descriptors\":{descriptors:?},\"sigsys_handler\":{}}}",
                parsed.handler
            );
        }
        None => println!(
            "{{\"confinement\":\"none\",\"open_descriptors\":{descriptors:?},\"sigsys_handler\":{}}}",
            parsed.handler
        ),
        _ => unreachable!(),
    }
    if let Some(stack) = parsed.engine_stack_fixture {
        println!("ready: engine-stack-fixture");
        if std::io::stdout().flush().is_err() {
            return 70;
        }
        println!("engine-stack-fixture: {:?}", engine_stack_fixture(stack));
    }
    if parsed.wait {
        if std::io::stdout().flush().is_err() {
            return 70;
        }
        let mut byte = 0u8;
        unsafe {
            libc::read(0, (&mut byte as *mut u8).cast(), 1);
        }
    }
    if let Some(call) = &parsed.call {
        return attempt(call, &parsed);
    }
    0
}

fn attempt(call: &str, parsed: &Parsed) -> u8 {
    let mut buffer = [0u8; 4096];
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let mut ring_params = [0u64; 32];
    let mut tcp_addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    tcp_addr.sin_family = libc::AF_INET as _;
    tcp_addr.sin_port = parsed.port.to_be();
    tcp_addr.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
    let mut unix_addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    unix_addr.sun_family = libc::AF_UNIX as _;
    if call == "unix-connect" {
        let path = parsed.path.as_bytes_with_nul();
        if path.len() > unix_addr.sun_path.len() {
            eprintln!("probe setup: Unix socket path too long");
            return 70;
        }
        for (dst, src) in unix_addr.sun_path.iter_mut().zip(path) {
            *dst = *src as _;
        }
    }
    // Connect and send controls need a socket, and mprotect needs a writable page.
    // Do that setup before readiness so an unrelated setup death cannot count as denial.
    let fd = if ["connect", "udp-send", "unix-connect"].contains(&call) {
        let domain = if call == "unix-connect" {
            libc::AF_UNIX
        } else {
            libc::AF_INET
        };
        let kind = if call == "udp-send" {
            libc::SOCK_DGRAM
        } else {
            libc::SOCK_STREAM
        };
        let fd = unsafe { libc::syscall(libc::SYS_socket, domain, kind, 0) };
        if fd < 0 {
            eprintln!("probe setup: socket: {}", std::io::Error::last_os_error());
            return 70;
        }
        fd
    } else {
        -1
    };
    let page = if call == "mprotect-exec" {
        let page = unsafe {
            libc::syscall(
                libc::SYS_mmap,
                0,
                buffer.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if page == -1 {
            eprintln!("probe setup: mmap: {}", std::io::Error::last_os_error());
            return 70;
        }
        page
    } else {
        0
    };
    let raw_call = match call {
        "open" | "create" => "openat",
        "stat" => "newfstatat",
        "list" => "getdents64",
        "readlink" => "readlinkat",
        "socket" | "tcp" | "udp" | "unix" | "netlink" => "socket",
        "udp-send" => "sendto",
        "unix-connect" => "connect",
        "exec" => "execve",
        "pthread-create" => "pthread_create",
        "signal-parent" => "kill",
        "process-vm-readv" => "process_vm_readv",
        "mmap-exec" | "mmap-shared" => "mmap",
        "mprotect-exec" => "mprotect",
        "io-uring" => "io_uring_setup",
        other => other,
    };
    println!("ready: {call} syscall: {raw_call}");
    if std::io::stdout().flush().is_err() {
        return 70;
    }
    let rc = unsafe {
        *libc::__errno_location() = 0;
        match call {
            "open" | "create" => libc::syscall(
                libc::SYS_openat,
                libc::AT_FDCWD,
                parsed.path.as_ptr(),
                if call == "create" {
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
                } else {
                    libc::O_RDONLY
                },
                0o600,
            ),
            "stat" => libc::syscall(
                libc::SYS_newfstatat,
                libc::AT_FDCWD,
                parsed.path.as_ptr(),
                stat.as_mut_ptr(),
                0,
            ),
            // No directory fd survives confinement. Even a listing on the stdin pipe
            // must be killed at entry, before the kernel can return ENOTDIR.
            "list" => libc::syscall(libc::SYS_getdents64, 0, buffer.as_mut_ptr(), buffer.len()),
            "readlink" => libc::syscall(
                libc::SYS_readlinkat,
                libc::AT_FDCWD,
                parsed.path.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
            ),
            "socket" | "tcp" => {
                libc::syscall(libc::SYS_socket, libc::AF_INET, libc::SOCK_STREAM, 0)
            }
            "udp" => libc::syscall(libc::SYS_socket, libc::AF_INET, libc::SOCK_DGRAM, 0),
            "unix" => libc::syscall(libc::SYS_socket, libc::AF_UNIX, libc::SOCK_STREAM, 0),
            "netlink" => libc::syscall(
                libc::SYS_socket,
                libc::AF_NETLINK,
                libc::SOCK_RAW,
                libc::NETLINK_ROUTE,
            ),
            "connect" => libc::syscall(
                libc::SYS_connect,
                fd,
                &tcp_addr,
                std::mem::size_of_val(&tcp_addr),
            ),
            "unix-connect" => libc::syscall(
                libc::SYS_connect,
                fd,
                &unix_addr,
                std::mem::size_of_val(&unix_addr),
            ),
            "udp-send" => libc::syscall(
                libc::SYS_sendto,
                fd,
                c"probe".as_ptr(),
                5,
                0,
                &tcp_addr,
                std::mem::size_of_val(&tcp_addr),
            ),
            "exec" => {
                let argv = [c"/bin/true".as_ptr(), std::ptr::null()];
                let envp: [*const libc::c_char; 1] = [std::ptr::null()];
                libc::syscall(libc::SYS_execve, argv[0], argv.as_ptr(), envp.as_ptr())
            }
            "clone" | "fork" => {
                let pid = if call == "clone" {
                    libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0)
                } else {
                    libc::fork() as libc::c_long
                };
                if pid == 0 {
                    libc::_exit(0);
                }
                if pid > 0 {
                    let mut status = 0;
                    if libc::waitpid(pid as _, &mut status, 0) != pid as _ || status != 0 {
                        eprintln!("probe setup: forked child did not exit cleanly");
                        return 70;
                    }
                }
                pid
            }
            "pthread-create" => {
                let mut thread = std::mem::MaybeUninit::uninit();
                let rc = libc::pthread_create(
                    thread.as_mut_ptr(),
                    std::ptr::null(),
                    thread_return,
                    std::ptr::null_mut(),
                );
                if rc == 0 {
                    libc::pthread_join(thread.assume_init(), std::ptr::null_mut());
                }
                *libc::__errno_location() = rc;
                if rc == 0 { 0 } else { -1 }
            }
            // Use SIGCONT so a seccomp regression allowing kill cannot terminate the supervisor.
            "signal-parent" => libc::syscall(libc::SYS_kill, parsed.pid, libc::SIGCONT),
            "ptrace" => libc::syscall(libc::SYS_ptrace, libc::PTRACE_ATTACH, parsed.pid, 0, 0),
            "process-vm-readv" => {
                let local = libc::iovec {
                    iov_base: buffer.as_mut_ptr().cast(),
                    iov_len: 1,
                };
                let remote = libc::iovec {
                    iov_base: buffer.as_mut_ptr().cast(),
                    iov_len: 1,
                };
                libc::syscall(
                    libc::SYS_process_vm_readv,
                    parsed.pid,
                    &local,
                    1,
                    &remote,
                    1,
                    0,
                )
            }
            "mmap-exec" | "mmap-shared" => libc::syscall(
                libc::SYS_mmap,
                0,
                buffer.len(),
                if call == "mmap-exec" {
                    libc::PROT_READ | libc::PROT_EXEC
                } else {
                    libc::PROT_READ | libc::PROT_WRITE
                },
                libc::MAP_ANONYMOUS
                    | if call == "mmap-shared" {
                        libc::MAP_SHARED
                    } else {
                        libc::MAP_PRIVATE
                    },
                -1,
                0,
            ),
            "mprotect-exec" => libc::syscall(
                libc::SYS_mprotect,
                page,
                buffer.len(),
                libc::PROT_READ | libc::PROT_EXEC,
            ),
            "io-uring" => libc::syscall(libc::SYS_io_uring_setup, 1, ring_params.as_mut_ptr()),
            "userfaultfd" => {
                libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC | libc::O_NONBLOCK)
            }
            "panic" => panic!("probe panic"),
            "abort" => std::process::abort(),
            _ => unreachable!(),
        }
    };
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    println!("result: {rc} errno: {errno}");
    0
}

extern "C" fn thread_return(_: *mut libc::c_void) -> *mut libc::c_void {
    std::ptr::null_mut()
}

fn engine_stack_fixture(stack: u64) -> basal_proto::ActivationResult {
    use basal_proto::{ActivationRequest, Budgets, JsonText, Profile};
    use std::{cell::RefCell, rc::Rc};
    let request = ActivationRequest {
        activation_id: 1, profile: Profile::Flow, tools: vec![], prelude_hash: crate::engine::prelude_hash(),
        script: "return 1\n}); function f(n) { return n ? 1 + f(n - 1) : 0; } f(50); (async function () { return 1;".into(),
        trigger: JsonText::null(), self_input: JsonText::null(),
        budgets: Budgets { stack_bytes: stack, ..Default::default() }, prefix: vec![],
    };
    let link = Rc::new(RefCell::new(crate::link::Channel::new(
        std::io::Cursor::new(Vec::<u8>::new()),
        Vec::<u8>::new(),
    )));
    crate::engine::run_activation(&request, link)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probe_arguments_are_closed_and_default_to_required() {
        let default = parse(&[]).unwrap();
        assert_eq!(default.options.landlock, LandlockMode::Required);
        for arg in [
            "--bogus",
            "--landlock=no",
            "--layers=none",
            "--fixture-tasks=no",
            "--syscall=unknown",
            "--engine-stack-fixture=1",
            "--port=not-a-port",
            "--port=65536",
            "--pid=0",
            "--pid=-1",
            "--path=a\0b",
        ] {
            assert!(parse(&[arg.into()]).is_err());
        }
        assert!(!parse(&["--no-sandbox".into()]).unwrap().sandbox);
    }
}
