use super::*;

struct Missing;
impl Spawn for Missing {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError> {
        Err(SpawnError::Exec("missing worker".into()))
    }
}

fn pool() -> Pool {
    let mut config = PoolConfig::new("missing", WorkerLaunch::Plain);
    config.warm_spares = 0;
    config.max_workers = 2;
    Pool::new(config, Arc::new(Missing), Clock::manual(0), Arc::default())
}

#[test]
fn handout_history_keeps_a_bounded_recent_tail() {
    let pool = pool();
    for worker in 0..2048 {
        pool.lock().record_handout(Handout {
            worker,
            pid: 1,
            binding: Binding::Flow("f".into()),
            source: Source::Bound,
        });
    }
    let history = pool.handouts();
    assert_eq!(history.len(), 1024);
    assert_eq!(history.first().unwrap().worker, 1024);
    assert_eq!(history.last().unwrap().worker, 2047);
}

#[test]
fn replacement_debt_never_exceeds_pool_capacity() {
    let pool = pool();
    let mut state = pool.lock();
    for _ in 0..32 {
        state.live = 1;
        pool.shared.lost(&mut state, true);
    }
    assert_eq!(state.owed_respawns, 2);
}

#[test]
fn spawn_backoff_is_exponential_capped_and_visible() {
    let pool = pool();
    let clock = &pool.shared.clock;
    let mut at = 0;
    for delay in [250, 500, 1000, 2000, 4000, 8000, 16000, 30000, 30000] {
        clock.set(at);
        assert!(pool.acquire(Binding::Flow("f".into())).is_err());
        let health = pool.stats();
        assert!(health.spawn_error.unwrap().contains("missing worker"));
        assert_eq!(health.spawn_retry_in_ms, delay);
        assert!(matches!(
            pool.acquire(Binding::Flow("f".into())),
            Err(PoolError::Backoff { .. })
        ));
        at += delay;
    }
}

#[test]
fn spawn_failure_notifies_the_engine_and_arms_the_pool_retry() {
    let pool = pool();
    let (tx, rx) = std::sync::mpsc::channel();
    pool.on_change(Arc::new(move || {
        tx.send(()).unwrap();
    }));
    assert!(pool.acquire(Binding::Flow("flow".into())).is_err());
    rx.recv_timeout(Duration::from_secs(60)).unwrap();
    assert_eq!(pool.next_maintenance_at(), Some(250));
    pool.shared.clock.set(250);
    assert_eq!(
        pool.next_maintenance_at(),
        None,
        "an unused expired retry cannot spin the loop"
    );
}

/// On Windows a fatal invalid-handle or access-violation status is a
/// confinement fault and is published as `fatal_status_deaths`; the
/// worker's own startup refusal (exit 70) and a kill are other crashes.
#[cfg(windows)]
#[test]
fn a_fatal_status_exit_counts_as_a_confinement_fault_and_exit_70_does_not() {
    use std::os::windows::process::ExitStatusExt;
    let pool = pool();
    let deaths = |pool: &Pool| pool.worker_confinement()["fatal_status_deaths"].clone();
    assert_eq!(deaths(&pool), 0);
    let mut state = pool.lock();
    state.live = 4;
    pool.shared
        .crashed(&mut state, Ok(ExitStatus::from_raw(0xc000_0008)));
    drop(state);
    assert_eq!(deaths(&pool), 1);
    let mut state = pool.lock();
    for code in [70, basal_launch::KILL_EXIT_CODE, 0] {
        pool.shared
            .crashed(&mut state, Ok(ExitStatus::from_raw(code)));
    }
    pool.shared
        .crashed(&mut state, Ok(ExitStatus::from_raw(0xc000_0005)));
    drop(state);
    assert_eq!(deaths(&pool), 2);
    assert!(pool.worker_confinement().get("sigsys_deaths").is_none());
    assert_eq!(
        pool.shared.metrics.workers_crashed.load(Ordering::Relaxed),
        5
    );
}
