//! Installing, approving and disabling flows.
//!
//! Install validates a flow's manifest on its own and against the catalog,
//! then records the version with the BLAKE3 hash of its exact script and
//! manifest bytes. Approval binds to that hash: the approved version is
//! what new triggers are admitted under, and a run keeps the version it was
//! admitted with even after another is approved. The consent card and the
//! capture-only dry run that come before approval are not here.
//!
//! A disabled flow admits nothing, and every run of it, resumed ones
//! included, is refused any new call outside its VM. Calls it already
//! dispatched keep their obligations: their outcomes are still recorded and
//! their token reservations still settled. The operator can disable any
//! flow, and an agent can disable a flow it owns; each disable tells the
//! owner through the outbox.

use basal_host::{Catalog, EventBody, OpKind};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::authorize::ShellDenylist;
use crate::error::{CoreError, Result};
use crate::ids::code_hash;
use crate::manifest::{Manifest, ManifestError, OpRef};
use crate::runtime::Runtime;
use crate::schedule::{self, Approval, ScheduledFlow, SchedulerConfig};

/// A version offered for install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    pub script: String,
    pub manifest: String,
    /// The agent (or the operator) that wrote the flow; it owns the flow
    /// once this version is approved.
    pub author: String,
    /// The operator accepts a flow bound to a module's events that holds a
    /// mutation on the same module without cause echo; loop protection for
    /// it is then rate limiting only.
    pub loop_override: bool,
}

/// Why an install was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    SelfRequiresPackage,
    FlowScopeUnsupported {
        module: String,
        op: String,
    },
    Manifest(ManifestError),
    ScriptTooLarge {
        bytes: usize,
        cap: usize,
    },
    UnknownEvent {
        module: String,
        name: String,
        version: u32,
    },
    /// An event's `resolve_op` is not a query, so reading a body through it
    /// could change something.
    ResolveOpNotQuery {
        module: String,
        op: String,
    },
    UnknownOp {
        module: String,
        op: String,
    },
    /// The module did not say whether the op only reads.
    UnmarkedOp {
        module: String,
        op: String,
    },
    ShellCapable {
        module: String,
        op: String,
    },
    UnknownAgent {
        field: &'static str,
        agent: String,
    },
    /// An agent-owned flow may act only for its owner, not another agent.
    ForeignAgentTarget {
        field: &'static str,
        agent: String,
        author: String,
    },
    /// An `fs` or `git` root that does not exist (after `~` expansion) when
    /// the flow is installed.
    MissingRoot {
        field: &'static str,
        root: String,
    },
    /// The flow is bound to `module`'s events and holds a mutation on
    /// `module` that does not echo causes, without the operator's override.
    LoopRule {
        module: String,
        op: String,
    },
    /// This version exists with different code. Any edit is a new version.
    VersionExists {
        flow_id: String,
        version: u32,
    },
    /// A version at or below the approved one.
    StaleVersion {
        flow_id: String,
        version: u32,
        approved: u32,
    },
    NoSuchVersion {
        flow_id: String,
        version: u32,
    },
    /// The approval names a different code hash than the installed version.
    HashMismatch {
        flow_id: String,
        version: u32,
    },
    /// Core no longer stands behind this version (see [`revoke`]); it can
    /// never be approved again. Any edit is a new version.
    Revoked {
        flow_id: String,
        version: u32,
    },
    /// An agent tried to enable a flow whose current disable it did not
    /// make itself (the operator's, or the runtime's auto-disable).
    NotYourDisable {
        flow_id: String,
        disabled_by: String,
    },
    /// An agent tried to disable a flow it does not own.
    NotOwner {
        flow_id: String,
        agent: String,
    },
    NoSuchFlow(String),
    /// A decision named a consent card basal never raised.
    NoSuchCard(String),
    Store(CoreError),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for InstallError {}

impl From<CoreError> for InstallError {
    fn from(e: CoreError) -> Self {
        Self::Store(e)
    }
}

impl From<rusqlite::Error> for InstallError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.into())
    }
}

/// Something the card must show, which does not refuse the install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// The flow reads private text and holds an op that sends data out.
    PrivateTextWithOutboundOp { module: String, op: String },
    /// The operator overrode the loop install rule for this op; loop
    /// protection for the flow is rate limiting only.
    LoopOverride { module: String, op: String },
    /// The flow reads private text and may fetch from this host, which can
    /// carry that text out in a URL, a header or a body.
    PrivateTextWithNetFetch { host: String },
}

/// A recorded version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub flow_id: String,
    pub version: u32,
    pub code_hash: [u8; 32],
    pub warnings: Vec<Warning>,
    /// False when the identical version was already installed.
    pub new: bool,
}

/// Validates a parsed manifest against its author, the catalog and the shell denylist.
pub fn validate(
    manifest: &Manifest,
    author: &str,
    catalog: &dyn Catalog,
    denylist: &ShellDenylist,
    loop_override: bool,
) -> std::result::Result<Vec<Warning>, InstallError> {
    if manifest.agents().iter().any(|(_, agent)| *agent == "$self") {
        return Err(InstallError::SelfRequiresPackage);
    }
    validate_inner(manifest, author, catalog, denylist, loop_override, false)
}

pub(crate) fn validate_inner(
    manifest: &Manifest,
    author: &str,
    catalog: &dyn Catalog,
    denylist: &ShellDenylist,
    loop_override: bool,
    package: bool,
) -> std::result::Result<Vec<Warning>, InstallError> {
    let mut warnings = Vec::new();
    let mut trigger_modules: Vec<&str> = Vec::new();
    if let Some(events) = &manifest.trigger.events {
        for e in events {
            let Some(decl) = catalog.event(&e.module, &e.name, e.version) else {
                return Err(InstallError::UnknownEvent {
                    module: e.module.clone(),
                    name: e.name.clone(),
                    version: e.version,
                });
            };
            if let EventBody::Resolved { resolve_op } = &decl.body {
                let kind = catalog.op(&e.module, resolve_op).and_then(|d| d.kind);
                if kind != Some(OpKind::Query) {
                    return Err(InstallError::ResolveOpNotQuery {
                        module: e.module.clone(),
                        op: resolve_op.clone(),
                    });
                }
            }
            trigger_modules.push(&e.module);
        }
    }
    let reads_text = manifest.facts.as_ref().is_some_and(|f| f.text);
    for OpRef { module, op } in &manifest.ops {
        if module == basal_host::subc_catalog::CORE && !catalog.supports_flow_scopes(module) {
            return Err(InstallError::FlowScopeUnsupported {
                module: module.clone(),
                op: op.clone(),
            });
        }
        let pair = || (module.clone(), op.clone());
        if denylist.contains(module, op) {
            let (module, op) = pair();
            return Err(InstallError::ShellCapable { module, op });
        }
        let Some(decl) = catalog.op(module, op) else {
            let (module, op) = pair();
            return Err(InstallError::UnknownOp { module, op });
        };
        if decl.shell_capable {
            let (module, op) = pair();
            return Err(InstallError::ShellCapable { module, op });
        }
        let Some(kind) = decl.kind else {
            let (module, op) = pair();
            return Err(InstallError::UnmarkedOp { module, op });
        };
        if kind == OpKind::Mutate {
            if trigger_modules.contains(&module.as_str()) && !decl.cause_echo {
                if !loop_override {
                    let (module, op) = pair();
                    return Err(InstallError::LoopRule { module, op });
                }
                let (module, op) = pair();
                warnings.push(Warning::LoopOverride { module, op });
            }
            if reads_text {
                let (module, op) = pair();
                warnings.push(Warning::PrivateTextWithOutboundOp { module, op });
            }
        }
    }
    if reads_text && let Some(net) = &manifest.net {
        for f in &net.fetch {
            warnings.push(Warning::PrivateTextWithNetFetch {
                host: f.host.clone(),
            });
        }
    }
    if !package {
        for (field, agent) in manifest.agents() {
            // A flow installed by an agent is agent-owned: every agent field
            // in its manifest must name that same agent, so it can act only
            // for its owner. A flow installed by the operator or by an
            // unverified local caller is global and belongs to no agent; it
            // may name any agent the catalog knows.
            if author != crate::decisions::OPERATOR_ACTOR && author != "local:unverified" {
                let Some(target_id) = catalog.agent_id(agent) else {
                    return Err(InstallError::UnknownAgent {
                        field,
                        agent: agent.to_owned(),
                    });
                };
                // Manifests may use a display name, while the scope-stamped author
                // is a stable id. Compare resolved identities, not their spelling.
                if target_id != author {
                    return Err(InstallError::ForeignAgentTarget {
                        field,
                        agent: agent.to_owned(),
                        author: author.to_owned(),
                    });
                }
            } else if !catalog.agent_known(agent) {
                return Err(InstallError::UnknownAgent {
                    field,
                    agent: agent.to_owned(),
                });
            }
        }
    }
    for (field, root) in manifest.roots() {
        let exists = basal_host::builtins::expand_home(root).is_some_and(|p| p.exists());
        if !exists {
            return Err(InstallError::MissingRoot {
                field,
                root: root.to_owned(),
            });
        }
    }
    Ok(warnings)
}

fn version_i64(v: u32) -> i64 {
    i64::from(v)
}

/// Records a validated version. Installing the identical version again is
/// a no-op; the same version with other code, or a version not above the
/// approved one, is refused.
pub fn record(
    tx: &Transaction,
    manifest: &Manifest,
    request: &InstallRequest,
    warnings: Vec<Warning>,
    now_ms: i64,
) -> std::result::Result<Installed, InstallError> {
    let hash = code_hash(&request.script, &request.manifest);
    let flow_id = manifest.id.clone();
    let version = manifest.version;
    tx.execute(
        "INSERT OR IGNORE INTO flows (flow_id, created_at) VALUES (?1, ?2)",
        params![flow_id, now_ms],
    )?;
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT code_hash FROM installs WHERE flow_id = ?1 AND version = ?2",
            params![flow_id, version_i64(version)],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing.as_slice() == hash.as_slice() {
            return Ok(Installed {
                flow_id,
                version,
                code_hash: hash,
                warnings,
                new: false,
            });
        }
        return Err(InstallError::VersionExists { flow_id, version });
    }
    let approved: Option<i64> = tx.query_row(
        "SELECT approved_version FROM flows WHERE flow_id = ?1",
        [&flow_id],
        |r| r.get(0),
    )?;
    if let Some(approved) = approved
        && approved >= version_i64(version)
    {
        return Err(InstallError::StaleVersion {
            flow_id,
            version,
            approved: u32::try_from(approved)
                .map_err(|_| CoreError::Corrupt(format!("approved version {approved}")))?,
        });
    }
    tx.execute(
        "INSERT INTO installs (flow_id, version, code_hash, manifest, script, author, \
         loop_override, state, installed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'validated', ?8)",
        params![
            flow_id,
            version_i64(version),
            hash.as_slice(),
            request.manifest,
            request.script,
            request.author,
            i64::from(request.loop_override),
            now_ms
        ],
    )?;
    Ok(Installed {
        flow_id,
        version,
        code_hash: hash,
        warnings,
        new: true,
    })
}

/// Approves an installed version whose code hash is `code_hash`. The
/// previously approved version is superseded; runs already admitted under
/// it keep running it.
pub fn approve(
    tx: &Transaction,
    flow_id: &str,
    version: u32,
    code_hash: &[u8; 32],
    approval_ref: &str,
    now_ms: i64,
    schedule: &SchedulerConfig,
) -> std::result::Result<Option<Approval>, InstallError> {
    let row: Option<(Vec<u8>, String, String, Option<i64>)> = tx
        .query_row(
            "SELECT code_hash, state, author, revoked_at FROM installs \
             WHERE flow_id = ?1 AND version = ?2",
            params![flow_id, version_i64(version)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((hash, state, author, revoked_at)) = row else {
        return Err(InstallError::NoSuchVersion {
            flow_id: flow_id.to_owned(),
            version,
        });
    };
    if hash.as_slice() != code_hash.as_slice() {
        return Err(InstallError::HashMismatch {
            flow_id: flow_id.to_owned(),
            version,
        });
    }
    if revoked_at.is_some() {
        return Err(InstallError::Revoked {
            flow_id: flow_id.to_owned(),
            version,
        });
    }
    if state == "approved" {
        // A retried approval changes nothing, its schedule included.
        let has_schedule = schedule::table::load(tx, flow_id)?.is_some();
        return Ok(has_schedule.then_some(Approval::Unchanged));
    }
    let approved: Option<i64> = tx.query_row(
        "SELECT approved_version FROM flows WHERE flow_id = ?1",
        [flow_id],
        |r| r.get(0),
    )?;
    if state == "superseded" || approved.is_some_and(|a| a >= version_i64(version)) {
        return Err(InstallError::StaleVersion {
            flow_id: flow_id.to_owned(),
            version,
            approved: approved
                .and_then(|a| u32::try_from(a).ok())
                .unwrap_or(version),
        });
    }
    tx.execute(
        "UPDATE installs SET state = 'superseded' WHERE flow_id = ?1 AND state = 'approved'",
        [flow_id],
    )?;
    tx.execute(
        "UPDATE installs SET state = 'approved', approval_ref = ?3, approved_at = ?4 \
         WHERE flow_id = ?1 AND version = ?2",
        params![flow_id, version_i64(version), approval_ref, now_ms],
    )?;
    tx.execute(
        "UPDATE flows SET approved_version = ?2, owner = ?3 WHERE flow_id = ?1",
        params![flow_id, version_i64(version), author],
    )?;
    let approval = schedule_approved(tx, flow_id, version, now_ms, schedule)?;
    // When core revokes the approved version (see `revoke`), it disables the
    // flow with core as the actor. That disable says only that core no longer
    // stood behind the version approved at the time. In production an
    // approval is the operator's answer to a card on core's consent plane,
    // so approving a newer version means core stands behind the flow again,
    // and the disable is cleared. A disable by anyone else (the operator,
    // the owner, the runtime) was not about the approved version, so it
    // stays. A flow is also disabled with core as the actor when core
    // refuses one of its calls because its agent was retired; that is about
    // the agent, not the version, and a new version cannot bring a retired
    // agent back, so it stays too.
    if flow(tx, flow_id)?.is_some_and(|f| {
        f.disabled_by.as_deref() == Some(CORE_ACTOR)
            && f.disabled_reason.as_deref() != Some("agent_retired")
    }) {
        enable(tx, flow_id, now_ms)?;
    }
    Ok(approval)
}

/// Brings the flow's schedule in line with the version just approved, in
/// the approval's transaction: a version with a schedule trigger creates or
/// replaces the schedule (planning what the old one owed first); a version
/// without one removes any schedule an earlier version had, with its
/// planned fires.
fn schedule_approved(
    tx: &Transaction,
    flow_id: &str,
    version: u32,
    now_ms: i64,
    config: &SchedulerConfig,
) -> std::result::Result<Option<Approval>, InstallError> {
    let manifest_text: String = tx.query_row(
        "SELECT manifest FROM installs WHERE flow_id = ?1 AND version = ?2",
        params![flow_id, version_i64(version)],
        |r| r.get(0),
    )?;
    let manifest = Manifest::parse(&manifest_text)
        .map_err(|e| CoreError::Corrupt(format!("installed manifest of {flow_id}: {e}")))?;
    match manifest.trigger.schedule {
        Some(spec) => {
            let flow = ScheduledFlow {
                flow_id: flow_id.to_owned(),
                version: u64::from(version),
                spec,
            };
            let approval = schedule::table::approve(tx, &flow, timestamp(now_ms)?, config)?;
            Ok(Some(approval))
        }
        None => {
            schedule::table::remove(tx, flow_id)?;
            Ok(None)
        }
    }
}

fn timestamp(ms: i64) -> Result<jiff::Timestamp> {
    jiff::Timestamp::from_millisecond(ms).map_err(|e| CoreError::Invalid(format!("time {ms}: {e}")))
}

/// The approved version of a flow: what admission copies into each new run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approved {
    pub version: u32,
    pub code_hash: [u8; 32],
    pub script: String,
    pub manifest: String,
}

pub fn approved(conn: &Connection, flow_id: &str) -> Result<Option<Approved>> {
    if let Some(approved) = crate::packages::approved(conn, flow_id)? {
        return Ok(Some(approved));
    }
    let row: Option<(i64, Vec<u8>, String, String)> = conn
        .query_row(
            "SELECT i.version, i.code_hash, i.script, i.manifest FROM flows f \
             JOIN installs i ON i.flow_id = f.flow_id AND i.version = f.approved_version \
             WHERE f.flow_id = ?1 AND i.state = 'approved'",
            [flow_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    row.map(|(version, hash, script, manifest)| {
        Ok(Approved {
            version: u32::try_from(version)
                .map_err(|_| CoreError::Corrupt(format!("version {version}")))?,
            code_hash: crate::model::digest(hash)?,
            script,
            manifest,
        })
    })
    .transpose()
}

/// Why core no longer stands behind `version` of `flow_id`, if basal has
/// recorded that it does not (see [`revoke`]).
pub fn revocation(conn: &Connection, flow_id: &str, version: u32) -> Result<Option<String>> {
    let instance: Option<String> = conn
        .query_row(
            "SELECT reason FROM instance_revocations WHERE flow_id=?1 AND version=?2",
            params![flow_id, version],
            |r| r.get(0),
        )
        .optional()?;
    if instance.is_some() {
        return Ok(instance);
    }
    let reason: Option<Option<String>> = conn
        .query_row(
            "SELECT revoked_reason FROM installs WHERE flow_id = ?1 AND version = ?2 \
             AND revoked_at IS NOT NULL",
            params![flow_id, version_i64(version)],
            |r| r.get(0),
        )
        .optional()?;
    Ok(reason.map(Option::unwrap_or_default))
}

/// Records that core no longer stands behind `version` of `flow_id`: the
/// operator revoked it in core, core holds no install of it, or core
/// approved other code under it. The version is never activated or
/// approved again. If it is the flow's approved version, the flow is left
/// with no approved version (so `flow.health` lists it as `unapproved`),
/// and is disabled by core with `reason`, which stops its schedule and
/// tells its owner as every disable does. Returns false when the version
/// was already recorded as revoked (nothing is written then).
/// Package instances only record the version's revocation: their enable state
/// is independent of approval, so selecting another version can admit work again.
pub fn revoke(
    tx: &Transaction,
    flow_id: &str,
    version: u32,
    reason: &str,
    now_ms: i64,
) -> Result<bool> {
    let package: Option<String> = tx
        .query_row(
            "SELECT package FROM flows WHERE flow_id=?1",
            [flow_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if package.is_some() {
        let changed = tx.execute("INSERT OR IGNORE INTO instance_revocations (flow_id,version,reason,revoked_at) VALUES (?1,?2,?3,?4)", params![flow_id,version,reason,now_ms])?;
        return Ok(changed != 0);
    }
    let row: Option<(String, Option<i64>)> = tx
        .query_row(
            "SELECT state, revoked_at FROM installs WHERE flow_id = ?1 AND version = ?2",
            params![flow_id, version_i64(version)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((state, revoked_at)) = row else {
        return Err(CoreError::Corrupt(format!(
            "no install of {flow_id} v{version} to revoke"
        )));
    };
    if revoked_at.is_some() {
        return Ok(false);
    }
    // A revoked approved version is no longer the approved one; `revoked_at`
    // is what tells it apart from a version a newer approval replaced.
    tx.execute(
        "UPDATE installs SET revoked_at = ?3, revoked_reason = ?4, \
         state = CASE state WHEN 'approved' THEN 'superseded' ELSE state END \
         WHERE flow_id = ?1 AND version = ?2",
        params![flow_id, version_i64(version), now_ms, reason],
    )?;
    if state == "approved" {
        tx.execute(
            "UPDATE flows SET approved_version = NULL WHERE flow_id = ?1",
            [flow_id],
        )?;
        match disable(tx, flow_id, &Actor::Core, reason, now_ms) {
            Ok(_) => {}
            Err(InstallError::Store(e)) => return Err(e),
            Err(e) => return Err(CoreError::Invalid(e.to_string())),
        }
    }
    Ok(true)
}

/// A flow's enablement and ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowRecord {
    pub flow_id: String,
    pub owner: Option<String>,
    pub enabled: bool,
    pub approved_version: Option<u32>,
    pub disabled_by: Option<String>,
    pub disabled_reason: Option<String>,
}

pub fn flow(conn: &Connection, flow_id: &str) -> Result<Option<FlowRecord>> {
    type Row = (
        Option<String>,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
    );
    let row: Option<Row> = conn
        .query_row(
            "SELECT owner, state, CASE WHEN package IS NOT NULL AND EXISTS \
             (SELECT 1 FROM instance_revocations r WHERE r.flow_id=flows.flow_id AND r.version=flows.approved_version) \
             THEN NULL ELSE approved_version END, disabled_by, disabled_reason FROM flows \
             WHERE flow_id = ?1",
            [flow_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    row.map(|(owner, state, approved, by, reason)| {
        Ok(FlowRecord {
            flow_id: flow_id.to_owned(),
            owner,
            enabled: match state.as_str() {
                "enabled" => true,
                "disabled" => false,
                other => return Err(CoreError::Corrupt(format!("flow state {other:?}"))),
            },
            approved_version: approved
                .map(|v| u32::try_from(v).map_err(|_| CoreError::Corrupt(format!("version {v}"))))
                .transpose()?,
            disabled_by: by,
            disabled_reason: reason,
        })
    })
    .transpose()
}

/// Whether the flow is disabled. A flow with no record is treated as
/// disabled: nothing about it was ever approved.
pub fn is_disabled(conn: &Connection, flow_id: &str) -> Result<bool> {
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM flows WHERE flow_id = ?1",
            [flow_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(state.as_deref() != Some("enabled"))
}

/// How a disable by the runtime itself (auto-disable for sustained
/// saturation) is recorded in `flows.disabled_by`.
pub const RUNTIME_ACTOR: &str = "runtime";

/// How a disable by core is recorded in `flows.disabled_by`: core no longer
/// stands behind the flow's approved version (see [`revoke`]).
pub const CORE_ACTOR: &str = "core";

/// How every operator disable begins in `flows.disabled_by`
/// (`operator:<who>`).
pub const OPERATOR_ACTOR_PREFIX: &str = "operator:";

/// Who disables a flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    /// The operator may disable any flow.
    Operator(String),
    /// An agent may disable only a flow it owns.
    Agent(String),
    /// The runtime itself, for sustained saturation.
    Runtime,
    /// Core, which no longer stands behind the flow's approved version.
    Core,
}

impl Actor {
    fn label(&self) -> String {
        match self {
            Self::Operator(who) => format!("{OPERATOR_ACTOR_PREFIX}{who}"),
            Self::Agent(who) => format!("agent:{who}"),
            Self::Runtime => RUNTIME_ACTOR.to_owned(),
            Self::Core => CORE_ACTOR.to_owned(),
        }
    }
}

/// A notification for a flow's owner, waiting in the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// `flow.disabled` or `flow.auto_disabled`.
    pub kind: String,
    pub flow_id: String,
    /// The flow's owner when it was written; none if it had none.
    pub recipient: Option<String>,
    /// JSON with the details.
    pub body: String,
}

/// Writes a notification for the flow's owner.
pub fn notify_owner(
    tx: &Transaction,
    flow_id: &str,
    kind: &str,
    body: &str,
    now_ms: i64,
) -> Result<()> {
    let owner: Option<String> = tx
        .query_row(
            "SELECT owner FROM flows WHERE flow_id = ?1",
            [flow_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    tx.execute(
        "INSERT INTO outbox (at, kind, flow_id, recipient, body) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![now_ms, kind, flow_id, owner, body],
    )?;
    Ok(())
}

/// Disables a flow and tells its owner. Returns false when it was already
/// disabled (nothing is written then), with one exception: the operator
/// takes over a disable made by anyone else (see below).
pub fn disable(
    tx: &Transaction,
    flow_id: &str,
    actor: &Actor,
    reason: &str,
    now_ms: i64,
) -> std::result::Result<bool, InstallError> {
    let Some(record) = flow(tx, flow_id)? else {
        return Err(InstallError::NoSuchFlow(flow_id.to_owned()));
    };
    if let Actor::Agent(agent) = actor
        && record.owner.as_deref() != Some(agent.as_str())
    {
        return Err(InstallError::NotOwner {
            flow_id: flow_id.to_owned(),
            agent: agent.clone(),
        });
    }
    let changed = tx.execute(
        "UPDATE flows SET state = 'disabled', disabled_by = ?2, disabled_reason = ?3, \
         disabled_at = ?4 WHERE flow_id = ?1 AND state = 'enabled'",
        params![flow_id, actor.label(), reason, now_ms],
    )?;
    if changed == 0 {
        // The flow is already disabled. Who disabled it decides who may
        // enable it again, and an owner may undo its own disable; so the
        // operator must be able to take over a disable the owner (or the
        // runtime) made, or the operator could not stop a flow its owner
        // had paused and would re-enable. The record moves to the
        // operator; the schedule is already stopped. Agent and runtime
        // disables of a disabled flow change nothing.
        let takes_over = matches!(actor, Actor::Operator(_))
            && !record.enabled
            && !record
                .disabled_by
                .as_deref()
                .is_some_and(|by| by.starts_with(OPERATOR_ACTOR_PREFIX));
        if !takes_over {
            return Ok(false);
        }
        let taken = tx.execute(
            "UPDATE flows SET disabled_by = ?2, disabled_reason = ?3, disabled_at = ?4 \
             WHERE flow_id = ?1 AND state = 'disabled'",
            params![flow_id, actor.label(), reason, now_ms],
        )?;
        if taken == 0 {
            return Ok(false);
        }
    } else {
        // The flow's schedule stops in the same commit: due times passing
        // while it is disabled never fire, and fires planned but not
        // admitted go.
        schedule::table::disable(tx, flow_id, timestamp(now_ms)?)?;
    }
    let kind = match actor {
        Actor::Runtime => "flow.auto_disabled",
        _ => "flow.disabled",
    };
    notify_owner(
        tx,
        flow_id,
        kind,
        &serde_json::json!({"flow_id": flow_id, "by": actor.label(), "reason": reason}).to_string(),
        now_ms,
    )?;
    Ok(true)
}

/// Runs `f` in a write transaction, committing what it wrote unless the
/// store itself failed. An install refusal is a decision, not a failure:
/// whatever was written before it (nothing that grants anything) stays.
fn decide<T>(
    rt: &Runtime,
    f: impl FnOnce(&Transaction) -> std::result::Result<T, InstallError>,
) -> std::result::Result<T, InstallError> {
    let result = rt
        .store()
        .write(|tx| match f(tx) {
            Err(InstallError::Store(e)) => Err(e),
            other => Ok(other),
        })
        .map_err(InstallError::Store)?;
    rt.shared.signal.bump();
    result
}

impl Runtime {
    /// Validates and records a flow version. The steps that come between
    /// this and approval (the dry run and the operator's consent card) are
    /// not part of it.
    pub fn install(
        &self,
        request: &InstallRequest,
    ) -> std::result::Result<Installed, InstallError> {
        let cap = basal_proto::MAX_SCRIPT_BYTES;
        if request.script.len() > cap {
            return Err(InstallError::ScriptTooLarge {
                bytes: request.script.len(),
                cap,
            });
        }
        let manifest = Manifest::parse(&request.manifest).map_err(InstallError::Manifest)?;
        let warnings = validate(
            &manifest,
            &request.author,
            self.catalog(),
            &self.config().shell_denylist,
            request.loop_override,
        )?;
        let now = self.config().clock.now_ms();
        decide(self, |tx| record(tx, &manifest, request, warnings, now))
    }

    /// Approves an installed version, bound to its code hash. New triggers
    /// are admitted under it; runs already admitted keep their version. The
    /// flow's schedule follows in the same commit: what happened to it is
    /// returned, `None` when the approved version has no schedule trigger
    /// (any schedule an earlier version had is removed).
    pub fn approve(
        &self,
        flow_id: &str,
        version: u32,
        code_hash: &[u8; 32],
        approval_ref: &str,
    ) -> std::result::Result<Option<Approval>, InstallError> {
        let now = self.config().clock.now_ms();
        let schedule = self.config().schedule.clone();
        decide(self, |tx| {
            approve(
                tx,
                flow_id,
                version,
                code_hash,
                approval_ref,
                now,
                &schedule,
            )
        })
    }

    /// Disables a flow at once. The operator may disable any flow; an agent
    /// only a flow it owns.
    pub fn disable_flow(
        &self,
        flow_id: &str,
        actor: &Actor,
        reason: &str,
    ) -> std::result::Result<bool, InstallError> {
        let now = self.config().clock.now_ms();
        let changed = decide(self, |tx| disable(tx, flow_id, actor, reason, now))?;
        // Wake activations waiting for outcomes, so their next call is
        // checked against the flow's new state without delay.
        self.shared.signal.bump();
        Ok(changed)
    }

    /// Enables a disabled flow again. Operator only.
    pub fn enable_flow(&self, flow_id: &str) -> std::result::Result<bool, InstallError> {
        let now = self.config().clock.now_ms();
        decide(self, |tx| enable(tx, flow_id, now))
    }

    /// Enables a disabled flow on behalf of `actor`, deciding whether the
    /// actor may undo the flow's current disable in the same transaction
    /// that enables it (see [`enable_as`]).
    pub fn enable_flow_as(
        &self,
        flow_id: &str,
        actor: &Actor,
    ) -> std::result::Result<bool, InstallError> {
        let now = self.config().clock.now_ms();
        decide(self, |tx| enable_as(tx, flow_id, actor, now))
    }

    pub fn flow(&self, flow_id: &str) -> Result<Option<FlowRecord>> {
        self.store().read(|c| flow(c, flow_id))
    }

    /// Owner notifications not yet delivered, oldest first.
    pub fn outbox(&self) -> Result<Vec<Notification>> {
        self.store().read(|c| {
            let mut stmt = c.prepare(
                "SELECT kind, flow_id, recipient, body FROM outbox \
                 WHERE delivered_at IS NULL AND kind <> 'refusal_committed' ORDER BY seq",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Notification {
                        kind: r.get(0)?,
                        flow_id: r.get(1)?,
                        recipient: r.get(2)?,
                        body: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

/// Enables a disabled flow for `actor`. The operator may always. An agent
/// may only undo a disable it made itself, on a flow it owns: a disable by
/// the operator is the operator's stop, and an auto-disable is the loop
/// protection for a flow whose limits saturated, so letting the owner undo
/// either would let a flow's own author override its brakes. The runtime
/// never enables a flow. Enabling a flow that is not disabled changes
/// nothing and is allowed for its owner.
pub fn enable_as(
    tx: &Transaction,
    flow_id: &str,
    actor: &Actor,
    now_ms: i64,
) -> std::result::Result<bool, InstallError> {
    let Some(record) = flow(tx, flow_id)? else {
        return Err(InstallError::NoSuchFlow(flow_id.to_owned()));
    };
    match actor {
        Actor::Operator(_) => {}
        Actor::Runtime | Actor::Core => {
            return Err(InstallError::NotYourDisable {
                flow_id: flow_id.to_owned(),
                disabled_by: record.disabled_by.unwrap_or_default(),
            });
        }
        Actor::Agent(agent) => {
            if record.owner.as_deref() != Some(agent.as_str()) {
                return Err(InstallError::NotOwner {
                    flow_id: flow_id.to_owned(),
                    agent: agent.clone(),
                });
            }
            if record.enabled {
                return Ok(false);
            }
            let own = Actor::Agent(agent.clone()).label();
            let refused = || InstallError::NotYourDisable {
                flow_id: flow_id.to_owned(),
                disabled_by: record.disabled_by.clone().unwrap_or_default(),
            };
            match record.disabled_by.as_deref() {
                Some(by) if by == own => {}
                _ => return Err(refused()),
            }
        }
    }
    enable(tx, flow_id, now_ms)
}

/// Enables a disabled flow again (operator only). Its saturation history
/// is cleared, so windows from before the disable cannot count towards
/// disabling it again. Its schedule, if it has one, starts again in the same
/// commit, first due after `now_ms`: time spent disabled is not missed.
pub fn enable(
    tx: &Transaction,
    flow_id: &str,
    now_ms: i64,
) -> std::result::Result<bool, InstallError> {
    if flow(tx, flow_id)?.is_none() {
        return Err(InstallError::NoSuchFlow(flow_id.to_owned()));
    }
    let changed = tx.execute(
        "UPDATE flows SET state = 'enabled', disabled_by = NULL, disabled_reason = NULL, \
         disabled_at = NULL WHERE flow_id = ?1 AND state = 'disabled'",
        [flow_id],
    )?;
    if changed > 0 {
        tx.execute(
            "UPDATE rate_windows SET saturated = 0 WHERE flow_id = ?1",
            [flow_id],
        )?;
    }
    if changed > 0 && !crate::packages::removed(tx, flow_id)? {
        schedule::table::enable(tx, flow_id, timestamp(now_ms)?)?;
    }
    Ok(changed > 0)
}
