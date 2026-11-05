//! Activation ownership tokens, immutable snapshots and log-backed projections.
use crate::{CoreError, Result, model::Run};
use rusqlite::{Connection, Transaction, params};

/// The identity of one activation of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub run_id: String,
    pub owner: String,
    pub generation: u64,
}

impl Lease {
    pub fn generation_i64(&self) -> Result<i64> {
        i64::try_from(self.generation)
            .map_err(|_| CoreError::Corrupt(format!("generation {}", self.generation)))
    }

    pub fn lost(&self) -> CoreError {
        CoreError::OwnerLost {
            run_id: self.run_id.clone(),
            generation: self.generation,
        }
    }
}

/// Store the immutable code admitted with a run, sharing it with other runs of
/// the same flow version. Legacy unversioned runs use version zero.
pub fn snapshot(
    tx: &Transaction,
    flow_id: &str,
    version: Option<u32>,
    code_hash: &[u8],
    script: &str,
    manifest: &str,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO run_snapshots(flow_id,flow_version,code_hash,script,manifest) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(flow_id,flow_version,code_hash) DO NOTHING",
        params![flow_id,version.unwrap_or(0),code_hash,script,manifest],
    )?;
    let (id, same): (i64, bool) = tx.query_row(
        "SELECT snapshot_id,script=?4 AND manifest=?5 FROM run_snapshots WHERE flow_id=?1 AND flow_version=?2 AND code_hash=?3",
        params![flow_id,version.unwrap_or(0),code_hash,script,manifest],
        |r| Ok((r.get(0)?,r.get(1)?)),
    )?;
    if !same {
        return Err(CoreError::Corrupt(
            "conflicting run snapshot identity".into(),
        ));
    }
    Ok(id)
}

/// Log-backed replacement for the `runs_with_snapshots` reader. The caller
/// copies the actor before opening this connection; finished summaries take
/// precedence if closing committed after that copy.
pub(crate) fn load_staged(
    conn: &Connection,
    run_id: &str,
    state: Option<&crate::run_records::State>,
    active: bool,
    owner: &str,
) -> Result<Run> {
    crate::run_readers::load(conn, run_id, state, active, owner)
}
