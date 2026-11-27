//! TEMPORARY diagnostic for an access violation seen only on Windows arm64
//! (the `windows-11-arm` CI runner, native `aarch64-pc-windows-msvc`): the
//! fuzz case `/(a|a?)+$/u` against forty `a`s and `!` killed the confined
//! worker with exit code `0xc0000005` about 90 ms into the activation, before
//! its JS time budget ran out. Every other platform runs it to a typed
//! `BudgetExhausted(JsTime)`.
//!
//! Each variant below runs in a child process (this test binary re-run on
//! the ignored `child` test), so one crash does not hide the others' results.
//! Comparing them separates the candidate causes: QuickJS alone, outside
//! basal and outside the sandbox (`bare-*`), against basal's engine with its
//! budget allocator but still unconfined (`engine-*`), and the failing
//! pattern against near relatives. On Windows a vectored exception handler
//! prints the fault's code, address, access parameters and a symbolized
//! backtrace before the child dies. `regex_crash_variants_exit_cleanly` fails
//! with every variant's outcome when any child does not exit cleanly.
//!
//! Remove this module once the crash is fixed.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use basal_proto::Profile;

use super::{EchoLink, request};
use crate::engine::run_activation;

const VARIANT_ENV: &str = "BASAL_REGEX_DIAGNOSTIC_VARIANT";
const CHILD_TEST: &str = "engine::tests::regex_crash_diagnostic::child";

/// The failing case and its relatives. A bare variant stops after `polls`
/// interrupt polls; QuickJS polls about every 10,000 backtracking steps.
const BARE: &[(&str, &str, u64)] = &[
    (
        "bare-failing-pattern",
        "/(a|a?)+$/u.test('a'.repeat(40) + '!')",
        3_000,
    ),
    (
        "bare-failing-pattern-first-poll-interrupts",
        "/(a|a?)+$/u.test('a'.repeat(40) + '!')",
        0,
    ),
    (
        "bare-without-u-flag",
        "/(a|a?)+$/.test('a'.repeat(40) + '!')",
        3_000,
    ),
    ("bare-short-subject", "/(a|a?)+$/u.exec('aaa!')", 3_000),
    (
        "bare-control-a-or-aa",
        "/(a|aa)+$/u.test('a'.repeat(40) + '!')",
        3_000,
    ),
    (
        "bare-control-a-plus",
        "/(a+)+$/u.test('a'.repeat(40) + '!')",
        3_000,
    ),
];

/// The failing case through basal's engine, unconfined, with the fuzz
/// test's 200 ms JS time budget.
const ENGINE: &[(&str, &str)] = &[
    (
        "engine-failing-pattern",
        "return /(a|a?)+$/u.test('a'.repeat(40) + '!');",
    ),
    (
        "engine-failing-pattern-via-match",
        "return 'a'.repeat(40).concat('!').match(new RegExp('(a|a?)+$', 'u'));",
    ),
];

/// The failing fuzz case (labelled `regex-2` in the fuzz tally: pattern
/// `(a|a?)+$`, subject forty `a`s and `!`), run through the regex step of the
/// fuzz script.
const FUZZ_CASE: &str = "engine-fuzz-case";
const FUZZ_SCRIPT: &str = r#"
    const v = await ops.call('mock', 'echo', trigger.raw);
    const str = typeof v === 'string' ? v : JSON.stringify(v);
    let re;
    try { re = new RegExp(trigger.pattern ?? str.slice(0, 32), 'u'); } catch (e) { re = /a|b/; }
    const m = (trigger.subject ?? str).match(re);
    return m ? m.length : 0;
"#;

#[test]
fn regex_crash_variants_exit_cleanly() {
    let exe = std::env::current_exe().expect("test binary path");
    let names = BARE
        .iter()
        .map(|(name, ..)| *name)
        .chain(ENGINE.iter().map(|(name, _)| *name))
        .chain([FUZZ_CASE]);
    let mut outcomes = Vec::new();
    let mut failed = false;
    for name in names {
        // Output goes to files rather than pipes, so a long backtrace cannot
        // fill a pipe and stall the child while it is being waited on.
        let path = |stream: &str| {
            std::env::temp_dir().join(format!(
                "basal-regex-diagnostic-{}-{name}.{stream}",
                std::process::id()
            ))
        };
        let (out_path, err_path) = (path("out"), path("err"));
        let started = Instant::now();
        let mut child = Command::new(&exe)
            .args([CHILD_TEST, "--exact", "--ignored", "--nocapture"])
            .args(["--test-threads", "1"])
            .env(VARIANT_ENV, name)
            .stdin(Stdio::null())
            .stdout(File::create(&out_path).expect("stdout file"))
            .stderr(File::create(&err_path).expect("stderr file"))
            .spawn()
            .expect("spawn the variant");
        let deadline = started + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll the variant") {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        failed |= !status.is_some_and(|status| status.success());
        let status = status.map_or("still running after 60 s, killed".to_owned(), |status| {
            status.to_string()
        });
        let read = |path: &std::path::Path| {
            let bytes = std::fs::read(path).unwrap_or_default();
            let _ = std::fs::remove_file(path);
            last_lines(&bytes)
        };
        outcomes.push(format!(
            "{name}: {status} after {:?}\n  stdout: {}\n  stderr: {}",
            started.elapsed(),
            read(&out_path),
            read(&err_path),
        ));
    }
    let report = outcomes.join("\n");
    println!("{report}");
    assert!(!failed, "a regex variant did not exit cleanly:\n{report}");
}

/// The end of a child's output, without the test harness's chatter.
fn last_lines(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| {
            !line.is_empty() && !line.starts_with("running ") && !line.starts_with("test result:")
        })
        .collect();
    let start = lines.len().saturating_sub(80);
    lines[start..].join("\n    ")
}

/// Runs the variant named by the environment; a no-op otherwise.
#[test]
#[ignore = "run only as a child of regex_crash_variants_exit_cleanly"]
fn child() {
    let Ok(name) = std::env::var(VARIANT_ENV) else {
        return;
    };
    #[cfg(windows)]
    crash_report::install();
    if let Some((_, source, polls)) = BARE.iter().find(|(n, ..)| *n == name) {
        println!("\nvariant {name}: {}", bare(source, *polls));
    } else if let Some((_, script)) = ENGINE.iter().find(|(n, _)| *n == name) {
        let mut request = request(Profile::Flow, script);
        request.budgets.js_time_micros = 200_000;
        let link = Rc::new(RefCell::new(EchoLink::default()));
        println!("\nvariant {name}: {:?}", run_activation(&request, link));
    } else if name == FUZZ_CASE {
        // The fuzz case's regex step, after a host call as in the fuzz
        // script, so it runs in a resumed job rather than the first entry.
        let mut request = request(Profile::Flow, FUZZ_SCRIPT);
        request.budgets.js_time_micros = 200_000;
        let trigger = format!(
            r#"{{"raw":"\"x\"","pattern":"(a|a?)+$","subject":"{}!"}}"#,
            "a".repeat(40)
        );
        request.trigger = basal_proto::JsonText::new(trigger).expect("small");
        let link = Rc::new(RefCell::new(EchoLink::default()));
        println!("\nvariant {name}: {:?}", run_activation(&request, link));
    } else {
        panic!("unknown variant {name}");
    }
}

/// Evaluates `source` in a plain QuickJS runtime with rquickjs's default
/// allocator and no basal setup, interrupting after `polls` polls.
fn bare(source: &str, polls: u64) -> String {
    let rt = rquickjs::Runtime::new().expect("runtime");
    let count = Rc::new(Cell::new(0u64));
    let seen = count.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        seen.set(seen.get() + 1);
        seen.get() > polls
    })));
    let ctx = rquickjs::Context::full(&rt).expect("context");
    let outcome = ctx.with(|ctx| match ctx.eval::<rquickjs::Value, _>(source) {
        Ok(value) => format!("value {value:?}"),
        Err(error) => format!("error {error}: {:?}", ctx.catch()),
    });
    format!("{outcome} after {} polls", count.get())
}

/// Prints how a fatal fault happened before Windows ends the process.
#[cfg(windows)]
mod crash_report {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_RECORD;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

    /// `EXCEPTION_POINTERS`, declared here because windows-sys gates it
    /// behind a feature this crate does not otherwise need.
    #[repr(C)]
    struct ExceptionPointers {
        record: *const EXCEPTION_RECORD,
        context: *mut c_void,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AddVectoredExceptionHandler(
            first: u32,
            handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32,
        ) -> *mut c_void;
    }

    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    static REPORTED: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn report(info: *mut ExceptionPointers) -> i32 {
        // SAFETY: Windows passes valid exception pointers to the handler.
        let record = unsafe { &*(*info).record };
        let code = record.ExceptionCode as u32;
        // Only faults that end the process are reported; first-chance
        // exceptions that some frame handles pass through untouched.
        let fatal = matches!(
            code,
            0xC000_0005 | 0xC000_001D | 0xC000_0094 | 0xC000_0096 | 0x8000_0002
        );
        if fatal && !REPORTED.swap(true, Ordering::SeqCst) {
            // SAFETY: a null name asks for this process's executable.
            let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
            let pc = record.ExceptionAddress as usize;
            let count = (record.NumberParameters as usize).min(record.ExceptionInformation.len());
            eprintln!(
                "diagnostic: exception {code:#010x} at {pc:#x} (executable base {base:#x}, \
                 offset {:#x}); parameters {:x?}",
                pc.wrapping_sub(base),
                &record.ExceptionInformation[..count],
            );
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    pub(super) fn install() {
        // SAFETY: the handler is a plain function that lives for the
        // whole process.
        unsafe {
            AddVectoredExceptionHandler(1, report);
        }
    }
}
