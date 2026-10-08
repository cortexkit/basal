//! One raw forbidden syscall per Linux child; readiness distinguishes denial from setup failure.
use crate::confinement::linux::{self, LandlockMode, Options};
use basal_proto::Confinement;
use std::io::Write;

struct Parsed {
    options: Options,
    sandbox: bool,
    call: Option<String>,
    handler: bool,
    wait: bool,
    induce_personality: bool,
}
fn parse(args: &[String]) -> Result<Parsed, ()> {
    let mut parsed = Parsed {
        options: Options::default(),
        sandbox: true,
        call: None,
        handler: false,
        wait: false,
        induce_personality: false,
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
        } else if let Some(value) = arg.strip_prefix("--fixture-tasks=") {
            parsed.options.fixtures.task_count = Some(value.parse().map_err(|_| ())?);
        } else if let Some(value) = arg.strip_prefix("--fixture-maps=") {
            parsed.options.fixtures.maps = Some(value.to_owned());
        } else if let Some(value) = arg.strip_prefix("--syscall=") {
            if ![
                "open",
                "create",
                "socket",
                "exec",
                "clone",
                "mmap-exec",
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
        unsafe {
            libc::signal(
                libc::SIGSYS,
                sigsys_handler as *const () as libc::sighandler_t,
            );
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
                "{{\"confinement\":\"linux\",\"seccomp\":{seccomp},\"landlock\":{report},\"open_descriptors\":{descriptors:?}}}"
            );
        }
        None => println!("{{\"confinement\":\"none\",\"open_descriptors\":{descriptors:?}}}"),
        _ => unreachable!(),
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
    if let Some(call) = parsed.call {
        println!("ready: {call}");
        if std::io::stdout().flush().is_err() {
            return 70;
        }
        let rc = unsafe {
            match call.as_str() {
                "open" => libc::syscall(
                    libc::SYS_openat,
                    libc::AT_FDCWD,
                    c"/dev/null".as_ptr(),
                    libc::O_RDONLY,
                    0,
                ),
                "create" => libc::syscall(
                    libc::SYS_openat,
                    libc::AT_FDCWD,
                    c"/tmp/basal-probe-create".as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                    0o600,
                ),
                "socket" => libc::syscall(libc::SYS_socket, libc::AF_INET, libc::SOCK_STREAM, 0),
                "exec" => {
                    let argv = [c"/bin/true".as_ptr(), std::ptr::null()];
                    let envp: [*const libc::c_char; 1] = [std::ptr::null()];
                    libc::syscall(libc::SYS_execve, argv[0], argv.as_ptr(), envp.as_ptr())
                }
                "clone" => libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0),
                "mmap-exec" => libc::syscall(
                    libc::SYS_mmap,
                    0,
                    4096,
                    libc::PROT_READ | libc::PROT_EXEC,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                ),
                "panic" => panic!("probe panic"),
                "abort" => std::process::abort(),
                _ => unreachable!(),
            }
        };
        println!(
            "result: {rc} errno: {}",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        );
    }
    0
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
        ] {
            assert!(parse(&[arg.into()]).is_err());
        }
        assert!(!parse(&["--no-sandbox".into()]).unwrap().sandbox);
    }
}
