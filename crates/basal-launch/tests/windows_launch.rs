//! Real launches of a GUI-subsystem test child under the Windows confinement,
//! read back by the parent.
#![cfg(windows)]

use basal_launch::{
    ConfinedProcess, Deviation, JobLimits, KILL_EXIT_CODE, LOW_INTEGRITY, LaunchOptions, NULL_SID,
    TOKEN_PRIMARY, create_or_open_profile, grant_test_binary_directory, launch,
};
use std::io::{BufRead, BufReader, Read};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows_sys::Win32::System::Threading::{GetProcessMitigationPolicy, ProcessDynamicCodePolicy};

/// The job commit limit for these launches. Production passes the limit of
/// the worker's profile from `basal_proto::limits`; the test child needs far
/// less than this.
const COMMIT_BYTES: u64 = 512 * 1024 * 1024;

/// The test child, copied into a directory of its own so that the package
/// grant changes no ACL but that directory's.
fn placed_child() -> &'static Path {
    static PLACED: OnceLock<PathBuf> = OnceLock::new();
    PLACED.get_or_init(|| {
        let built = Path::new(env!("CARGO_BIN_EXE_basal-launch-test-helper"));
        let directory = built
            .parent()
            .expect("the test child has a directory")
            .join("basal-launch-placed");
        std::fs::create_dir_all(&directory).expect("create the placement directory");
        let placed = directory.join("basal-launch-test-helper.exe");
        std::fs::copy(built, &placed).expect("place the test child");
        let package = create_or_open_profile().expect("worker profile");
        grant_test_binary_directory(&placed, &package).expect("grant the package read and execute");
        placed
    })
}

fn start(deviation: Deviation, args: &[&str]) -> ConfinedProcess {
    let mut options = LaunchOptions::new(placed_child(), COMMIT_BYTES).deviation(deviation);
    for arg in args {
        options = options.arg(arg);
    }
    launch(&options).unwrap_or_else(|error| panic!("launch {deviation}: {error}"))
}

/// Reads `count` lines from the child's stdout. If the child ends first,
/// fails with its exit code and stderr.
fn lines(child: &mut ConfinedProcess, count: usize) -> Vec<String> {
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut lines = Vec::new();
    for _ in 0..count {
        let mut line = String::new();
        let read = stdout
            .read_line(&mut line)
            .expect("read the child's stdout");
        if read == 0 {
            let code = child.wait().expect("wait for the child");
            let mut stderr = String::new();
            let _ = child
                .stderr
                .take()
                .expect("stderr")
                .read_to_string(&mut stderr);
            panic!("the child ended with {code:#010x} after {lines:?}; stderr: {stderr}");
        }
        lines.push(line.trim_end().to_owned());
    }
    child.stdout = Some(stdout.into_inner());
    lines
}

/// The child's dynamic-code policy word, read through the parent's handle.
fn dynamic_code_policy(child: &ConfinedProcess) -> u32 {
    let mut policy = 0u32;
    let ok = unsafe {
        GetProcessMitigationPolicy(
            child.as_raw_handle(),
            ProcessDynamicCodePolicy,
            (&mut policy as *mut u32).cast(),
            4,
        )
    };
    assert_ne!(
        ok,
        0,
        "GetProcessMitigationPolicy: {}",
        std::io::Error::last_os_error()
    );
    policy
}

fn kill_and_reap(child: &ConfinedProcess) {
    child.kill().expect("kill");
    assert_eq!(child.wait().expect("wait"), KILL_EXIT_CODE);
    assert_eq!(child.try_wait().expect("try_wait"), Some(KILL_EXIT_CODE));
}

/// The full confinement starts the GUI-subsystem child, which runs to its
/// first output with exactly the three pipes as its standard handles. The
/// parent then reads back the child's primary token and job through its own
/// handles, and kills it.
#[test]
fn full_confinement_starts_the_child_reads_back_its_token_and_job_and_kills_it() {
    let mut child = start(Deviation::Full, &[]);
    let ready = lines(&mut child, 1).remove(0);
    let [stdin, stdout, stderr] = child.inherited_stdio();
    assert_eq!(
        ready,
        format!("ready stdin={stdin} stdout={stdout} stderr={stderr} reverted=true")
    );
    assert_eq!(
        child.try_wait().expect("try_wait"),
        None,
        "the child waits on stdin"
    );

    let token = child.primary_token().expect("read the primary token");
    let package = child.package_sid().as_str().to_owned();
    println!("full: pid {} package {package}", child.id());
    println!("full: primary token {token:?}");
    assert_eq!(token.token_type, TOKEN_PRIMARY);
    assert_eq!(token.appcontainer.as_deref(), Some(package.as_str()));
    assert!(
        token.less_privileged,
        "the WIN://NOALLAPPPKG claim is present"
    );
    assert_eq!(token.capabilities, Vec::<String>::new());
    assert_eq!(token.integrity, LOW_INTEGRITY);
    assert_eq!(token.restricting_sids, [NULL_SID]);
    assert_eq!(token.enabled_groups, Vec::<String>::new());
    assert!(token.deny_only_groups > 0);
    assert_eq!(token.privileges, 0);

    let limits = child.job_limits().expect("read the job limits");
    println!("full: job limits {limits:?}");
    assert_eq!(limits, JobLimits::confined(COMMIT_BYTES as usize));
    assert!(child.in_owned_job().expect("job membership"));
    let dynamic_code = dynamic_code_policy(&child);
    println!("full: dynamic-code policy {dynamic_code:#x}");
    assert_eq!(dynamic_code & 1, 1, "dynamic code is prohibited");

    kill_and_reap(&child);
    println!("full: killed, exit code {KILL_EXIT_CODE}");
}

/// The fully confined child cannot create a file in its own TEMP directory:
/// the directory exists and resolves, but grants the worker no write.
#[test]
fn full_confinement_denies_a_file_in_the_childs_temp_directory() {
    let mut child = start(Deviation::Full, &["--try-temp-write"]);
    let report = lines(&mut child, 2);
    println!("temp: {report:?} in {}", child.temp_dir().display());
    assert!(child.temp_dir().is_dir(), "the TEMP directory exists");
    // Win32 5 is ERROR_ACCESS_DENIED.
    assert_eq!(report[1], "temp-write denied 5");
    assert!(!child.temp_dir().join("basal-launch-probe").exists());
    kill_and_reap(&child);
}

#[cfg(feature = "deviations")]
mod deviations {
    use super::*;
    use basal_launch::Refusal;

    fn assert_refused(deviation: Deviation, refusal: Refusal) {
        assert_eq!(deviation.parent_refusal(), Some(refusal));
        let options = LaunchOptions::new(placed_child(), COMMIT_BYTES).deviation(deviation);
        let error = launch(&options).expect_err("the parent must refuse before resume");
        println!("{deviation}: {error}");
        assert_eq!(error.refusal(), Some(refusal), "{error}");
    }

    #[test]
    fn a_job_with_other_limits_is_refused_as_job_limits_mismatch() {
        assert_refused(Deviation::JobLimitsMismatch, Refusal::JobLimitsMismatch);
    }

    #[test]
    fn a_child_outside_the_owned_job_is_refused_as_not_in_owned_job() {
        assert_refused(Deviation::NotInOwnedJob, Refusal::NotInOwnedJob);
    }

    #[test]
    fn a_primary_with_privileges_is_refused_as_birth_token_mismatch() {
        assert_refused(Deviation::BirthTokenMismatch, Refusal::BirthTokenMismatch);
    }

    #[test]
    fn a_missing_start_up_token_is_refused_as_initial_token_open() {
        assert_refused(Deviation::InitialTokenOpen, Refusal::InitialTokenOpen);
    }

    /// Each worker check's variant passes the parent's checks, because the
    /// parent expects the broken property, and the child starts with that
    /// property visibly broken.
    #[test]
    fn every_worker_check_variant_starts_with_its_property_broken() {
        for deviation in Deviation::ALL {
            if deviation.worker_reason().is_none() {
                continue;
            }
            let mut child = start(deviation, &[]);
            let ready = lines(&mut child, 1).remove(0);
            assert!(ready.starts_with("ready "), "{deviation}: {ready}");
            let token = child.primary_token().expect("read the primary token");
            println!("{deviation}: started; primary token {token:?}");
            match deviation {
                Deviation::NotLpac => assert!(!token.less_privileged),
                Deviation::CapabilitiesPresent => assert_eq!(token.capabilities, ["S-1-15-3-1"]),
                Deviation::RestrictingSidMismatch => {
                    let mut sids = token.restricting_sids.clone();
                    sids.sort();
                    assert_eq!(sids, [NULL_SID, "S-1-1-0"]);
                }
                Deviation::GroupNotDenyOnly => assert_eq!(token.enabled_groups.len(), 1),
                Deviation::PrivilegesPresent => assert!(token.privileges > 0),
                Deviation::MitigationMismatch => assert_eq!(dynamic_code_policy(&child) & 1, 0),
                _ => {
                    assert!(token.less_privileged);
                    assert_eq!(token.restricting_sids, [NULL_SID]);
                }
            }
            if deviation != Deviation::NotLpac {
                assert!(token.less_privileged, "{deviation}");
            }
            kill_and_reap(&child);
        }
    }

    /// The positive controls start, with the weaker tokens they are meant
    /// to have. The plain child can create a file in the TEMP directory,
    /// which shows the path the confined child is denied is a real, writable
    /// one. The LPAC-only child is denied too: the directory grants the
    /// package read and execute only.
    #[test]
    fn the_positive_controls_start_with_their_weaker_tokens() {
        let mut child = start(Deviation::LpacOnly, &["--try-temp-write"]);
        let report = lines(&mut child, 2);
        println!("lpac-only: {report:?}");
        assert_eq!(report[1], "temp-write denied 5");
        let token = child.primary_token().expect("read the primary token");
        println!("lpac-only: primary token {token:?}");
        assert_eq!(
            token.appcontainer.as_deref(),
            Some(child.package_sid().as_str())
        );
        assert!(token.less_privileged);
        assert_eq!(token.capabilities, Vec::<String>::new());
        assert_eq!(token.restricting_sids, Vec::<String>::new());
        assert!(!token.enabled_groups.is_empty());
        kill_and_reap(&child);

        let mut child = start(Deviation::Plain, &["--try-temp-write"]);
        let report = lines(&mut child, 2);
        println!("plain: {report:?}");
        assert_eq!(report[1], "temp-write created");
        assert!(child.temp_dir().join("basal-launch-probe").is_file());
        let token = child.primary_token().expect("read the primary token");
        println!("plain: primary token {token:?}");
        assert_eq!(token.appcontainer, None);
        assert!(!token.less_privileged);
        kill_and_reap(&child);
    }
}
