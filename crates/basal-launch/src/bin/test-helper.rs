//! A minimal child image for basal-launch's own tests.
//!
//! It is a GUI-subsystem image, like the worker, so no console host starts
//! for it. Started by the launcher, it drops the start-up thread token the
//! launcher set (as the worker does before reading input), reports its
//! standard handles on stdout, optionally tries to create a file in its
//! TEMP directory and reports the outcome, then waits for one byte or the end
//! of stdin and exits.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use std::io::{Read, Write};
    use windows_sys::Win32::Security::RevertToSelf;
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    let reverted = unsafe { RevertToSelf() } != 0;
    let [stdin, stdout, stderr] = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .map(|which| unsafe { GetStdHandle(which) } as usize);
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "ready stdin={stdin} stdout={stdout} stderr={stderr} reverted={reverted}"
    );
    if std::env::args().any(|arg| arg == "--try-temp-write") {
        let outcome = match std::env::var_os("TEMP") {
            None => "no-temp".to_owned(),
            Some(directory) => {
                let path = std::path::Path::new(&directory).join("basal-launch-probe");
                match std::fs::File::create_new(&path) {
                    Ok(_) => "created".to_owned(),
                    Err(error) => format!("denied {}", error.raw_os_error().unwrap_or(-1)),
                }
            }
        };
        let _ = writeln!(out, "temp-write {outcome}");
    }
    let _ = out.flush();
    drop(out);
    let _ = std::io::stdin().read(&mut [0u8; 1]);
}

#[cfg(not(windows))]
fn main() {}
