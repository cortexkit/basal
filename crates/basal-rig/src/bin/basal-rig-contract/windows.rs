//! Native process observations and CLI capture for the isolated Windows rig.

use std::ffi::OsString;
use std::io::Read;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

pub(super) fn basal_pid() -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let expected = home.parent()?.join("bin/ckdev-basal.exe");
    // SAFETY: snapshot handles are newly owned unless the call returns INVALID_HANDLE_VALUE.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut row: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    row.dwSize = std::mem::size_of_val(&row) as u32;
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut row) };
    while more != 0 {
        let process =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, row.th32ProcessID) };
        if !process.is_null() {
            let process = unsafe { OwnedHandle::from_raw_handle(process) };
            let mut path = vec![0u16; 32768];
            let mut size = path.len() as u32;
            // SAFETY: size describes the writable UTF-16 buffer; the process is owned.
            if unsafe {
                QueryFullProcessImageNameW(process.as_raw_handle(), 0, path.as_mut_ptr(), &mut size)
            } != 0
            {
                use std::os::windows::ffi::OsStringExt;
                let actual = PathBuf::from(OsString::from_wide(&path[..size as usize]));
                if actual
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&expected.to_string_lossy())
                {
                    return Some(row.th32ProcessID.to_string());
                }
            }
        }
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut row) };
    }
    None
}

pub(super) fn ck_output(
    program: PathBuf,
    connection: OsString,
    args: &[&str],
) -> Result<std::process::Output, String> {
    use basal_launch::{PlainCommand, PlainStdio};
    use std::os::windows::process::ExitStatusExt;
    let mut argv = vec!["--subc".into(), connection, "--json".into()];
    argv.extend(args.iter().map(|arg| OsString::from(*arg)));
    let command = PlainCommand {
        program,
        args: argv,
        env: std::env::vars_os().collect(),
        cwd: None,
        stdin: PlainStdio::Null,
        stdout: PlainStdio::Piped,
        stderr: PlainStdio::Piped,
    };
    let mut child = basal_launch::spawn_plain(&command).map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().ok_or("no stdout pipe")?;
    let mut stderr = child.stderr.take().ok_or("no stderr pipe")?;
    let out = std::thread::spawn(move || {
        let mut bytes = vec![];
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = vec![];
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let status = child.wait().map_err(|e| e.to_string())?;
    // End descendants before joining pipe readers, even when the CLI exited normally.
    child.kill().map_err(|e| e.to_string())?;
    drop(child);
    let stdout = out
        .join()
        .map_err(|_| "stdout reader panicked")?
        .map_err(|e| e.to_string())?;
    let stderr = err
        .join()
        .map_err(|_| "stderr reader panicked")?
        .map_err(|e| e.to_string())?;
    Ok(std::process::Output {
        status: std::process::ExitStatus::from_raw(status),
        stdout,
        stderr,
    })
}
