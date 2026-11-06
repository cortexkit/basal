//! Stop-the-world conversion of the SQLite execution model to the run log.
//!
//! Startup owns the store lease and calls this before attaching hosts or admitting
//! traffic. It runs once per store and leaves no switch behind: there is no
//! setting that keeps the old execution model. Deployment approval and a store
//! backup happen before the binary is placed. A failed post-check never permits
//! startup.

mod audit;
mod convert;
pub(crate) mod legacy;
mod pages;
#[cfg(test)]
pub(crate) mod tests;

use crate::{Boundary, CoreError, Hooks, Result, Step, Store};
use rusqlite::{OptionalExtension, Transaction};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Sweep,
    Create,
    FinishedRuns,
    LiveRuns,
    BrocaCalls,
    Drop,
    PageSize,
    Finish,
    Done,
}

impl Phase {
    pub const STEPS: [Self; 8] = [
        Self::Sweep,
        Self::Create,
        Self::FinishedRuns,
        Self::LiveRuns,
        Self::BrocaCalls,
        Self::Drop,
        Self::PageSize,
        Self::Finish,
    ];
    pub fn next(self) -> Self {
        match self {
            Self::Sweep => Self::Create,
            Self::Create => Self::FinishedRuns,
            Self::FinishedRuns => Self::LiveRuns,
            Self::LiveRuns => Self::BrocaCalls,
            Self::BrocaCalls => Self::Drop,
            Self::Drop => Self::PageSize,
            Self::PageSize => Self::Finish,
            Self::Finish | Self::Done => Self::Done,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sweep => "sweep",
            Self::Create => "create",
            Self::FinishedRuns => "finished_runs",
            Self::LiveRuns => "live_runs",
            Self::BrocaCalls => "broca_calls",
            Self::Drop => "drop",
            Self::PageSize => "page_size",
            Self::Finish => "finish",
            Self::Done => "done",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        Self::STEPS
            .into_iter()
            .chain([Self::Done])
            .find(|p| p.as_str() == value)
            .ok_or_else(|| CoreError::Corrupt(format!("unknown migration_phase {value:?}")))
    }
}

pub fn phase(store: &Store) -> Result<Phase> {
    store.read(phase_from_connection)
}
fn phase_from_connection(c: &rusqlite::Connection) -> Result<Phase> {
    let value: Option<String> = c
        .query_row(
            "SELECT value FROM meta WHERE key='migration_phase'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    value
        .as_deref()
        .map(Phase::parse)
        .unwrap_or(Ok(Phase::Sweep))
}

pub struct Migration {
    store: Arc<Store>,
    directory: PathBuf,
    hooks: Arc<dyn Hooks>,
}

impl Migration {
    /// The caller must hold the store lease and must not have constructed a
    /// runtime. Production calls this at startup; rehearsals use isolated copies.
    pub fn new(store: Arc<Store>, directory: impl AsRef<Path>, hooks: Arc<dyn Hooks>) -> Self {
        Self {
            store,
            directory: directory.as_ref().to_owned(),
            hooks,
        }
    }
    pub fn phase(&self) -> Result<Phase> {
        phase(&self.store)
    }
    pub fn run(&self) -> Result<()> {
        // A newly created, still-empty store has no legacy execution state to
        // sweep or convert. Create the run-log tables (`schema_run_log.sql`) in
        // one transaction; the ordinary resumable phases then create the log
        // directory and finish the audits.
        if self.phase()? == Phase::Sweep
            && self.store.is_fresh()
            && self.store.read(|c| {
                Ok(c.query_row(
                    "SELECT NOT EXISTS(SELECT 1 FROM runs) AND NOT EXISTS(SELECT 1 FROM flows)",
                    [],
                    |r| r.get::<_, bool>(0),
                )?)
            })?
        {
            let started = Instant::now();
            self.store.write(|tx| {
                audit::capture(tx)?;
                convert::create(tx, &self.store)?;
                convert::finished(tx)?;
                self.at(Phase::Create, false)?;
                set_phase(tx, Phase::LiveRuns)
            })?;
            tracing::info!(target:"migration", phase="create", duration_ms=started.elapsed().as_millis() as u64, "fresh run-log schema initialized");
            self.at(Phase::Create, true)?;
        }
        while self.phase()? != Phase::Done {
            self.advance()?;
        }
        Ok(())
    }
    pub fn advance(&self) -> Result<()> {
        let phase = self.phase()?;
        if phase == Phase::Done {
            return Ok(());
        }
        let started = Instant::now();
        if phase == Phase::Sweep {
            if self.directory.exists() {
                return Err(CoreError::Corrupt(
                    "run-log directory exists before migration initialization".into(),
                ));
            }
            self.store.write(|tx| set_phase(tx, Phase::Sweep))?;
            legacy::sweep(&self.store)?;
        }
        let live = if phase == Phase::LiveRuns {
            Some(convert::live(&self.store, &self.directory)?)
        } else {
            None
        };
        let snapshots = if matches!(phase, Phase::PageSize | Phase::Finish) {
            convert::snapshots(&self.store, &self.directory)?
        } else {
            Vec::new()
        };
        if phase == Phase::PageSize {
            self.store.read(|c| audit::check(c, &snapshots))?;
            pages::set_pages(&self.store, self.hooks.as_ref())?;
        }
        self.store.write(|tx| {
            match phase {
                Phase::Sweep => audit::capture(tx)?,
                Phase::Create => convert::create(tx, &self.store)?,
                Phase::FinishedRuns => convert::finished(tx)?,
                Phase::LiveRuns => live
                    .as_ref()
                    .expect("prepared migration live image")
                    .apply(tx)?,
                Phase::BrocaCalls => convert::broca(tx)?,
                Phase::Drop => convert::drop_legacy(tx)?,
                Phase::PageSize => {
                    pages::verify(tx)?;
                    audit::check(tx, &snapshots)?;
                }
                Phase::Finish => {
                    audit::check(tx, &snapshots)?;
                    pages::verify(tx)?;
                    tx.execute("DELETE FROM meta WHERE key='migration_audit'", [])?;
                }
                Phase::Done => unreachable!(),
            }
            self.at(phase, false)?;
            set_phase(tx, phase.next())
        })?;
        tracing::info!(target:"migration", phase=phase.as_str(), duration_ms=started.elapsed().as_millis() as u64, "run-log migration phase complete");
        self.at(phase, true)
    }
    fn at(&self, phase: Phase, committed: bool) -> Result<()> {
        let boundary = match (phase, committed) {
            (Phase::Sweep, false) => Boundary::MigrationSweepReady,
            (Phase::Sweep, true) => Boundary::MigrationSweepCommitted,
            (Phase::Create, false) => Boundary::MigrationCreateReady,
            (Phase::Create, true) => Boundary::MigrationCreateCommitted,
            (Phase::FinishedRuns, false) => Boundary::MigrationFinishedReady,
            (Phase::FinishedRuns, true) => Boundary::MigrationFinishedCommitted,
            (Phase::LiveRuns, false) => Boundary::MigrationLiveReady,
            (Phase::LiveRuns, true) => Boundary::MigrationLiveCommitted,
            (Phase::BrocaCalls, false) => Boundary::MigrationBrocaReady,
            (Phase::BrocaCalls, true) => Boundary::MigrationBrocaCommitted,
            (Phase::Drop, false) => Boundary::MigrationDropReady,
            (Phase::Drop, true) => Boundary::MigrationDropCommitted,
            (Phase::PageSize, false) => Boundary::MigrationPagesReady,
            (Phase::PageSize, true) => Boundary::MigrationPagesCommitted,
            (Phase::Finish, false) => Boundary::MigrationFinishReady,
            (Phase::Finish, true) => Boundary::MigrationFinishCommitted,
            (Phase::Done, _) => return Ok(()),
        };
        if self.hooks.at("migration", &boundary) == Step::Crash {
            self.store.cut();
            return Err(CoreError::Cut);
        }
        Ok(())
    }
}

/// Fresh stores select the target page size before creating basal's tables.
/// The backend may already own internal lease pages, so changing their size
/// still requires the same lease-held SQLite maintenance operation.
pub(crate) fn initialize_pages(store: &Store) -> Result<()> {
    pages::set_pages(store, &crate::NoHooks)
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn initialize_broca_fixture(tx: &Transaction<'_>) -> Result<()> {
    convert::rebuild_broca(tx)
}

fn set_phase(tx: &Transaction<'_>, phase: Phase) -> Result<()> {
    tx.execute("INSERT INTO meta(key,value) VALUES('migration_phase',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[phase.as_str()])?;
    Ok(())
}

/// Broca's union ownership cannot be represented by a single SQLite FK. Use
/// this same check in migration, retention and crash/restore fixtures.
pub fn check_broca_ownership(c: &rusqlite::Connection) -> Result<()> {
    let orphan: Option<String> = c.query_row("SELECT b.run_id FROM broca_calls b WHERE NOT EXISTS(SELECT 1 FROM live_runs l WHERE l.run_id=b.run_id) AND NOT EXISTS(SELECT 1 FROM run_history h WHERE h.run_id=b.run_id) LIMIT 1",[],|r|r.get(0)).optional()?;
    if let Some(id) = orphan {
        return Err(CoreError::Corrupt(format!("broca_calls: orphan run {id}")));
    }
    Ok(())
}
