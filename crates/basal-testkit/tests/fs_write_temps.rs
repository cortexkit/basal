//! Cleanup of `fs.write` temporary files.
//!
//! A write records its temporary file in the runtime's store before it
//! creates the file, and clears the record once the file has been renamed
//! over its target. A record left behind (by a write that stopped part way
//! after its run ended, or by a crash) is acted on by the next maintenance
//! pass, which removes the recorded file only if it is still a regular file
//! with exactly the recorded name, in exactly the recorded directory, inside
//! the write's roots. Every assertion here is about state on disk and in the
//! store at a point the test chose; nothing waits for time to pass.

mod common;

#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use basal_core::{Config, NoHooks, RunState, Runtime, Store};
use basal_host::builtins::fs::{self, TempHold, TempLease, TempLedger, TempRemoval};
use basal_host::builtins::{self, BuiltinHost, codes};
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, InstallStatus, TransportError,
};
use basal_proto::CallKind;
use basal_testkit::harness::{World, scratch, test_manifest};
use common::{config, finish};
use serde_json::{Value, json};

type OnCreated = Arc<dyn Fn(&TempLease) + Send + Sync>;

/// Built-ins go to a real [`BuiltinHost`]; everything else to the world's
/// mock. The ledger a runtime binds is wrapped so a test can act at the
/// moment a write's temporary file exists and its contents are not yet
/// written.
struct TempHost {
    world_mock: basal_host::mock::MockHost,
    builtins: BuiltinHost,
    on_created: Arc<Mutex<Option<OnCreated>>>,
}

impl TempHost {
    fn new(world: &World) -> Arc<Self> {
        Arc::new(Self {
            world_mock: world.mock.clone(),
            builtins: BuiltinHost::default(),
            on_created: Arc::new(Mutex::new(None)),
        })
    }

    fn on_created(&self, f: Option<OnCreated>) {
        *self.on_created.lock().unwrap() = f;
    }
}

struct Observed {
    inner: Arc<dyn TempLedger>,
    on_created: Arc<Mutex<Option<OnCreated>>>,
}

impl TempLedger for Observed {
    fn record(&self, lease: &TempLease) -> Result<Box<dyn TempHold>, String> {
        self.inner.record(lease)
    }

    fn created(&self, lease: &TempLease) {
        self.inner.created(lease);
        let f = self.on_created.lock().unwrap().clone();
        if let Some(f) = f {
            f(lease);
        }
    }
}

impl Host for TempHost {
    fn classify(&self, kind: &CallKind) -> CallClass {
        if builtins::is_builtin(kind) {
            self.builtins.classify(kind)
        } else {
            self.world_mock.classify(kind)
        }
    }

    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        if builtins::is_builtin(&request.kind) {
            self.builtins.dispatch(request)
        } else {
            self.world_mock.dispatch(request)
        }
    }

    fn dispatch_classified(
        &self,
        request: &CallRequest,
        class: CallClass,
    ) -> Result<Dispatched, TransportError> {
        if builtins::is_builtin(&request.kind) {
            self.builtins.dispatch_classified(request, class)
        } else {
            self.world_mock.dispatch_classified(request, class)
        }
    }

    fn dispatch_committed(&self, request: &CallRequest) {
        self.world_mock.dispatch_committed(request);
    }

    fn now_ms(&self) -> f64 {
        self.world_mock.now_ms()
    }

    fn random(&self) -> f64 {
        self.world_mock.random()
    }

    fn attach(&self, sink: Arc<dyn CompletionSink>) {
        self.world_mock.attach(sink);
    }

    fn bind_fs_temps(&self, ledger: Arc<dyn TempLedger>) {
        self.builtins.bind_fs_temps(Arc::new(Observed {
            inner: ledger,
            on_created: self.on_created.clone(),
        }));
    }

    fn install_status(&self, flow_id: &str, version: u32) -> Result<InstallStatus, TransportError> {
        self.world_mock.install_status(flow_id, version)
    }
}

fn open(world: &World, host: Arc<TempHost>) -> (Runtime, Arc<Store>) {
    let store = Arc::new(Store::open(world.store_path(), world.durability).expect("store"));
    let rt = Runtime::new(
        store.clone(),
        host,
        Arc::new(world.catalog.clone()),
        Arc::new(NoHooks),
        Some(world.source.clone()),
        Config {
            selector: world.selector.clone(),
            ..config()
        },
    );
    rt.recover().expect("recover");
    (rt, store)
}

/// A granted root with a subdirectory the flow writes in, and a directory
/// outside the root holding a file nothing may touch.
struct Files {
    base: PathBuf,
    root: PathBuf,
    outside: PathBuf,
}

impl Files {
    fn new(tag: &str) -> Self {
        let base = scratch(tag);
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("sub")).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("secret.txt"), "secret").expect("secret");
        Self {
            base,
            root,
            outside,
        }
    }

    fn out(&self) -> PathBuf {
        self.root.join("sub").join("out.txt")
    }

    fn secret(&self) -> PathBuf {
        self.outside.join("secret.txt")
    }

    fn manifest_for(&self, root: &Path) -> Value {
        let mut m = test_manifest();
        m["fs"] = json!({ "write": [root.display().to_string()] });
        m
    }

    fn manifest(&self) -> Value {
        self.manifest_for(&self.root)
    }

    fn script(&self) -> String {
        format!(
            "await fs.write({:?}, 'the digest'); return 1;",
            self.out().display().to_string()
        )
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn admit(rt: &Runtime, world: &World, script: &str, manifest: &Value) -> String {
    rt.admit(&world.spec_with(rt, script, manifest).expect("approve"))
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned()
}

fn records(store: &Store) -> i64 {
    store
        .read(|c| Ok(c.query_row("SELECT count(*) FROM fs_temps", [], |r| r.get(0))?))
        .expect("count records")
}

/// Every entry of `dir` whose name starts `.basal-`.
fn temps_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".basal-"))
        .collect();
    names.sort();
    names
}

/// The ordinary path: the record exists while the file does, and nothing
/// is left once the write has returned, without any cleanup pass.
#[test]
fn a_successful_write_leaves_no_record_and_no_temporary_file() {
    let world = World::new("fs-temps-success");
    let files = Files::new("fs-temps-success-files");
    let host = TempHost::new(&world);
    let (rt, store) = open(&world, host.clone());
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let seen = seen.clone();
        let store = store.clone();
        host.on_created(Some(Arc::new(move |lease: &TempLease| {
            let recorded: i64 = store
                .read(|c| {
                    Ok(c.query_row(
                        "SELECT count(*) FROM fs_temps WHERE call_key = ?1 AND temp = ?2",
                        [&lease.call_key, &lease.temp.to_string_lossy().into_owned()],
                        |r| r.get(0),
                    )?)
                })
                .expect("read record");
            let on_disk = lease.dir.join(&lease.temp).is_file();
            seen.lock().unwrap().push((recorded, on_disk));
        })));
    }
    let run_id = admit(&rt, &world, &files.script(), &files.manifest());
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    // The file existed once, and its record was already committed then.
    assert_eq!(*seen.lock().unwrap(), [(1, true)]);
    assert_eq!(
        std::fs::read_to_string(files.out()).expect("written"),
        "the digest"
    );
    assert_eq!(records(&store), 0, "the record outlived a finished write");
    assert_eq!(temps_in(&files.root.join("sub")), Vec::<String>::new());
}

/// The run is cancelled while its write is between creating its temporary
/// file and renaming it. While the write is still running, a maintenance
/// pass leaves its file and record alone; once the write has ended without
/// accounting for its file, the next pass removes both, with no restart.
#[test]
fn a_cancelled_runs_write_that_dies_mid_flight_is_cleaned_up() {
    let world = World::new("fs-temps-cancel");
    let files = Files::new("fs-temps-cancel-files");
    let host = TempHost::new(&world);
    let (rt, store) = open(&world, host.clone());
    let (created_tx, created_rx) = mpsc::channel::<TempLease>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    host.on_created(Some(Arc::new(move |lease: &TempLease| {
        created_tx.send(lease.clone()).expect("report the file");
        let _ = release_rx.lock().unwrap().recv();
        // The write dies here: no rename, no removal, no cleared record.
        panic!("the write stops with its temporary file on disk");
    })));
    let run_id = admit(&rt, &world, &files.script(), &files.manifest());
    let driver = {
        let rt = rt.clone();
        let run_id = run_id.clone();
        std::thread::spawn(move || rt.run_to_rest(&run_id, Duration::from_secs(60)))
    };
    let lease = created_rx
        .recv()
        .expect("the write reached its temporary file");
    let temp = lease.dir.join(&lease.temp);
    assert!(temp.is_file(), "{temp:?}");
    assert_eq!(records(&store), 1);

    rt.cancel(&run_id, "test").expect("cancel");
    rt.enforce_deadlines().expect("maintenance");
    assert!(
        temp.is_file(),
        "a pass removed a file its write still holds"
    );
    assert_eq!(records(&store), 1, "a pass dropped a record a write holds");

    release_tx.send(()).expect("release the write");
    let _ = driver.join();
    rt.quiesce();
    assert_eq!(rt.run(&run_id).expect("run").state, RunState::Cancelled);
    assert!(temp.is_file(), "the dead write took its file with it");

    rt.enforce_deadlines().expect("maintenance");
    assert!(!temp.exists(), "{temp:?} survived its cancelled run");
    assert_eq!(records(&store), 0);
    assert!(!files.out().exists());
    assert_eq!(temps_in(&files.root.join("sub")), Vec::<String>::new());
}

/// What happens to the recorded file between the crash and recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tamper {
    /// Nothing: recovery removes it.
    None,
    #[cfg(unix)]
    /// The file was replaced by a symlink to a file outside the root.
    SymlinkedTemp,
    #[cfg(unix)]
    /// The directory was moved outside the root and a symlink to it put in
    /// its place.
    DirectoryMovedOutside,
    #[cfg(unix)]
    /// The directory was moved elsewhere inside the root and a symlink to it
    /// put in its place.
    DirectoryMovedWithin,
}

/// Runs the flow until its write has created its temporary file, then
/// "crashes" basal there: the store is cut, so nothing after that point is
/// committed, and the write stops without renaming or removing the file.
/// Returns the lease the write recorded.
fn crash_mid_write(world: &World, files: &Files, manifest: &Value) -> (String, TempLease) {
    let host = TempHost::new(world);
    let (rt, store) = open(world, host.clone());
    let (created_tx, created_rx) = mpsc::channel::<TempLease>();
    {
        let store = store.clone();
        host.on_created(Some(Arc::new(move |lease: &TempLease| {
            store.cut();
            created_tx.send(lease.clone()).expect("report the file");
            panic!("basal dies with the temporary file on disk");
        })));
    }
    let run_id = admit(&rt, world, &files.script(), manifest);
    let _ = rt.run_to_rest(&run_id, Duration::from_secs(60));
    rt.quiesce();
    let lease = created_rx
        .try_recv()
        .expect("the write reached its temporary file");
    drop(rt);
    world.mock.detach();
    (run_id, lease)
}

/// basal dies after creating the temporary file and before recording the
/// call's outcome. The record was committed first, so recovery's first
/// maintenance pass finds the file and removes it; when the recorded path
/// no longer names a regular file in the recorded directory inside the
/// root, the pass drops the record and touches nothing.
#[test]
fn a_crash_mid_write_is_cleaned_up_by_recovery_without_following_a_swapped_path() {
    for tamper in [
        Tamper::None,
        #[cfg(unix)]
        Tamper::SymlinkedTemp,
        #[cfg(unix)]
        Tamper::DirectoryMovedOutside,
        #[cfg(unix)]
        Tamper::DirectoryMovedWithin,
    ] {
        let world = World::new("fs-temps-crash");
        let files = Files::new("fs-temps-crash-files");
        let (run_id, lease) = crash_mid_write(&world, &files, &files.manifest());
        let temp = lease.dir.join(&lease.temp);
        assert!(temp.is_file(), "{tamper:?}: {temp:?}");
        let sub = files.root.join("sub");
        // Where the recorded file is after tampering, if it is anywhere.
        let left = match tamper {
            Tamper::None => None,
            #[cfg(unix)]
            Tamper::SymlinkedTemp => {
                std::fs::remove_file(&temp).expect("remove");
                symlink(files.secret(), &temp).expect("symlink");
                Some(temp.clone())
            }
            #[cfg(unix)]
            Tamper::DirectoryMovedOutside => {
                let moved = files.outside.join("sub");
                std::fs::rename(&sub, &moved).expect("move out");
                symlink(&moved, &sub).expect("symlink");
                Some(moved.join(&lease.temp))
            }
            #[cfg(unix)]
            Tamper::DirectoryMovedWithin => {
                let moved = files.root.join("moved");
                std::fs::rename(&sub, &moved).expect("move within");
                symlink(&moved, &sub).expect("symlink");
                Some(moved.join(&lease.temp))
            }
        };

        let host = TempHost::new(&world);
        let (rt, store) = open(&world, host);
        assert_eq!(records(&store), 1, "{tamper:?}: the record was not durable");
        rt.enforce_deadlines().expect("maintenance");
        assert_eq!(records(&store), 0, "{tamper:?}");
        match left {
            None => assert!(!temp.exists(), "{temp:?} survived recovery"),
            Some(left) => {
                let meta = std::fs::symlink_metadata(&left)
                    .unwrap_or_else(|e| panic!("{tamper:?}: {left:?} was removed: {e}"));
                #[cfg(unix)]
                assert_eq!(meta.is_symlink(), tamper == Tamper::SymlinkedTemp);
                #[cfg(windows)]
                assert!(!meta.is_symlink());
            }
        }
        assert_eq!(
            std::fs::read_to_string(files.secret()).expect("secret"),
            "secret"
        );
        // A second pass with nothing recorded changes nothing.
        rt.enforce_deadlines().expect("maintenance");
        if tamper == Tamper::None {
            let run = finish(&rt, &world, &run_id);
            assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
            assert_eq!(
                std::fs::read_to_string(files.out()).expect("written"),
                "the digest"
            );
            assert_eq!(records(&store), 0);
            assert_eq!(temps_in(&sub), Vec::<String>::new());
        }
    }
}

/// The granted root is a symlink, retargeted after the crash: the recorded
/// directory still exists under its recorded path, but no longer lies in
/// the root the write was granted, so its file is not removed.
#[test]
#[cfg(unix)]
fn a_crash_record_whose_directory_left_its_root_is_not_acted_on() {
    let world = World::new("fs-temps-retarget");
    let files = Files::new("fs-temps-retarget-files");
    let link = files.base.join("rootlink");
    symlink(&files.root, &link).expect("root link");
    let (_, lease) = crash_mid_write(&world, &files, &files.manifest_for(&link));
    let temp = lease.dir.join(&lease.temp);
    assert!(temp.is_file(), "{temp:?}");
    std::fs::remove_file(&link).expect("unlink root link");
    symlink(&files.outside, &link).expect("retarget root link");

    let (rt, store) = open(&world, TempHost::new(&world));
    assert_eq!(records(&store), 1);
    rt.enforce_deadlines().expect("maintenance");
    assert_eq!(records(&store), 0);
    assert!(temp.is_file(), "{temp:?} was removed outside its root");
}

/// basal dies mid-write and the call is sent again before any cleanup pass:
/// the new send finds the file its earlier send left under the call's own
/// name, replaces it, and finishes, leaving nothing behind.
#[test]
fn a_resent_write_replaces_the_file_its_earlier_send_left() {
    let world = World::new("fs-temps-resend");
    let files = Files::new("fs-temps-resend-files");
    let (run_id, lease) = crash_mid_write(&world, &files, &files.manifest());
    let temp = lease.dir.join(&lease.temp);
    assert!(temp.is_file(), "{temp:?}");

    let (rt, store) = open(&world, TempHost::new(&world));
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert_eq!(
        std::fs::read_to_string(files.out()).expect("written"),
        "the digest"
    );
    assert_eq!(records(&store), 0);
    assert_eq!(temps_in(&files.root.join("sub")), Vec::<String>::new());
}

/// Files an earlier version of basal left, named `.basal-<pid>-<seq>.tmp`,
/// are removed once from the directories journaled writes went to. Names of
/// any other shape, symlinks, and directories no write went to are left.
#[test]
fn legacy_temporary_files_are_removed_once_from_written_directories() {
    let world = World::new("fs-temps-legacy");
    let files = Files::new("fs-temps-legacy-files");
    let sub = files.root.join("sub");
    std::fs::write(sub.join(".basal-123-4.tmp"), "stray").expect("stray");
    let keep = [
        ".basal-12a-4.tmp",
        ".basal-123-.tmp",
        ".basal-123-4.tmp.keep",
        ".basal-123.tmp",
    ];
    for name in keep {
        std::fs::write(sub.join(name), "keep").expect("decoy");
    }
    #[cfg(unix)]
    symlink(files.secret(), sub.join(".basal-55-6.tmp")).expect("symlink");
    std::fs::write(files.root.join(".basal-7-8.tmp"), "unwritten dir").expect("root stray");
    std::fs::write(files.outside.join(".basal-9-9.tmp"), "outside").expect("outside");

    let (rt, store) = open(&world, TempHost::new(&world));
    let run_id = admit(&rt, &world, &files.script(), &files.manifest());
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    rt.enforce_deadlines().expect("maintenance");

    assert!(!sub.join(".basal-123-4.tmp").exists());
    let mut expected: Vec<String> = keep.iter().map(|n| (*n).to_owned()).collect();
    #[cfg(unix)]
    expected.push(".basal-55-6.tmp".to_owned());
    expected.sort();
    assert_eq!(temps_in(&sub), expected);
    #[cfg(unix)]
    assert!(
        std::fs::symlink_metadata(sub.join(".basal-55-6.tmp"))
            .expect("symlink kept")
            .is_symlink()
    );
    assert!(files.root.join(".basal-7-8.tmp").is_file());
    assert!(files.outside.join(".basal-9-9.tmp").is_file());
    assert_eq!(
        std::fs::read_to_string(files.secret()).expect("secret"),
        "secret"
    );

    // The pass runs once per store: a stray appearing later is left.
    std::fs::write(sub.join(".basal-123-5.tmp"), "later").expect("later stray");
    drop(rt);
    drop(store);
    world.mock.detach();
    let (rt, _store) = open(&world, TempHost::new(&world));
    rt.enforce_deadlines().expect("maintenance");
    assert!(sub.join(".basal-123-5.tmp").is_file());
}

/// [`fs::remove_temp`] on its own: it removes only a regular file under a
/// temporary file name, is idempotent, and refuses a record naming anything
/// else without touching it.
#[test]
fn remove_temp_removes_only_a_recorded_regular_file() {
    let files = Files::new("fs-temps-remove");
    let sub = files.root.join("sub");
    let dir = std::fs::canonicalize(&sub).expect("real dir");
    #[cfg(windows)]
    let dir = PathBuf::from(
        dir.to_str()
            .unwrap()
            .strip_prefix(r"\\?\")
            .unwrap_or(dir.to_str().unwrap()),
    );
    let roots = vec![files.root.display().to_string()];
    let lease = |temp: &str| TempLease {
        call_key: "k".into(),
        dir: dir.clone(),
        target: "out.txt".into(),
        temp: temp.into(),
        roots: roots.clone(),
    };
    let name = fs::temp_name("bk1-0123abcd").expect("name");
    let name = name.to_str().expect("utf-8");
    assert_eq!(name, ".basal-call-bk1-0123abcd.tmp");
    std::fs::write(sub.join(name), "partial").expect("temp");
    assert_eq!(fs::remove_temp(&lease(name)), Ok(TempRemoval::Removed));
    assert!(!sub.join(name).exists());
    assert_eq!(fs::remove_temp(&lease(name)), Ok(TempRemoval::Absent));

    // A name of any other shape is never removed, whatever it holds.
    std::fs::write(sub.join("notes.txt"), "notes").expect("notes");
    assert!(matches!(
        fs::remove_temp(&lease("notes.txt")),
        Ok(TempRemoval::Refused(_))
    ));
    assert!(sub.join("notes.txt").is_file());

    // A directory under a temporary file name is not a file to remove.
    std::fs::create_dir(sub.join(name)).expect("dir");
    let Ok(TempRemoval::Refused(why)) = fs::remove_temp(&lease(name)) else {
        panic!("a directory was not refused");
    };
    assert_eq!(why.code, codes::DENIED);
    assert!(sub.join(name).is_dir());

    // Keys that cannot name a file are refused before anything is created.
    for key in ["", "a/b", "..", &"k".repeat(65)] {
        assert_eq!(
            fs::temp_name(key).map_err(|d| d.code),
            Err(codes::INVALID_ARGUMENTS),
            "{key:?}"
        );
    }
    assert!(fs::is_legacy_temp_name(".basal-1-2.tmp".as_ref()));
    assert!(!fs::is_legacy_temp_name(".basal-call-1-2.tmp".as_ref()));
}
