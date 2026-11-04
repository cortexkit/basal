use super::*;
use crate::NoHooks;
use std::{
    fs::{self, OpenOptions},
    sync::{
        Condvar,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        // Durability tests need the OS's local filesystem: remote checkouts
        // may be shared through a filesystem that cannot perform fullfsync.
        let path = std::env::temp_dir().join(format!(
            "run-log-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn config(&self, hooks: Arc<dyn Hooks>, lease_epoch: u64) -> Config {
        Config {
            directory: self.0.join("run-log"),
            identity: Identity {
                store_id: [7; 16],
                log_epoch: 3,
            },
            lease_epoch,
            target_size: Config::TARGET_SIZE,
            hooks,
        }
    }
    fn epoch(&self) -> PathBuf {
        self.0.join("run-log/e3")
    }
    fn file(&self, seq: u64) -> PathBuf {
        segment_path(&self.epoch(), seq)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct MemoryRegistry {
    lease: AtomicU64,
    state: Mutex<RegistrySnapshot>,
    fail_promotion: AtomicBool,
}
impl MemoryRegistry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            lease: AtomicU64::new(1),
            state: Mutex::new(RegistrySnapshot {
                segments: BTreeMap::new(),
                first_retained: 0,
                predecessor: None,
                log_hwm: Lsn::default(),
                floor_synced: Lsn::default(),
                admit_lsns: vec![],
            }),
            fail_promotion: AtomicBool::new(false),
        })
    }
    fn known(&self, lsn: Lsn) {
        self.state.lock().unwrap().log_hwm = lsn;
    }
}
impl SegmentRegistry for MemoryRegistry {
    fn check_lease(&self, expected: u64) -> Result<()> {
        if self.lease.load(Ordering::SeqCst) == expected {
            Ok(())
        } else {
            Err(Error::Fenced)
        }
    }
    fn snapshot(&self) -> Result<RegistrySnapshot> {
        Ok(self.state.lock().unwrap().clone())
    }
    fn reserve(&self, expected: u64, seq: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        self.check_lease(expected)?;
        if state.segments.contains_key(&seq) {
            return Err(Error::Registry("duplicate reservation".into()));
        }
        state.segments.insert(seq, SegmentState::Reserved);
        Ok(())
    }
    fn promote(&self, expected: u64, seq: u64, synced: Lsn) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        self.check_lease(expected)?;
        if self.fail_promotion.load(Ordering::SeqCst) {
            return Err(Error::Registry("injected promotion failure".into()));
        }
        *state
            .segments
            .get_mut(&seq)
            .ok_or_else(|| Error::Registry("unreserved promotion".into()))? = SegmentState::Active;
        state.log_hwm = state.log_hwm.max(synced);
        Ok(())
    }
}

/// Durable facts stand in for the later records/replay layer. Their values are
/// fixed literals, independent of the writer's offsets, CRCs and tail decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Facts {
    deadline: u64,
    broken: u64,
    budget: u64,
    key: String,
    acknowledged: String,
    unkeyed_sent: bool,
}
impl Facts {
    fn committed() -> Self {
        Self {
            deadline: 1234,
            broken: 4,
            budget: 91,
            key: "stable-key".into(),
            acknowledged: "accepted-value".into(),
            unkeyed_sent: true,
        }
    }
}
impl LogRecord for Facts {
    fn record_type(&self) -> u8 {
        3
    }
    fn decode(kind: u8, payload: &[u8]) -> Result<Self> {
        if kind != 3 {
            return Err(Error::Corrupt("unknown Facts type"));
        }
        format::decode(payload)
    }
}

struct Cut {
    target: Mutex<Option<Boundary>>,
    fired: AtomicBool,
}
impl Cut {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            target: Mutex::new(None),
            fired: AtomicBool::new(false),
        })
    }
    fn arm(&self, boundary: Boundary) {
        *self.target.lock().unwrap() = Some(boundary);
    }
}
impl Hooks for Cut {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        if self.target.lock().unwrap().as_ref() == Some(boundary) {
            self.fired.store(true, Ordering::SeqCst);
            Step::Crash
        } else {
            Step::Continue
        }
    }
}

fn reopen(scratch: &Scratch, registry: Arc<MemoryRegistry>) -> (RunLog<Facts>, Recovered<Facts>) {
    RunLog::open(scratch.config(Arc::new(NoHooks), 1), registry).unwrap()
}

fn crash_case(boundary: Boundary) {
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    let cut = Cut::new();
    let log = RunLog::<Facts>::create(scratch.config(cut.clone(), 1), registry.clone()).unwrap();
    let acknowledged = log.append(vec![Facts::committed()]).unwrap();
    log.sync_through(acknowledged).unwrap();
    registry.known(acknowledged);
    cut.arm(boundary.clone());
    let result = if boundary == Boundary::RunLogAppended {
        let mut lazy = Facts::committed();
        lazy.acknowledged = "not acknowledged".into();
        log.append(vec![lazy])
    } else {
        // Consume the existing spare; reserve/create cut the refill, whereas
        // link/promote cut the rotation itself.
        let rotated = log.rotate();
        if matches!(boundary, Boundary::RunLogReserved | Boundary::RunLogCreated) {
            // A following command observes the asynchronous refill's cut.
            log.append(vec![])
        } else {
            rotated
        }
    };
    assert_eq!(result, Err(Error::Cut));
    assert!(cut.fired.load(Ordering::SeqCst), "{boundary:?}");
    drop(log);
    if boundary == Boundary::RunLogAppended {
        // Simulate loss of the unsynced suffix, not just a graceful reopen
        // against a kernel page cache that still contains the lazy append.
        OpenOptions::new()
            .write(true)
            .open(scratch.file(acknowledged.sequence))
            .unwrap()
            .set_len(acknowledged.offset)
            .unwrap();
    }
    let (log, recovered) = reopen(&scratch, registry.clone());
    // At the file/registry boundary, every acknowledged fact is
    // still present. A replay consumer must not resend an unkeyed sent intent;
    // keyed retries retain the key, deadline/broken/budget retain their values.
    let durable = recovered
        .records
        .iter()
        .find(|r| r.lsn == acknowledged)
        .unwrap();
    assert_eq!(durable.record, Facts::committed());
    assert!(durable.record.unkeyed_sent);
    assert_eq!(durable.record.key, "stable-key");
    assert_eq!(durable.record.acknowledged, "accepted-value");
    assert_eq!(durable.record.deadline, 1234);
    assert_eq!(durable.record.broken, 4);
    assert_eq!(durable.record.budget, 91);
    drop(log);
    let (log, second) = reopen(&scratch, registry);
    assert_eq!(second.records[0].record, Facts::committed());
    drop(log);
}

#[test]
fn crash_append_before_sync_preserves_invariant_c() {
    crash_case(Boundary::RunLogAppended);
}
#[test]
fn crash_rotation_reserve_preserves_invariant_c() {
    crash_case(Boundary::RunLogReserved);
}
#[test]
fn crash_rotation_create_preserves_invariant_c() {
    crash_case(Boundary::RunLogCreated);
}
#[test]
fn crash_rotation_link_preserves_invariant_c() {
    crash_case(Boundary::RunLogLinked);
}
#[test]
fn crash_rotation_promote_preserves_invariant_c() {
    crash_case(Boundary::RunLogPromoted);
}
#[test]
fn closed_boundary_set_has_crash_cases() {
    let cases = [
        Boundary::RunLogAppended,
        Boundary::RunLogReserved,
        Boundary::RunLogCreated,
        Boundary::RunLogLinked,
        Boundary::RunLogPromoted,
    ];
    assert_eq!(Boundary::RUN_LOG, cases);
    let declared: std::collections::BTreeSet<_> = include_str!("../hooks.rs")
        .split("pub enum Boundary {")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap()
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_suffix(','))
        .filter(|name| name.starts_with("RunLog"))
        .map(str::to_owned)
        .collect();
    let covered = Boundary::RUN_LOG
        .iter()
        .map(|boundary| format!("{boundary:?}"))
        .collect();
    assert_eq!(
        declared, covered,
        "new file-writer boundaries need a crash case"
    );
    for boundary in Boundary::RUN_LOG {
        crash_case(boundary);
    }
}

#[test]
fn segment_format_crc_and_invalid_frames() {
    assert_eq!(format::crc32c(b"123456789"), 0xe3069283);
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    let log =
        RunLog::<Facts>::create(scratch.config(Arc::new(NoHooks), 1), registry.clone()).unwrap();
    let lsn = log.append(vec![Facts::committed()]).unwrap();
    log.sync_through(lsn).unwrap();
    drop(log);
    let bytes = fs::read(scratch.file(0)).unwrap();
    assert_eq!(&bytes[..8], b"BASALLOG");
    assert_eq!(&bytes[8..12], &1u32.to_le_bytes());
    assert_eq!(&bytes[12..28], &[7; 16]);
    assert_eq!(&bytes[28..36], &3u64.to_le_bytes());
    assert_eq!(&bytes[36..44], &1u64.to_le_bytes());
    assert_eq!(&bytes[44..52], &0u64.to_le_bytes());
    assert_eq!(bytes[64], 3);
    assert_eq!(lsn.offset as usize, bytes.len());
    for invalid in [
        vec![0; 8],
        format::frame(99, &Facts::committed()).unwrap(),
        vec![1, 0, 0, 0, 0, 0, 0, 0, 3],
    ] {
        let mut file = OpenOptions::new()
            .append(true)
            .open(scratch.file(0))
            .unwrap();
        file.write_all(&invalid).unwrap();
        drop(file);
        let (log, recovered) = reopen(&scratch, registry.clone());
        assert_eq!(recovered.records.len(), 1);
        assert_eq!(fs::metadata(scratch.file(0)).unwrap().len(), lsn.offset);
        drop(log);
    }
}

#[test]
fn torn_tail_below_each_durable_source_fails_closed() {
    for source in 0..3 {
        let scratch = Scratch::new();
        let registry = MemoryRegistry::new();
        let log = RunLog::<Facts>::create(scratch.config(Arc::new(NoHooks), 1), registry.clone())
            .unwrap();
        let lsn = log.append(vec![Facts::committed()]).unwrap();
        log.sync_through(lsn).unwrap();
        drop(log);
        {
            let mut state = registry.state.lock().unwrap();
            match source {
                0 => state.log_hwm = lsn,
                1 => state.floor_synced = lsn,
                _ => state.admit_lsns.push(lsn),
            }
        }
        OpenOptions::new()
            .write(true)
            .open(scratch.file(0))
            .unwrap()
            .set_len(lsn.offset - 1)
            .unwrap();
        assert!(matches!(
            RunLog::<Facts>::open(scratch.config(Arc::new(NoHooks), 1), registry),
            Err(Error::Corrupt(_))
        ));
        assert_eq!(fs::metadata(scratch.file(0)).unwrap().len(), lsn.offset - 1);
    }
}

#[test]
fn q2_missing_empty_partial_and_invalid_headers_repair_idempotently() {
    for damage in 0..4 {
        let scratch = Scratch::new();
        let registry = MemoryRegistry::new();
        let log = RunLog::<Facts>::create(scratch.config(Arc::new(NoHooks), 1), registry.clone())
            .unwrap();
        drop(log);
        match damage {
            0 => fs::remove_file(scratch.file(1)).unwrap(),
            1 => fs::write(scratch.file(1), []).unwrap(),
            2 => fs::write(scratch.file(1), b"BASAL").unwrap(),
            _ => fs::write(scratch.file(1), [0; HEADER_LEN as usize]).unwrap(),
        }
        // Crash once at the repair's newly durable header, then restart twice.
        let cut = Cut::new();
        cut.arm(Boundary::RunLogCreated);
        assert!(matches!(
            RunLog::<Facts>::open(scratch.config(cut, 1), registry.clone()),
            Err(Error::Cut)
        ));
        for _ in 0..2 {
            let (log, _) = reopen(&scratch, registry.clone());
            assert_eq!(
                registry.snapshot().unwrap().segments[&1],
                SegmentState::Reserved
            );
            assert_eq!(fs::metadata(scratch.file(1)).unwrap().len(), HEADER_LEN);
            drop(log);
        }
    }
}

#[test]
fn q2_torn_link_is_recomputed_and_synced_link_is_promoted() {
    for complete in [false, true] {
        let scratch = Scratch::new();
        let registry = MemoryRegistry::new();
        let cut = Cut::new();
        let log =
            RunLog::<Facts>::create(scratch.config(cut.clone(), 1), registry.clone()).unwrap();
        let lsn = log.append(vec![Facts::committed()]).unwrap();
        log.sync_through(lsn).unwrap();
        cut.arm(Boundary::RunLogLinked);
        assert_eq!(log.rotate(), Err(Error::Cut));
        drop(log);
        assert_eq!(
            registry.snapshot().unwrap().segments[&1],
            SegmentState::Reserved
        );
        if !complete {
            let len = fs::metadata(scratch.file(1)).unwrap().len();
            OpenOptions::new()
                .write(true)
                .open(scratch.file(1))
                .unwrap()
                .set_len(len - 1)
                .unwrap();
        }
        if complete {
            registry.fail_promotion.store(true, Ordering::SeqCst);
            assert!(matches!(
                RunLog::<Facts>::open(scratch.config(Arc::new(NoHooks), 1), registry.clone()),
                Err(Error::Registry(_))
            ));
            registry.fail_promotion.store(false, Ordering::SeqCst);
        }
        let (log, recovered) = reopen(&scratch, registry.clone());
        assert_eq!(recovered.records[0].record, Facts::committed());
        // A sealed predecessor causes normal rotation to redo a torn Link.
        assert_eq!(
            registry.snapshot().unwrap().segments[&1],
            SegmentState::Active
        );
        assert!(log.append(vec![Facts::committed()]).is_ok());
        drop(log);
        let (log, recovered) = reopen(&scratch, registry);
        assert_eq!(recovered.records.len(), 2);
        drop(log);
    }
}

#[test]
fn q2_reserved_link_keeps_valid_records_under_the_normal_tail_rule() {
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    let cut = Cut::new();
    let log = RunLog::<Facts>::create(scratch.config(cut.clone(), 1), registry.clone()).unwrap();
    let known = log.append(vec![Facts::committed()]).unwrap();
    log.sync_through(known).unwrap();
    registry.known(known);
    cut.arm(Boundary::RunLogLinked);
    assert_eq!(log.rotate(), Err(Error::Cut));
    drop(log);
    let mut file = OpenOptions::new()
        .append(true)
        .open(scratch.file(1))
        .unwrap();
    file.write_all(&format::frame(3, &Facts::committed()).unwrap())
        .unwrap();
    let valid_end = file.metadata().unwrap().len();
    file.write_all(&[12, 0]).unwrap();
    drop(file);
    let (log, recovered) = reopen(&scratch, registry.clone());
    assert_eq!(recovered.records.len(), 2);
    assert_eq!(recovered.records[1].record, Facts::committed());
    assert_eq!(fs::metadata(scratch.file(1)).unwrap().len(), valid_end);
    assert_eq!(
        registry.snapshot().unwrap().segments[&1],
        SegmentState::Active
    );
    drop(log);
    let (log, recovered) = reopen(&scratch, registry);
    assert_eq!(recovered.records.len(), 2);
    drop(log);
}

#[test]
fn q2_torn_link_uses_predecessors_recovered_end() {
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    let log =
        RunLog::<Facts>::create(scratch.config(Arc::new(NoHooks), 1), registry.clone()).unwrap();
    let known = log.append(vec![Facts::committed()]).unwrap();
    log.sync_through(known).unwrap();
    registry.known(known);
    drop(log);
    for seq in [0, 1] {
        let mut file = OpenOptions::new()
            .append(true)
            .open(scratch.file(seq))
            .unwrap();
        file.write_all(&[12, 0]).unwrap();
    }
    let (log, recovered) = reopen(&scratch, registry.clone());
    assert_eq!(recovered.end, known);
    assert_eq!(
        registry.snapshot().unwrap().segments[&1],
        SegmentState::Reserved
    );
    assert_eq!(fs::metadata(scratch.file(1)).unwrap().len(), HEADER_LEN);
    log.rotate().unwrap();
    drop(log);
    let (log, recovered) = reopen(&scratch, registry);
    assert_eq!(recovered.records.len(), 1);
    drop(log);
}

struct FenceAtLink(Arc<MemoryRegistry>);
impl Hooks for FenceAtLink {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        if *boundary == Boundary::RunLogLinked {
            self.0.lease.store(2, Ordering::SeqCst);
        }
        Step::Continue
    }
}

#[test]
fn lease_change_after_link_sync_prevents_registration_and_watermark_release() {
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    let log = RunLog::<Facts>::create(
        scratch.config(Arc::new(FenceAtLink(registry.clone())), 1),
        registry.clone(),
    )
    .unwrap();
    let known = log.append(vec![Facts::committed()]).unwrap();
    log.sync_through(known).unwrap();
    assert_eq!(log.rotate(), Err(Error::Fenced));
    assert_eq!(log.synced_lsn(), known);
    assert_eq!(
        registry.snapshot().unwrap().segments[&1],
        SegmentState::Reserved
    );
    drop(log);
    let (log, recovered) =
        RunLog::<Facts>::open(scratch.config(Arc::new(NoHooks), 2), registry).unwrap();
    assert_eq!(recovered.records[0].record, Facts::committed());
    assert_eq!(log.append(vec![Facts::committed()]).unwrap().sequence, 2);
    drop(log);
}

#[test]
fn broken_chain_missing_sealed_segment_and_records_after_seal_fail_closed() {
    for damage in 0..4 {
        let scratch = Scratch::new();
        let registry = MemoryRegistry::new();
        let log = RunLog::<Facts>::create(scratch.config(Arc::new(NoHooks), 1), registry.clone())
            .unwrap();
        log.append(vec![Facts::committed()]).unwrap();
        log.rotate().unwrap();
        drop(log);
        match damage {
            0 => {
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(scratch.file(1))
                    .unwrap();
                Header::read(&mut file).unwrap();
                let frame = format::read_frame(&mut file).unwrap().unwrap();
                let mut end: SegmentEnd = format::decode(&frame.payload).unwrap();
                end.crc ^= 1;
                file.seek(SeekFrom::Start(HEADER_LEN)).unwrap();
                file.write_all(&format::frame(LINK, &end).unwrap()).unwrap();
            }
            1 => fs::remove_file(scratch.file(0)).unwrap(),
            2 => {
                let file = OpenOptions::new()
                    .write(true)
                    .open(scratch.file(0))
                    .unwrap();
                file.set_len(HEADER_LEN + 2).unwrap();
            }
            _ => {
                let mut file = OpenOptions::new()
                    .append(true)
                    .open(scratch.file(0))
                    .unwrap();
                file.write_all(&format::frame(3, &Facts::committed()).unwrap())
                    .unwrap();
            }
        }
        assert!(matches!(
            RunLog::<Facts>::open(scratch.config(Arc::new(NoHooks), 1), registry),
            Err(Error::Corrupt(_))
        ));
    }
}

#[test]
fn floor_anchors_link_and_deletes_only_unregistered_leftovers_below_floor() {
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    let log =
        RunLog::<Facts>::create(scratch.config(Arc::new(NoHooks), 1), registry.clone()).unwrap();
    log.append(vec![Facts::committed()]).unwrap();
    let synced = log.rotate().unwrap();
    drop(log);
    let mut file = File::open(scratch.file(1)).unwrap();
    Header::read(&mut file).unwrap();
    let link = format::read_frame(&mut file).unwrap().unwrap();
    let predecessor: SegmentEnd = format::decode(&link.payload).unwrap();
    {
        let mut state = registry.state.lock().unwrap();
        state.segments.remove(&0);
        state.first_retained = 1;
        state.predecessor = Some(predecessor);
        state.floor_synced = synced;
    }
    let (log, recovered) = reopen(&scratch, registry.clone());
    assert!(recovered.records.is_empty());
    assert!(!scratch.file(0).exists());
    drop(log);
    let (log, _) = reopen(&scratch, registry.clone());
    drop(log);
    fs::write(scratch.file(99), b"leftover above floor").unwrap();
    assert!(matches!(
        RunLog::<Facts>::open(scratch.config(Arc::new(NoHooks), 1), registry),
        Err(Error::RestoreRequired("unregistered segment"))
    ));
    assert!(scratch.file(99).exists());
}

#[test]
fn lease_checks_fence_append_sync_and_registration() {
    for action in 0..3 {
        let scratch = Scratch::new();
        let registry = MemoryRegistry::new();
        let log = RunLog::<Facts>::create(scratch.config(Arc::new(NoHooks), 1), registry.clone())
            .unwrap();
        let lsn = log.append(vec![Facts::committed()]).unwrap();
        let before = fs::read(scratch.file(0)).unwrap();
        registry.lease.store(2, Ordering::SeqCst);
        let result = match action {
            0 => log.append(vec![Facts::committed()]),
            1 => log.sync_through(lsn),
            _ => log.rotate(),
        };
        assert_eq!(result, Err(Error::Fenced));
        assert_eq!(fs::read(scratch.file(0)).unwrap(), before);
        drop(log);
        assert_eq!(registry.reserve(1, 99), Err(Error::Fenced));
        assert!(!registry.snapshot().unwrap().segments.contains_key(&99));
        assert_eq!(registry.promote(1, 1, lsn), Err(Error::Fenced));
    }
}

#[test]
fn new_lease_starts_a_new_segment_and_preserves_old_records() {
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    let log =
        RunLog::<Facts>::create(scratch.config(Arc::new(NoHooks), 1), registry.clone()).unwrap();
    let lsn = log.append(vec![Facts::committed()]).unwrap();
    log.sync_through(lsn).unwrap();
    drop(log);
    registry.lease.store(2, Ordering::SeqCst);
    let (log, recovered) =
        RunLog::<Facts>::open(scratch.config(Arc::new(NoHooks), 2), registry.clone()).unwrap();
    assert_eq!(recovered.records[0].record, Facts::committed());
    let next = log.append(vec![Facts::committed()]).unwrap();
    assert_eq!(next.sequence, 1);
    let header = Header::read(&mut File::open(scratch.file(1)).unwrap()).unwrap();
    assert_eq!(header.lease, 2);
    drop(log);
    let (log, recovered) =
        RunLog::<Facts>::open(scratch.config(Arc::new(NoHooks), 2), registry).unwrap();
    assert_eq!(recovered.records.len(), 2);
    drop(log);
}

struct Pause {
    state: Mutex<(bool, bool, bool)>,
    changed: Condvar,
}
impl Hooks for Pause {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        let mut state = self.state.lock().unwrap();
        if *boundary == Boundary::RunLogAppended && state.0 {
            state.1 = true;
            self.changed.notify_all();
            let (next, timeout) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(20), |s| !s.2)
                .unwrap();
            assert!(!timeout.timed_out(), "test did not release paused writer");
            state = next;
            state.0 = false;
        }
        Step::Continue
    }
}

#[test]
fn one_group_sync_serves_every_waiter_through_its_lsn() {
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    let pause = Arc::new(Pause {
        state: Mutex::new((false, false, false)),
        changed: Condvar::new(),
    });
    let log = RunLog::<Facts>::create(scratch.config(pause.clone(), 1), registry).unwrap();
    let end = log.append(vec![Facts::committed()]).unwrap();
    pause.state.lock().unwrap().0 = true;
    let (reply, appended) = mpsc::channel();
    log.tx.send(Command::Append(vec![], reply)).unwrap();
    let state = pause.state.lock().unwrap();
    let (state, timeout) = pause
        .changed
        .wait_timeout_while(state, Duration::from_secs(20), |s| !s.1)
        .unwrap();
    assert!(!timeout.timed_out(), "writer never paused");
    drop(state);
    let mut replies = Vec::new();
    for _ in 0..32 {
        let (tx, rx) = mpsc::channel();
        log.tx.send(Command::Sync(end, tx)).unwrap();
        replies.push(rx);
    }
    {
        let mut state = pause.state.lock().unwrap();
        state.2 = true;
        pause.changed.notify_all();
    }
    assert_eq!(
        appended.recv_timeout(Duration::from_secs(20)).unwrap(),
        Ok(end)
    );
    for reply in replies {
        assert_eq!(
            reply.recv_timeout(Duration::from_secs(20)).unwrap(),
            Ok(end)
        );
    }
    assert_eq!(log.sync_count.load(Ordering::SeqCst), 1);
    assert_eq!(log.synced_lsn(), end);
    assert_eq!(log.sync_through(end).unwrap(), end);
    assert_eq!(log.sync_count.load(Ordering::SeqCst), 1);
}

#[test]
fn platform_sync_directory_sync_and_lifetime_lock() {
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    let log =
        RunLog::<Facts>::create(scratch.config(Arc::new(NoHooks), 1), registry.clone()).unwrap();
    assert!(RunLog::<Facts>::open(scratch.config(Arc::new(NoHooks), 1), registry.clone()).is_err());
    let mut file = platform::create(&scratch.0.join("sync-probe")).unwrap();
    file.write_all(b"durable probe").unwrap();
    platform::sync(&file).unwrap();
    platform::sync_dir(&scratch.0).unwrap();
    drop(file);
    assert_eq!(
        fs::read(scratch.0.join("sync-probe")).unwrap(),
        b"durable probe"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(scratch.0.join("run-log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(scratch.file(0)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(log);
    let (log, _) = reopen(&scratch, registry);
    drop(log);
}

#[test]
fn automatic_rotation_reserves_creates_links_then_promotes() {
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    let mut config = scratch.config(Arc::new(NoHooks), 1);
    config.target_size = HEADER_LEN + 1;
    let log = RunLog::<Facts>::create(config, registry.clone()).unwrap();
    let first = log.append(vec![Facts::committed()]).unwrap();
    let second = log.append(vec![Facts::committed()]).unwrap();
    assert_eq!(second.sequence, first.sequence + 1);
    log.sync_through(second).unwrap();
    drop(log);
    let state = registry.snapshot().unwrap();
    assert_eq!(state.segments[&1], SegmentState::Active);
    assert_eq!(state.segments[&2], SegmentState::Reserved);
    let (log, recovered) = reopen(&scratch, registry);
    assert_eq!(recovered.records.len(), 2);
    drop(log);
}

#[test]
fn ordinary_open_never_initializes_absent_or_foreign_logs() {
    let scratch = Scratch::new();
    let registry = MemoryRegistry::new();
    assert!(matches!(
        RunLog::<Facts>::open(scratch.config(Arc::new(NoHooks), 1), registry.clone()),
        Err(Error::RestoreRequired(_))
    ));
    assert!(!scratch.0.join("run-log").exists());
    let log =
        RunLog::<Facts>::create(scratch.config(Arc::new(NoHooks), 1), registry.clone()).unwrap();
    drop(log);
    let mut config = scratch.config(Arc::new(NoHooks), 1);
    config.identity.store_id = [8; 16];
    assert!(matches!(
        RunLog::<Facts>::open(config, registry),
        Err(Error::RestoreRequired("foreign segment identity"))
    ));
}
