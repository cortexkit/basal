//! Trigger admission: one run per trigger, deduplicated, and refused for
//! good once the run is pruned.

use basal_proto::JsonText;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::error::{CoreError, Result};
use crate::ids::code_hash;
use crate::runs;
use crate::store::now_ms;

/// A trigger offered for admission, with the approved code it runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerSpec {
    pub flow_id: String,
    /// Unique per flow: an event id, or a schedule fire's due time.
    pub trigger_id: String,
    /// The trigger payload the script sees.
    pub trigger: JsonText,
    /// The approved script, snapshotted into the run.
    pub script: String,
    /// The approved manifest bytes, snapshotted into the run.
    pub manifest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// A new run was created.
    Admitted { run_id: String },
    /// The trigger was admitted before; this is its run.
    Duplicate { run_id: String },
    /// The trigger's run was pruned; its tombstone refuses it again.
    Tombstoned { run_id: String },
    /// Admission is stopped (`drain`).
    Draining,
}

impl Admission {
    pub fn run_id(&self) -> Option<&str> {
        match self {
            Self::Admitted { run_id }
            | Self::Duplicate { run_id }
            | Self::Tombstoned { run_id } => Some(run_id),
            Self::Draining => None,
        }
    }
}

/// The tombstone key of a trigger.
pub fn trigger_key(flow_id: &str, trigger_id: &str) -> String {
    format!("{}:{flow_id}\u{0}{trigger_id}", flow_id.len())
}

fn draining(tx: &Transaction) -> Result<bool> {
    let v: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key = 'draining'", [], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(v.as_deref() == Some("1"))
}

fn next_run_id(tx: &Transaction, store_id: &str) -> Result<String> {
    let current: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key = 'next_run'", [], |r| {
            r.get(0)
        })
        .optional()?;
    let n: u64 = match current {
        Some(v) => v
            .parse()
            .map_err(|_| CoreError::Corrupt(format!("run counter {v:?}")))?,
        None => 1,
    };
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('next_run', ?1) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [(n + 1).to_string()],
    )?;
    Ok(format!("run-{store_id}-{n}"))
}

fn insert_run(
    tx: &Transaction,
    store_id: &str,
    spec: &TriggerSpec,
    attempt: u32,
) -> Result<String> {
    let run_id = next_run_id(tx, store_id)?;
    let now = now_ms();
    let hash = code_hash(&spec.script, &spec.manifest);
    tx.execute(
        "INSERT INTO runs (run_id, flow_id, trigger_id, attempt, trigger, script, manifest, \
         code_hash, state, admitted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9)",
        params![
            run_id,
            spec.flow_id,
            spec.trigger_id,
            attempt,
            spec.trigger.as_str(),
            spec.script,
            spec.manifest,
            hash.as_slice(),
            now
        ],
    )?;
    tx.execute(
        "INSERT INTO trigger_inbox (flow_id, trigger_id, attempt, run_id, admitted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![spec.flow_id, spec.trigger_id, attempt, run_id, now],
    )?;
    Ok(run_id)
}

/// Admits a trigger once. The same trigger offered again gets its existing
/// run; after its run was pruned, its tombstone refuses it.
pub fn admit(tx: &Transaction, store_id: &str, spec: &TriggerSpec) -> Result<Admission> {
    let tomb: Option<String> = tx
        .query_row(
            "SELECT run_id FROM tombstones WHERE kind = 'trigger' AND key = ?1",
            [trigger_key(&spec.flow_id, &spec.trigger_id)],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(run_id) = tomb {
        return Ok(Admission::Tombstoned { run_id });
    }
    let existing: Option<String> = tx
        .query_row(
            "SELECT run_id FROM trigger_inbox WHERE flow_id = ?1 AND trigger_id = ?2 \
             ORDER BY attempt LIMIT 1",
            params![spec.flow_id, spec.trigger_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(run_id) = existing {
        return Ok(Admission::Duplicate { run_id });
    }
    if draining(tx)? {
        return Ok(Admission::Draining);
    }
    Ok(Admission::Admitted {
        run_id: insert_run(tx, store_id, spec, 1)?,
    })
}

/// Admits the run's trigger again as new work: a new run with a new id, so
/// every call gets a new idempotency key. Only an explicit retrigger does
/// this; nothing in recovery ever does.
pub fn retrigger(tx: &Transaction, store_id: &str, run_id: &str) -> Result<Admission> {
    if draining(tx)? {
        return Ok(Admission::Draining);
    }
    let run = runs::load(tx, run_id)?;
    let attempt: i64 = tx.query_row(
        "SELECT COALESCE(MAX(attempt), 0) + 1 FROM trigger_inbox WHERE flow_id = ?1 AND trigger_id = ?2",
        params![run.flow_id, run.trigger_id],
        |r| r.get(0),
    )?;
    let attempt =
        u32::try_from(attempt).map_err(|_| CoreError::Corrupt(format!("attempt {attempt}")))?;
    let spec = TriggerSpec {
        flow_id: run.flow_id,
        trigger_id: run.trigger_id,
        trigger: run.trigger,
        script: run.script,
        manifest: run.manifest,
    };
    Ok(Admission::Admitted {
        run_id: insert_run(tx, store_id, &spec, attempt)?,
    })
}

pub fn set_draining(tx: &Transaction, on: bool) -> Result<()> {
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('draining', ?1) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [if on { "1" } else { "0" }],
    )?;
    Ok(())
}
