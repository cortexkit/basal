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
    let expected = module_image(&home)?;
    pid_of_image(&expected)
}

fn module_image(home: &std::path::Path) -> Option<PathBuf> {
    Some(home.parent()?.join("bin").join("ckdev-basal.exe"))
}

/// Windows can report one file under different spellings: a process started
/// through a junction (for example a build directory linked to another volume)
/// has its image reported under the volume's own drive letter. Compare the
/// final paths the OS resolves, falling back to the raw spelling when a path
/// cannot be resolved (an image that does not exist).
fn same_file_name(a: &std::path::Path, b: &std::path::Path) -> bool {
    let resolve =
        |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    resolve(a)
        .to_string_lossy()
        .eq_ignore_ascii_case(&resolve(b).to_string_lossy())
}

fn pid_of_image(expected: &std::path::Path) -> Option<String> {
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
                if same_file_name(&actual, expected) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_snapshot_finds_the_current_image_and_refuses_an_absent_image() {
        let module = module_image(std::path::Path::new(r"C:\rig\home")).unwrap();
        // Path equality normalizes separators, so assert the spelling itself:
        // the rig derives this name, it is not resolved from the filesystem.
        assert_eq!(
            module.as_os_str(),
            std::ffi::OsStr::new(r"C:\rig\bin\ckdev-basal.exe")
        );
        let image = std::env::current_exe().unwrap();
        assert_eq!(pid_of_image(&image), Some(std::process::id().to_string()));
        assert_eq!(
            pid_of_image(&image.with_file_name("absent-rig-image.exe")),
            None
        );
    }

    #[test]
    fn a_path_through_a_junction_names_the_same_image() {
        let base = std::env::temp_dir().join(format!("rig-junction-{}", std::process::id()));
        let real = base.join("real");
        let link = base.join("link");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("image.exe"), b"").unwrap();
        // Junctions need no privilege, unlike directory symlinks.
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        assert!(same_file_name(
            &link.join("image.exe"),
            &real.join("image.exe")
        ));
        assert!(!same_file_name(
            &link.join("image.exe"),
            &real.join("other.exe")
        ));
        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }
}
