//! Trigger admission: one run per trigger, deduplicated, and refused for
//! good once the run is pruned.
//!
//! A trigger is admitted only under its flow's approved version: the run
//! snapshots that version's script and manifest, and keeps them however
//! many versions are approved after it. A disabled flow admits nothing, and
//! each admission counts against the flow's run rate limit.

use basal_proto::JsonText;
use rusqlite::{OptionalExtension, Transaction};

use crate::error::{CoreError, Result};
use crate::ids::code_hash;
use crate::install;
use crate::manifest::Manifest;
use crate::rate::{RateLimits, Take};

/// What admission needs besides the trigger: the time on the runtime's
/// clock, the rate limits, and the deadline of a run whose manifest sets
/// none.
#[derive(Debug, Clone, Copy)]
pub struct AdmitContext {
    pub now_ms: i64,
    pub rate: RateLimits,
    pub default_deadline_ms: i64,
}

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
    /// The script and manifest are not the flow's approved version.
    NotApproved,
    /// The flow is disabled.
    Disabled,
    /// The flow's run rate limit for the current window is reached; the
    /// trigger was not admitted. Callers may offer it again later, but the
    /// scheduler records it as dropped and does not retry that scheduled fire.
    RateLimited,
    /// An epoch change waits for every previously admitted run to finish.
    StateBusy,
}

impl Admission {
    pub fn run_id(&self) -> Option<&str> {
        match self {
            Self::Admitted { run_id }
            | Self::Duplicate { run_id }
            | Self::Tombstoned { run_id } => Some(run_id),
            Self::Draining
            | Self::NotApproved
            | Self::Disabled
            | Self::RateLimited
            | Self::StateBusy => None,
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

/// What the gate found the trigger may be admitted under.
pub(crate) struct Grant {
    pub(crate) version: u32,
    pub(crate) deadline_ms: i64,
    pub(crate) code_hash: [u8; 32],
}

fn approved_deadline(text: &str, default_ms: i64) -> Result<i64> {
    type Cache = std::collections::VecDeque<(String, i64, i64)>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Cache>> = std::sync::OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some((_, _, deadline)) = cache
        .iter()
        .find(|(manifest, default, _)| manifest == text && *default == default_ms)
    {
        return Ok(*deadline);
    }
    let manifest = Manifest::parse_stored(text)
        .map_err(|e| CoreError::Corrupt(format!("approved manifest: {e}")))?;
    let deadline = manifest.deadline_ms(default_ms);
    // Exact bytes validated by this process need not be decoded and checked
    // again on every trigger. Invalid bytes never enter the bounded cache.
    const MAX_VALIDATED_MANIFESTS: usize = 64;
    if cache.len() == MAX_VALIDATED_MANIFESTS {
        cache.pop_front();
    }
    cache.push_back((text.to_owned(), default_ms, deadline));
    Ok(deadline)
}

/// Both storage models share approval and rate policy; adapters select the
/// slot lookup and the admission counter's owner-notice storage.
pub(crate) fn gate_with_slots(
    tx: &Transaction,
    spec: &TriggerSpec,
    ctx: &AdmitContext,
    supplied: Option<&install::Approved>,
    holds_slot: impl FnOnce() -> Result<bool>,
    count_admission: impl FnOnce() -> Result<Take>,
) -> Result<std::result::Result<Grant, Admission>> {
    if draining(tx)? {
        return Ok(Err(Admission::Draining));
    }
    if crate::packages::removed(tx, &spec.flow_id)?
        || crate::templates::expired(tx, &spec.flow_id, ctx.now_ms)?
    {
        return Ok(Err(Admission::NotApproved));
    }
    match install::flow(tx, &spec.flow_id)? {
        None => return Ok(Err(Admission::NotApproved)),
        Some(flow) if !flow.enabled => return Ok(Err(Admission::Disabled)),
        Some(_) => {}
    }
    let loaded;
    let approved = match supplied {
        Some(approved) => approved,
        None => {
            loaded = install::approved(tx, &spec.flow_id)?;
            let Some(approved) = loaded.as_ref() else {
                return Ok(Err(Admission::NotApproved));
            };
            approved
        }
    };
    if approved.code_hash != code_hash(&spec.script, &spec.manifest) {
        return Ok(Err(Admission::NotApproved));
    }
    let deadline_ms = approved_deadline(&spec.manifest, ctx.default_deadline_ms)?;
    let epoch = Manifest::parse_stored(&spec.manifest)
        .map_err(|e| CoreError::Corrupt(e.to_string()))?
        .state_epoch;
    if crate::state_epoch::recorded(tx, &spec.flow_id)? != epoch && holds_slot()? {
        return Ok(Err(Admission::StateBusy));
    }
    if let Take::Refused { .. } = count_admission()? {
        return Ok(Err(Admission::RateLimited));
    }
    Ok(Ok(Grant {
        version: approved.version,
        deadline_ms,
        code_hash: approved.code_hash,
    }))
}

pub fn set_draining(tx: &Transaction, on: bool) -> Result<()> {
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('draining', ?1) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [if on { "1" } else { "0" }],
    )?;
    Ok(())
}
