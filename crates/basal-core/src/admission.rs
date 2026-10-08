//! Trigger admission: one run per trigger, deduplicated, and refused for
//! good once the run is pruned.
//!
//! A trigger is admitted only under its flow's approved version: the run
//! snapshots that version's script and manifest, and keeps them however
//! many versions are approved after it. A disabled flow admits nothing, and
//! each admission counts against the flow's run rate limit.

use basal_proto::JsonText;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::error::{CoreError, Result};
use crate::ids::code_hash;
use crate::install;
use crate::manifest::Manifest;
use crate::rate::{self, RateLimits, Take};
use crate::runs;

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
}

impl Admission {
    pub fn run_id(&self) -> Option<&str> {
        match self {
            Self::Admitted { run_id }
            | Self::Duplicate { run_id }
            | Self::Tombstoned { run_id } => Some(run_id),
            Self::Draining | Self::NotApproved | Self::Disabled | Self::RateLimited => None,
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

/// A new run id and the run's place in admission order.
fn next_run_id(tx: &Transaction, store_id: &str) -> Result<(String, i64)> {
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
    let seq = i64::try_from(n).map_err(|_| CoreError::Corrupt(format!("run counter {n}")))?;
    Ok((format!("run-{store_id}-{n}"), seq))
}

/// What the gate found the trigger may be admitted under.
struct Grant {
    version: u32,
    deadline_ms: i64,
    code_hash: [u8; 32],
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
    let manifest =
        Manifest::parse(text).map_err(|e| CoreError::Corrupt(format!("approved manifest: {e}")))?;
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

/// Everything that can stop an admission besides deduplication: drain, a
/// disabled flow, code that is not the approved version, and the run rate
/// limit (checked last, so a refusal for any other reason does not count
/// towards saturation).
fn gate(
    tx: &Transaction,
    spec: &TriggerSpec,
    ctx: &AdmitContext,
    supplied: Option<&install::Approved>,
) -> Result<std::result::Result<Grant, Admission>> {
    if draining(tx)? {
        return Ok(Err(Admission::Draining));
    }
    if crate::packages::removed(tx, &spec.flow_id)? {
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
    if let Take::Refused { .. } =
        rate::take(tx, &spec.flow_id, rate::Kind::Run, ctx.now_ms, &ctx.rate)?
    {
        return Ok(Err(Admission::RateLimited));
    }
    Ok(Ok(Grant {
        version: approved.version,
        deadline_ms,
        code_hash: approved.code_hash,
    }))
}

fn insert_run(
    tx: &Transaction,
    store_id: &str,
    spec: &TriggerSpec,
    attempt: u32,
    grant: &Grant,
    // The runtime's clock, like run deadlines and rate windows, so a run's
    // age (how long it has waited since admission) is measured on one clock.
    now: i64,
) -> Result<String> {
    let (run_id, seq) = next_run_id(tx, store_id)?;
    tx.execute(
        "INSERT INTO runs (run_id, flow_id, trigger_id, attempt, trigger, script, manifest, \
         code_hash, state, admitted_at, flow_version, admit_seq, deadline_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9, ?10, ?11, ?12)",
        params![
            run_id,
            spec.flow_id,
            spec.trigger_id,
            attempt,
            spec.trigger.as_str(),
            spec.script,
            spec.manifest,
            grant.code_hash.as_slice(),
            now,
            i64::from(grant.version),
            seq,
            grant.deadline_ms
        ],
    )?;
    tx.execute(
        "UPDATE runs SET self_input=(SELECT json_object('agent_id',owner) FROM flows WHERE flow_id=?2 AND package IS NOT NULL) WHERE run_id=?1",
        params![run_id, spec.flow_id],
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
pub fn admit(
    tx: &Transaction,
    store_id: &str,
    spec: &TriggerSpec,
    ctx: &AdmitContext,
) -> Result<Admission> {
    admit_with_approved(tx, store_id, spec, ctx, None)
}

fn admit_with_approved(
    tx: &Transaction,
    store_id: &str,
    spec: &TriggerSpec,
    ctx: &AdmitContext,
    approved: Option<&install::Approved>,
) -> Result<Admission> {
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
    let grant = match gate(tx, spec, ctx, approved)? {
        Ok(grant) => grant,
        Err(refused) => return Ok(refused),
    };
    Ok(Admission::Admitted {
        run_id: insert_run(tx, store_id, spec, 1, &grant, ctx.now_ms)?,
    })
}

/// Admits a trigger under the flow's version approved now: the one rule
/// every trigger source follows, so a trigger always runs the code that is
/// approved and enabled at the moment it is admitted. A flow with no
/// approved version is offered an empty spec, which still goes through
/// deduplication and tombstones and is then refused as
/// [`Admission::NotApproved`].
pub fn admit_current(
    tx: &Transaction,
    store_id: &str,
    flow_id: &str,
    trigger_id: &str,
    trigger: JsonText,
    ctx: &AdmitContext,
) -> Result<Admission> {
    let approved = install::approved(tx, flow_id)?;
    let (script, manifest) = match &approved {
        Some(approved) => (approved.script.clone(), approved.manifest.clone()),
        None => (String::new(), String::new()),
    };
    let spec = TriggerSpec {
        flow_id: flow_id.to_owned(),
        trigger_id: trigger_id.to_owned(),
        trigger,
        script,
        manifest,
    };
    admit_with_approved(tx, store_id, &spec, ctx, approved.as_ref())
}

/// Admits the run's trigger again as new work: a new run with a new id, so
/// every call gets a new idempotency key. Only an explicit retrigger does
/// this; nothing in recovery ever does. The new run is newly admitted
/// work, so it runs the flow's approved version as of now.
pub fn retrigger(
    tx: &Transaction,
    store_id: &str,
    run_id: &str,
    ctx: &AdmitContext,
) -> Result<Admission> {
    let run = runs::load(tx, run_id)?;
    let Some(approved) = install::approved(tx, &run.flow_id)? else {
        return Ok(Admission::NotApproved);
    };
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
        script: approved.script,
        manifest: approved.manifest,
    };
    let grant = match gate(tx, &spec, ctx, None)? {
        Ok(grant) => grant,
        Err(refused) => return Ok(refused),
    };
    Ok(Admission::Admitted {
        run_id: insert_run(tx, store_id, &spec, attempt, &grant, ctx.now_ms)?,
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
