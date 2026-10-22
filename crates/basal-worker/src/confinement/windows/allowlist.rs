//! The handle allowlist (startup step 6).
//!
//! Before it reads input the worker lists every handle in its own table and
//! accepts the table only if it fits one of two measured profiles, one per
//! supported Windows image. Some rows are exact: the three standard pipes, the
//! loader's read-only `\KnownDlls` directory and the kernel CNG device that
//! `bcryptprimitives.dll` keeps open must each be there. Every other row is a
//! ceiling on handles the system's own start-up code creates for the
//! process's thread pool, loader and crypto initialisation: unnamed, private
//! objects that give no file, network, process or persistent reach. Anything
//! else (any other file, a section, process, thread, token, job, key, port,
//! another directory, any inheritable handle that is not a standard pipe) is
//! refused.
//!
//! The table must fit a single profile on its own. The two profiles are never
//! combined, so a table holding Windows 11's SchedulerSharedData handle and
//! Server 2022's two CNG semaphores fits neither and is refused.

use std::collections::BTreeMap;

/// Which standard handle an entry is, by its handle value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdio {
    Input,
    Output,
    Error,
}

/// One handle in the worker's table, as identified without dereferencing it:
/// its type comes from the table's type index and the system's type list.
/// Only the single Directory handle and a single non-inheritable File handle
/// that is not a standard pipe have their names queried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleEntry {
    /// The handle value.
    pub value: usize,
    /// The object type name, or `unknown type <index>`.
    pub type_name: String,
    /// The granted access mask.
    pub access: u32,
    /// Whether the handle is marked inheritable (`OBJ_INHERIT`).
    pub inheritable: bool,
    /// Which standard handle it is, if its value is one of them.
    pub stdio: Option<Stdio>,
    /// The object name, read for the Directory handle and a non-inheritable
    /// File handle that is not a standard pipe only.
    pub name: Option<String>,
}

/// Read access to the stdin pipe, as `CreatePipe` grants it.
pub const STDIN_ACCESS: u32 = 0x0012_0189;
/// Write access to the stdout and stderr pipes, as `CreatePipe` grants it.
pub const STDOUT_ACCESS: u32 = 0x0012_0196;
/// Query and traverse on the `\KnownDlls` object directory.
pub const KNOWN_DLLS_ACCESS: u32 = 0x3;
/// The one object directory the loader keeps open.
pub const KNOWN_DLLS: &str = "\\KnownDlls";
/// The kernel CNG device. `bcryptprimitives.dll`, which Rust's standard
/// library imports for `ProcessPrng`, opens it while the process loads and
/// keeps the handle for the life of the process: it asks the kernel for
/// random seed material through it, and closes it when the DLL detaches
/// during `ExitProcess`. Closing it at startup is therefore not possible: the
/// DLL's own later close or use of the stale value is an invalid-handle
/// reference, which strict handle checks make fatal (`0xC0000008`).
pub const CNG_DEVICE: &str = "\\Device\\CNG";
/// `FILE_READ_DATA | SYNCHRONIZE`, the access `bcryptprimitives.dll` opens
/// the CNG device with: enough for its device I/O controls, nothing more.
pub const CNG_ACCESS: u32 = 0x0010_0001;
/// The kernel security device that `bcrypt.dll` opens on Windows Server 2022
/// to register for crypto configuration changes. Measured on that image, the
/// DLL never uses or closes the handle again, so startup closes it.
pub const KSECDD_DEVICE: &str = "\\Device\\KsecDD";

/// At most `count` handles of this type with exactly this access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ceiling {
    pub type_name: &'static str,
    pub access: u32,
    pub count: usize,
}

/// The measured handle table of one Windows image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub name: &'static str,
    /// The ceiling rows. A row with count 0 is a type this image never has.
    pub ceilings: [Ceiling; 7],
    /// The largest number of handles in the whole table, the five exact rows
    /// (three pipes, `\KnownDlls`, the CNG device) included.
    pub total: usize,
}

const fn ceiling(type_name: &'static str, access: u32, count: usize) -> Ceiling {
    Ceiling {
        type_name,
        access,
        count,
    }
}

/// Windows 11 (the `windows-latest` runner image).
pub const WINDOWS_LATEST: Profile = Profile {
    name: "windows-latest",
    ceilings: [
        ceiling("Event", 0x001f_0003, 7),
        ceiling("IoCompletion", 0x001f_0003, 2),
        ceiling("TpWorkerFactory", 0x000f_00ff, 2),
        ceiling("IRTimer", 0x0010_0002, 4),
        ceiling("WaitCompletionPacket", 0x1, 6),
        ceiling("Semaphore", 0x0010_0003, 0),
        ceiling("SchedulerSharedData", 0x1, 1),
    ],
    total: 27,
};

/// Windows Server 2022 (the `windows-2022` runner image).
pub const WINDOWS_2022: Profile = Profile {
    name: "windows-2022",
    ceilings: [
        ceiling("Event", 0x001f_0003, 7),
        ceiling("IoCompletion", 0x001f_0003, 2),
        ceiling("TpWorkerFactory", 0x000f_00ff, 2),
        ceiling("IRTimer", 0x0010_0002, 4),
        ceiling("WaitCompletionPacket", 0x1, 5),
        ceiling("Semaphore", 0x0010_0003, 2),
        ceiling("SchedulerSharedData", 0x1, 0),
    ],
    total: 27,
};

/// Every profile a table may fit.
pub const PROFILES: [&Profile; 2] = [&WINDOWS_LATEST, &WINDOWS_2022];

/// What startup step 3 does with one handle, before the allowlist runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leftover {
    /// Close it.
    Close,
    /// Leave it open for the allowlist to judge.
    Keep,
    /// Read its name, then ask again with the name.
    NeedsName,
}

/// One handle as startup step 3 sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate<'a> {
    pub type_name: &'a str,
    /// Whether it is one of the three standard handles.
    pub stdio: bool,
    pub inheritable: bool,
    /// Whether it is the process's current-directory handle.
    pub current_directory: bool,
    /// The object name, once read.
    pub name: Option<&'a str>,
}

/// Decides what startup step 3 does with a handle. It closes exactly three
/// kinds of handle, each of which nothing in the worker uses again:
///
/// - every ALPC Port, which is the loader's connection to the session's
///   client/server runtime port;
/// - the current-directory handle the process starts with: the worker never
///   changes directory or opens a relative path;
/// - on Windows Server 2022, the `\Device\KsecDD` handle of `bcrypt.dll`.
///
/// Any other File that is neither a standard handle nor inheritable is named
/// first and then kept, so that the allowlist judges it: the CNG device is
/// allowed, anything else is refused. Startup never closes a handle it has
/// not identified, because a system DLL that later closes or uses its own
/// copy of the value would end the worker with an invalid-handle fault.
/// An inheritable handle that is not a standard pipe is kept as well, so that
/// the allowlist refuses it instead of the worker hiding it.
pub fn leftover(candidate: &Candidate) -> Leftover {
    match candidate.type_name {
        "ALPC Port" => Leftover::Close,
        "File" if candidate.stdio || candidate.inheritable => Leftover::Keep,
        "File" if candidate.current_directory => Leftover::Close,
        "File" => match candidate.name {
            None => Leftover::NeedsName,
            Some(KSECDD_DEVICE) => Leftover::Close,
            Some(_) => Leftover::Keep,
        },
        _ => Leftover::Keep,
    }
}

fn describe(entry: &HandleEntry) -> String {
    format!(
        "handle {:#x} {} access {:#x}{}{}",
        entry.value,
        entry.type_name,
        entry.access,
        if entry.inheritable {
            " inheritable"
        } else {
            ""
        },
        entry
            .name
            .as_deref()
            .map(|name| format!(" named {name}"))
            .unwrap_or_default()
    )
}

/// Checks a handle table. Returns the profile it fits, or every reason it
/// fits none.
pub fn check(entries: &[HandleEntry]) -> Result<&'static Profile, String> {
    let mut refused: Vec<String> = Vec::new();
    let mut stdio = [0usize; 3];
    let mut known_dlls = 0usize;
    let mut cng = 0usize;
    // Handles counted against a ceiling row, by (type, access).
    let mut counted: BTreeMap<(&str, u32), usize> = BTreeMap::new();
    for entry in entries {
        if let Some(slot) = entry.stdio {
            let (index, access) = match slot {
                Stdio::Input => (0, STDIN_ACCESS),
                Stdio::Output => (1, STDOUT_ACCESS),
                Stdio::Error => (2, STDOUT_ACCESS),
            };
            if entry.type_name == "File" && entry.access == access {
                stdio[index] += 1;
            } else {
                refused.push(format!("standard {slot:?} is {}", describe(entry)));
            }
            continue;
        }
        if entry.inheritable {
            refused.push(format!("inheritable {}", describe(entry)));
            continue;
        }
        if entry.type_name == "Directory" {
            if entry.name.as_deref() == Some(KNOWN_DLLS) && entry.access == KNOWN_DLLS_ACCESS {
                known_dlls += 1;
            } else {
                refused.push(describe(entry));
            }
            continue;
        }
        if entry.type_name == "File" {
            if entry.name.as_deref() == Some(CNG_DEVICE) && entry.access == CNG_ACCESS {
                cng += 1;
            } else {
                refused.push(describe(entry));
            }
            continue;
        }
        let row = PROFILES
            .iter()
            .flat_map(|profile| profile.ceilings.iter())
            .find(|row| row.type_name == entry.type_name.as_str() && row.access == entry.access);
        match row {
            Some(row) => *counted.entry((row.type_name, row.access)).or_default() += 1,
            None => refused.push(describe(entry)),
        }
    }
    for (index, name) in ["stdin", "stdout", "stderr"].into_iter().enumerate() {
        if stdio[index] != 1 {
            refused.push(format!("{} {name} pipe handles, expected 1", stdio[index]));
        }
    }
    if known_dlls != 1 {
        refused.push(format!(
            "{known_dlls} {KNOWN_DLLS} directory handles, expected 1"
        ));
    }
    if cng != 1 {
        refused.push(format!("{cng} {CNG_DEVICE} file handles, expected 1"));
    }
    if !refused.is_empty() {
        return Err(refused.join("; "));
    }

    let mut misfits: Vec<String> = Vec::new();
    for profile in PROFILES {
        let mut over: Vec<String> = Vec::new();
        for row in &profile.ceilings {
            let count = counted
                .get(&(row.type_name, row.access))
                .copied()
                .unwrap_or(0);
            if count > row.count {
                over.push(format!(
                    "{count} {} {:#x}, at most {}",
                    row.type_name, row.access, row.count
                ));
            }
        }
        if entries.len() > profile.total {
            over.push(format!(
                "{} handles, at most {}",
                entries.len(),
                profile.total
            ));
        }
        if over.is_empty() {
            return Ok(profile);
        }
        misfits.push(format!("not {}: {}", profile.name, over.join(", ")));
    }
    Err(misfits.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(type_name: &str, access: u32) -> HandleEntry {
        HandleEntry {
            value: 0,
            type_name: type_name.into(),
            access,
            inheritable: false,
            stdio: None,
            name: None,
        }
    }

    fn rows(type_name: &str, access: u32, count: usize) -> Vec<HandleEntry> {
        (0..count).map(|_| entry(type_name, access)).collect()
    }

    /// The exact rows: the three inherited pipes, `\KnownDlls` and the CNG
    /// device.
    fn exact() -> Vec<HandleEntry> {
        let pipe = |stdio, access| HandleEntry {
            inheritable: true,
            stdio: Some(stdio),
            ..entry("File", access)
        };
        vec![
            pipe(Stdio::Input, STDIN_ACCESS),
            pipe(Stdio::Output, STDOUT_ACCESS),
            pipe(Stdio::Error, STDOUT_ACCESS),
            HandleEntry {
                name: Some(KNOWN_DLLS.into()),
                ..entry("Directory", KNOWN_DLLS_ACCESS)
            },
            cng(),
        ]
    }

    fn cng() -> HandleEntry {
        HandleEntry {
            name: Some(CNG_DEVICE.into()),
            ..entry("File", CNG_ACCESS)
        }
    }

    /// The windows-latest profile's 26 ceiling handles plus the CNG device.
    fn latest() -> Vec<HandleEntry> {
        let mut table = exact();
        table.extend(rows("Event", 0x1f0003, 7));
        table.extend(rows("IoCompletion", 0x1f0003, 2));
        table.extend(rows("TpWorkerFactory", 0xf00ff, 2));
        table.extend(rows("IRTimer", 0x100002, 4));
        table.extend(rows("WaitCompletionPacket", 0x1, 6));
        table.extend(rows("SchedulerSharedData", 0x1, 1));
        table
    }

    /// The windows-2022 profile's 26 ceiling handles plus the CNG device.
    fn server_2022() -> Vec<HandleEntry> {
        let mut table = exact();
        table.extend(rows("Event", 0x1f0003, 7));
        table.extend(rows("IoCompletion", 0x1f0003, 2));
        table.extend(rows("TpWorkerFactory", 0xf00ff, 2));
        table.extend(rows("IRTimer", 0x100002, 4));
        table.extend(rows("WaitCompletionPacket", 0x1, 5));
        table.extend(rows("Semaphore", 0x100003, 2));
        table
    }

    #[test]
    fn each_images_measured_table_fits_its_own_profile() {
        assert_eq!(latest().len(), 27);
        assert_eq!(check(&latest()), Ok(&WINDOWS_LATEST));
        assert_eq!(server_2022().len(), 27);
        assert_eq!(check(&server_2022()), Ok(&WINDOWS_2022));
    }

    #[test]
    fn a_smaller_table_fits() {
        assert_eq!(check(&exact()), Ok(&WINDOWS_LATEST));
        let mut table = exact();
        table.extend(rows("Semaphore", 0x100003, 1));
        assert_eq!(check(&table), Ok(&WINDOWS_2022));
    }

    /// Windows 11's SchedulerSharedData handle together with Server 2022's
    /// semaphores: 29 handles that fit neither profile alone, and would pass
    /// only if the two profiles were combined.
    #[test]
    fn the_mixed_table_fits_no_profile() {
        let mut mixed = latest();
        mixed.extend(rows("Semaphore", 0x100003, 2));
        assert_eq!(mixed.len(), 29);
        let error = check(&mixed).expect_err("the mixed table must be refused");
        assert!(error.contains("not windows-latest: 2 Semaphore"), "{error}");
        assert!(
            error.contains("1 SchedulerSharedData 0x1, at most 0"),
            "{error}"
        );
        // Even within the total, one row of each image is refused.
        let mut small = exact();
        small.extend(rows("Semaphore", 0x100003, 1));
        small.extend(rows("SchedulerSharedData", 0x1, 1));
        assert!(check(&small).is_err());
    }

    #[test]
    fn an_excess_count_is_refused() {
        let mut table = latest();
        table.pop();
        table.push(entry("Event", 0x1f0003));
        assert_eq!(table.len(), 27);
        let error = check(&table).expect_err("eight events");
        assert!(error.contains("8 Event 0x1f0003, at most 7"), "{error}");

        let mut table = server_2022();
        table.push(entry("WaitCompletionPacket", 0x1));
        assert!(
            check(&table).is_err(),
            "six wait packets on 2022 and 28 in all"
        );
    }

    #[test]
    fn a_table_over_the_total_is_refused() {
        // Every ceiling of windows-latest filled, plus one Semaphore that only
        // windows-2022 allows: 28 handles.
        let mut table = latest();
        table.extend(rows("Semaphore", 0x100003, 1));
        let error = check(&table).expect_err("28 handles");
        assert!(error.contains("28 handles, at most 27"), "{error}");
    }

    fn candidate(type_name: &str) -> Candidate<'_> {
        Candidate {
            type_name,
            stdio: false,
            inheritable: false,
            current_directory: false,
            name: None,
        }
    }

    fn named(name: &str) -> Candidate<'_> {
        Candidate {
            name: Some(name),
            ..candidate("File")
        }
    }

    #[test]
    fn startup_closes_the_csr_port_the_current_directory_and_ksecdd() {
        assert_eq!(leftover(&candidate("ALPC Port")), Leftover::Close);
        let directory = Candidate {
            current_directory: true,
            ..candidate("File")
        };
        assert_eq!(leftover(&directory), Leftover::Close);
        assert_eq!(leftover(&named(KSECDD_DEVICE)), Leftover::Close);
    }

    /// Closing a handle a system DLL still owns ends the worker when the DLL
    /// later closes or uses it, so only identified handles are closed. The
    /// CNG device stays open for `bcryptprimitives.dll`, and any other File
    /// stays open for the allowlist to refuse.
    #[test]
    fn startup_keeps_the_cng_device_and_every_unidentified_file() {
        assert_eq!(leftover(&candidate("File")), Leftover::NeedsName);
        assert_eq!(leftover(&named(CNG_DEVICE)), Leftover::Keep);
        assert_eq!(leftover(&named("\\Device\\Afd")), Leftover::Keep);
        assert_eq!(leftover(&named("")), Leftover::Keep);
        // The standard pipes stay, and so does an inheritable file, which the
        // allowlist must see and refuse, even one named like a closed device.
        for (stdio, inheritable) in [(true, true), (true, false), (false, true)] {
            let kept = Candidate {
                stdio,
                inheritable,
                current_directory: true,
                name: Some(KSECDD_DEVICE),
                ..candidate("File")
            };
            assert_eq!(leftover(&kept), Leftover::Keep, "{kept:?}");
        }
        for kept in [
            "Directory",
            "Event",
            "IoCompletion",
            "TpWorkerFactory",
            "IRTimer",
            "WaitCompletionPacket",
            "Semaphore",
            "SchedulerSharedData",
            "Key",
        ] {
            assert_eq!(leftover(&candidate(kept)), Leftover::Keep, "{kept}");
        }
    }

    #[test]
    fn the_cng_device_must_be_there_once_with_its_name_and_access() {
        let mut table = latest();
        table.remove(4);
        let error = check(&table).expect_err("no CNG device");
        assert!(error.contains("0 \\Device\\CNG file handles"), "{error}");
        let mut table = latest();
        table.push(cng());
        assert!(check(&table).is_err(), "two CNG device handles");
        for (name, access) in [
            (Some(CNG_DEVICE), 0x100003),
            (Some(CNG_DEVICE), 0x12019f),
            (Some(KSECDD_DEVICE), CNG_ACCESS),
            (Some("\\Device\\CNG\\x"), CNG_ACCESS),
            (None, CNG_ACCESS),
        ] {
            let mut table = latest();
            table[4] = HandleEntry {
                name: name.map(Into::into),
                ..entry("File", access)
            };
            let error = check(&table).expect_err("not the CNG device");
            assert!(error.contains("File"), "{name:?} {access:#x}: {error}");
        }
    }

    #[test]
    fn an_unknown_row_is_refused() {
        for (type_name, access) in [
            ("Key", 0x20019),
            ("Section", 0x4),
            ("Process", 0x1000),
            ("Thread", 0x1fffff),
            ("Token", 0x8),
            ("Job", 0x1f001f),
            ("ALPC Port", 0x1f0001),
            ("File", 0x100003),
            ("Mutant", 0x1f0001),
            ("unknown type 99", 0x1),
            // A known type with an access mask the table does not list.
            ("Event", 0x1f0001),
            ("TpWorkerFactory", 0xf003f),
        ] {
            let mut table = exact();
            table.push(entry(type_name, access));
            let error = check(&table).expect_err(type_name);
            assert!(error.contains(type_name), "{error}");
        }
    }

    #[test]
    fn a_missing_pipe_or_known_dlls_is_refused() {
        for missing in 0..4 {
            let mut table = latest();
            table.remove(missing);
            assert!(check(&table).is_err(), "row {missing} missing");
        }
        // A duplicate stdin pipe is not a second accepted pipe.
        let mut table = latest();
        table.push(exact().remove(0));
        assert!(check(&table).is_err());
    }

    #[test]
    fn a_standard_handle_must_be_its_own_pipe() {
        // stdout with read access, stdin with write access.
        let mut table = exact();
        table[0].access = STDOUT_ACCESS;
        assert!(check(&table).is_err());
        let mut table = exact();
        table[1].access = STDIN_ACCESS;
        assert!(check(&table).is_err());
        // A standard handle that is not a File.
        let mut table = exact();
        table[2].type_name = "Event".into();
        assert!(check(&table).is_err());
    }

    #[test]
    fn an_inheritable_handle_that_is_not_stdio_is_refused() {
        let mut table = exact();
        table.push(HandleEntry {
            inheritable: true,
            ..entry("Event", 0x1f0003)
        });
        let error = check(&table).expect_err("inheritable event");
        assert!(error.contains("inheritable"), "{error}");
        // A file handle the parent leaked, with pipe access but not one of
        // the standard handles.
        let mut table = exact();
        table.push(HandleEntry {
            inheritable: true,
            ..entry("File", STDOUT_ACCESS)
        });
        assert!(check(&table).is_err());
    }

    #[test]
    fn only_the_known_dlls_directory_is_allowed() {
        let mut table = exact();
        table[3].name = Some("\\Sessions\\1\\BaseNamedObjects".into());
        assert!(check(&table).is_err());
        let mut table = exact();
        table[3].name = None;
        assert!(check(&table).is_err());
        let mut table = exact();
        table[3].access = 0xf;
        assert!(check(&table).is_err());
        let mut table = exact();
        table.push(table[3].clone());
        assert!(check(&table).is_err(), "two directory handles");
    }
}
