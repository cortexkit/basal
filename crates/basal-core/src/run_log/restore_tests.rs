use super::*;

fn prepared() -> Fixture {
    let f = Fixture::new();
    let log = RunLog::<RunFact>::create(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
    let cut = log.append(vec![RunFact::live()]).unwrap();
    log.sync_through(cut).unwrap();
    f.admit("live", cut);
    f.store
        .write(|tx| {
            sql::advance_log_hwm(
                tx,
                EpochLsn {
                    log_epoch: 3,
                    lsn: cut,
                },
            )
        })
        .unwrap();
    drop(log);
    f
}

fn restore_once(f: &Fixture) {
    let (log, records) =
        RunLog::<RunFact>::open_restoring(f.config(Arc::new(NoHooks)), f.store.clone()).unwrap();
    assert!(records.records.is_empty());
    let first = f.store.read(sql::meta).unwrap();
    assert_eq!(first.log_epoch, 4);
    assert!(first.gc_floor.first_retained > 1);
    assert!(f.store.read(|c| sql::history_lost(c, "live")).unwrap());
    assert!(!f.store.read(sql::restore_pending).unwrap());
    assert_eq!(f.store.read(|c| sql::segments(c, 4)).unwrap().len(), 2);
    drop(log);
    let (log, _) =
        RunLog::<RunFact>::open_restoring(f.config(Arc::new(NoHooks)), f.store.clone()).unwrap();
    assert_eq!(f.store.read(sql::meta).unwrap(), first);
    drop(log);
}

#[test]
fn absent_log_reinitializes_exactly_once() {
    let f = prepared();
    fs::remove_dir_all(&f.config(Arc::new(NoHooks)).directory).unwrap();
    restore_once(&f);
}

#[test]
fn foreign_log_reinitializes_exactly_once() {
    let f = prepared();
    let path = f.scratch.file(0);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .read(true)
        .open(path)
        .unwrap();
    Header::write(
        &mut file,
        Identity {
            store_id: [99; 16],
            log_epoch: 3,
        },
        f.store.lease_epoch(),
        0,
    )
    .unwrap();
    file.sync_all().unwrap();
    drop(file);
    restore_once(&f);
}

#[test]
fn older_log_at_valid_boundary_reinitializes_exactly_once() {
    let f = prepared();
    fs::OpenOptions::new()
        .write(true)
        .open(f.scratch.file(0))
        .unwrap()
        .set_len(HEADER_LEN)
        .unwrap();
    restore_once(&f);
}

#[test]
fn newer_log_with_unregistered_segment_reinitializes_exactly_once() {
    let f = prepared();
    fs::copy(f.scratch.file(1), f.scratch.file(9)).unwrap();
    restore_once(&f);
    assert!(f.store.read(sql::meta).unwrap().gc_floor.first_retained > 9);
}

#[test]
fn missing_final_active_segment_is_restore_but_missing_sealed_predecessor_is_damage() {
    let f = prepared();
    fs::remove_file(f.scratch.file(0)).unwrap();
    restore_once(&f);

    let f = prepared();
    let (log, _) =
        RunLog::<RunFact>::open(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
    log.rotate().unwrap();
    drop(log);
    fs::remove_file(f.scratch.file(0)).unwrap();
    assert!(matches!(
        RunLog::<RunFact>::open_restoring(f.config(Arc::new(NoHooks)), f.store.clone()),
        Err(Error::Corrupt("registered sealed segment missing"))
    ));
    assert_eq!(f.store.read(sql::meta).unwrap().log_epoch, 3);
    assert!(!f.store.read(|c| sql::history_lost(c, "live")).unwrap());
}

#[test]
fn damage_inside_durable_log_is_not_restore() {
    let f = prepared();
    let path = f.scratch.file(0);
    let mut bytes = fs::read(&path).unwrap();
    bytes[HEADER_LEN as usize + 5] ^= 0x40;
    fs::write(path, bytes).unwrap();
    assert!(matches!(
        RunLog::<RunFact>::open_restoring(f.config(Arc::new(NoHooks)), f.store.clone()),
        Err(Error::Corrupt(_))
    ));
    assert_eq!(f.store.read(sql::meta).unwrap().log_epoch, 3);
}

#[test]
fn every_restore_boundary_resumes_once_including_both_sides_of_marker_clear() {
    let declared: std::collections::BTreeSet<_> = include_str!("../hooks.rs")
        .split("pub enum Boundary {")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|s| s.starts_with("Restore"))
        .map(|s| s.trim_end_matches(',').to_owned())
        .collect();
    assert_eq!(
        declared,
        Boundary::RESTORE.iter().map(|b| format!("{b:?}")).collect()
    );
    for boundary in Boundary::RESTORE {
        let f = prepared();
        fs::remove_dir_all(&f.config(Arc::new(NoHooks)).directory).unwrap();
        let hooks = Cut::new();
        hooks.arm(boundary.clone());
        assert!(
            matches!(
                RunLog::<RunFact>::open_restoring(f.config(hooks), f.store.clone()),
                Err(Error::Cut)
            ),
            "{boundary:?}"
        );
        assert_eq!(
            f.store.read(sql::restore_pending).unwrap(),
            boundary != Boundary::RestoreFinished
        );
        let mut config = f.config(Arc::new(NoHooks));
        let Fixture {
            scratch,
            store,
            registry,
        } = f;
        drop(registry);
        drop(store);
        let store =
            Arc::new(Store::open(scratch.0.join("store.db"), Durability::default()).unwrap());
        config.lease_epoch = store.lease_epoch();
        let (log, _) = RunLog::<RunFact>::open_restoring(config.clone(), store.clone()).unwrap();
        let meta = store.read(sql::meta).unwrap();
        assert_eq!(meta.log_epoch, 4, "{boundary:?}");
        assert!(!store.read(sql::restore_pending).unwrap());
        assert!(store.read(|c| sql::history_lost(c, "live")).unwrap());
        drop(log);
        let (log, _) = RunLog::<RunFact>::open_restoring(config, store.clone()).unwrap();
        assert_eq!(store.read(sql::meta).unwrap(), meta);
        drop(log);
    }
}

#[test]
fn restore_advances_past_observed_run_ids_and_all_sqlite_segment_sequences() {
    let f = prepared();
    let (log, _) =
        RunLog::<RunFact>::open(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
    let cut = log
        .append(vec![RunFact {
            run_id: "run-store-700-0123456789abcdef".into(),
            facts: Facts::committed(),
        }])
        .unwrap();
    log.sync_through(cut).unwrap();
    drop(log);
    f.store
        .write(|tx| {
            sql::reserve_segment(tx, 2, 800)?;
            Ok(())
        })
        .unwrap();
    fs::copy(f.scratch.file(1), f.scratch.file(900)).unwrap();
    let (log, _) =
        RunLog::<RunFact>::open_restoring(f.config(Arc::new(NoHooks)), f.store.clone()).unwrap();
    assert_eq!(
        f.store
            .read(|c| Ok(
                c.query_row("SELECT value FROM meta WHERE key='next_run'", [], |r| {
                    r.get::<_, String>(0)
                })?
            ))
            .unwrap(),
        "701"
    );
    assert_eq!(
        f.store.read(sql::meta).unwrap().gc_floor.first_retained,
        901
    );
    drop(log);
}

#[test]
fn missing_reserved_spare_and_unregistered_predecessor_below_floor_are_normal_recovery() {
    let f = prepared();
    fs::remove_file(f.scratch.file(1)).unwrap();
    let (log, _) =
        RunLog::<RunFact>::open_restoring(f.config(Arc::new(NoHooks)), f.store.clone()).unwrap();
    assert_eq!(f.store.read(sql::meta).unwrap().log_epoch, 3);
    assert!(!f.store.read(|c| sql::history_lost(c, "live")).unwrap());
    drop(log);

    let f = Fixture::new();
    let log = RunLog::<RunFact>::create(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
    log.rotate().unwrap();
    assert_eq!(log.reclaim(BTreeMap::new()).unwrap(), 1);
    assert!(!f.scratch.file(0).exists());
    drop(log);
    let (log, _) =
        RunLog::<RunFact>::open_restoring(f.config(Arc::new(NoHooks)), f.store.clone()).unwrap();
    assert_eq!(f.store.read(sql::meta).unwrap().log_epoch, 3);
    drop(log);
}

#[test]
fn known_position_beyond_the_old_spare_is_restore_not_silent_repair() {
    for known in [
        Lsn {
            sequence: 9,
            offset: HEADER_LEN + 10,
        },
        Lsn {
            sequence: 1,
            offset: HEADER_LEN + 10,
        },
    ] {
        let f = prepared();
        f.store
            .write(|tx| {
                sql::advance_log_hwm(
                    tx,
                    EpochLsn {
                        log_epoch: 3,
                        lsn: known,
                    },
                )
            })
            .unwrap();
        restore_once(&f);
        assert!(f.store.read(sql::meta).unwrap().gc_floor.first_retained > known.sequence);
    }
}

#[test]
fn header_only_spare_with_known_position_inside_active_prefix_is_not_restore() {
    let f = prepared();
    assert_eq!(fs::metadata(f.scratch.file(1)).unwrap().len(), HEADER_LEN);
    let (log, _) =
        RunLog::<RunFact>::open_restoring(f.config(Arc::new(NoHooks)), f.store.clone()).unwrap();
    assert_eq!(f.store.read(sql::meta).unwrap().log_epoch, 3);
    assert!(!f.store.read(sql::restore_pending).unwrap());
    assert!(!f.store.read(|c| sql::history_lost(c, "live")).unwrap());
    drop(log);
}

#[test]
fn foreign_epoch_directory_reinitializes_exactly_once_even_if_old_epoch_remains() {
    let f = prepared();
    let root = f.config(Arc::new(NoHooks)).directory;
    fs::create_dir(root.join("e4")).unwrap();
    fs::copy(f.scratch.file(1), root.join("e4/0000000000000030.seg")).unwrap();
    restore_once(&f);
    assert!(f.store.read(sql::meta).unwrap().gc_floor.first_retained > 0x30);
}
