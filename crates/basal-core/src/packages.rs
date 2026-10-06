//! Immutable package bytes and generation-ordered persona instances.
//! Instances share code, never state: all state remains keyed by flow id.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::ids::{code_hash, hex};
use crate::install::{self, Approved};
use crate::manifest::Manifest;
use crate::schedule::{self, ScheduledFlow};
use crate::{CoreError, InstallError, Runtime};

#[derive(Debug)]
pub enum PackageError {
    Refused {
        code: &'static str,
        current: Option<i64>,
    },
    Install(InstallError),
    Store(CoreError),
}

fn refused(code: &'static str) -> PackageError {
    PackageError::Refused {
        code,
        current: None,
    }
}

impl From<CoreError> for PackageError {
    fn from(e: CoreError) -> Self {
        Self::Store(e)
    }
}
impl From<rusqlite::Error> for PackageError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.into())
    }
}

/// Length-framed stable identity; neither names nor versions enter it.
pub fn instance_id(package: &str, agent_id: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"basal-instance-id-v1\0");
    for value in [package, agent_id] {
        h.update(&(value.len() as u64).to_le_bytes());
        h.update(value.as_bytes());
    }
    format!("{package}_{}", &h.finalize().to_hex()[..16])
}

pub fn get(conn: &Connection, package: &str, version: u32) -> crate::Result<Option<Approved>> {
    let row = conn.query_row(
        "SELECT code_hash, script, manifest FROM package_versions WHERE package=?1 AND version=?2",
        params![package, version],
        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
    ).optional()?;
    row.map(|(hash, script, manifest)| {
        Ok(Approved {
            version,
            code_hash: crate::model::digest(hash)?,
            script,
            manifest,
        })
    })
    .transpose()
}

pub(crate) fn approved(conn: &Connection, flow_id: &str) -> crate::Result<Option<Approved>> {
    let pair = conn.query_row(
        "SELECT package, approved_version FROM flows WHERE flow_id=?1 AND package IS NOT NULL AND removed=0 AND NOT EXISTS (SELECT 1 FROM instance_revocations r WHERE r.flow_id=flows.flow_id AND r.version=flows.approved_version)",
        [flow_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)),
    ).optional()?;
    match pair {
        Some((p, v)) => get(conn, &p, v),
        None => Ok(None),
    }
}

pub fn removed(conn: &Connection, flow_id: &str) -> crate::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT removed FROM flows WHERE flow_id=?1",
            [flow_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

// Domain refusals commit no partial changes: all validation precedes writes.
fn decide<T>(
    rt: &Runtime,
    f: impl FnOnce(&Transaction) -> Result<T, PackageError>,
) -> Result<T, PackageError> {
    rt.store()
        .write(|tx| match f(tx) {
            Err(PackageError::Store(e)) => Err(e),
            other => Ok(other),
        })
        .map_err(PackageError::Store)?
}

fn generation(
    tx: &Transaction,
    package: &str,
    agent: &str,
    incoming: i64,
    op: &str,
    version: Option<u32>,
) -> Result<Option<Value>, PackageError> {
    let row = tx.query_row(
        "SELECT generation, operation, version, reply FROM instance_generations WHERE package=?1 AND agent_id=?2",
        params![package, agent], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<u32>>(2)?, r.get::<_, String>(3)?)),
    ).optional()?;
    if let Some((current, operation, stored_version, reply)) = row {
        if incoming < current {
            return Err(PackageError::Refused {
                code: "generation_stale",
                current: Some(current),
            });
        }
        if incoming == current {
            if operation != op || stored_version != version {
                return Err(refused("generation_conflict"));
            }
            return Ok(Some(
                serde_json::from_str(&reply).map_err(|e| CoreError::Corrupt(e.to_string()))?,
            ));
        }
    }
    Ok(None)
}

fn record_generation(
    tx: &Transaction,
    package: &str,
    agent: &str,
    generation: i64,
    operation: &str,
    version: Option<u32>,
    reply: &Value,
) -> Result<(), PackageError> {
    tx.execute("INSERT INTO instance_generations (package,agent_id,generation,operation,version,reply) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(package,agent_id) DO UPDATE SET generation=excluded.generation,operation=excluded.operation,version=excluded.version,reply=excluded.reply",
        params![package,agent,generation,operation,version,reply.to_string()])?;
    Ok(())
}

impl Runtime {
    pub fn register_package(&self, script: &str, text: &str) -> Result<Value, PackageError> {
        if script.len() > basal_proto::MAX_SCRIPT_BYTES {
            return Err(PackageError::Install(InstallError::ScriptTooLarge {
                bytes: script.len(),
                cap: basal_proto::MAX_SCRIPT_BYTES,
            }));
        }
        let manifest =
            Manifest::decode(text).map_err(|e| PackageError::Install(InstallError::Manifest(e)))?;
        if manifest.id.len() > 46 {
            return Err(refused("package_id_too_long"));
        }
        manifest
            .check()
            .map_err(|e| PackageError::Install(InstallError::Manifest(e)))?;
        if manifest.agents().iter().any(|(_, agent)| *agent != "$self") {
            return Err(refused("package_names_agent"));
        }
        install::validate_inner(
            &manifest,
            "operator",
            self.catalog(),
            &self.config().shell_denylist,
            false,
            true,
        )
        .map_err(PackageError::Install)?;
        let hash = code_hash(script, text);
        decide(self, |tx| {
            let existing = get(tx, &manifest.id, manifest.version)?;
            if existing.as_ref().is_some_and(|p| p.code_hash != hash) {
                return Err(refused("package_version_conflict"));
            }
            let new = existing.is_none();
            if new {
                tx.execute("INSERT INTO package_versions (package,version,script,manifest,code_hash,registered_at) VALUES (?1,?2,?3,?4,?5,?6)", params![manifest.id,manifest.version,script,text,hash.as_slice(),self.config().clock.now_ms()])?;
            }
            Ok(
                json!({"package":manifest.id,"version":manifest.version,"code_hash":hex(&hash),"new":new}),
            )
        })
    }

    pub fn get_package(&self, package: &str, version: u32) -> Result<Value, PackageError> {
        let p = self
            .store()
            .read(|c| get(c, package, version))?
            .ok_or_else(|| refused("package_unknown"))?;
        Ok(
            json!({"package":package,"version":version,"script":p.script,"manifest":p.manifest,"code_hash":hex(&p.code_hash)}),
        )
    }

    pub fn ensure_instance(
        &self,
        package: &str,
        version: u32,
        agent: &str,
        incoming: i64,
    ) -> Result<Value, PackageError> {
        decide(self, |tx| {
            if let Some(reply) = generation(tx, package, agent, incoming, "ensure", Some(version))?
            {
                return Ok(reply);
            }
            let p = get(tx, package, version)?.ok_or_else(|| refused("package_unknown"))?;
            if self.catalog().agent_id(agent).as_deref() != Some(agent) {
                return Err(refused("agent_unknown"));
            }
            let id = instance_id(package, agent);
            // The pointer remains even when approval is revoked, so removal
            // and re-ensure can report the selected version without losing state.
            let previous: Option<u32> = tx
                .query_row(
                    "SELECT approved_version FROM flows WHERE flow_id=?1",
                    [&id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let new = previous.is_none();
            tx.execute("INSERT INTO flows (flow_id,owner,approved_version,created_at,package) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(flow_id) DO UPDATE SET approved_version=excluded.approved_version,removed=0", params![id,agent,version,self.config().clock.now_ms(),package])?;
            // A same-version generation changes only ordering and removal.
            // In particular, a disabled schedule must not be reset by a resend.
            if previous != Some(version) {
                let manifest =
                    Manifest::parse(&p.manifest).map_err(|e| CoreError::Corrupt(e.to_string()))?;
                if let Some(spec) = manifest.trigger.schedule {
                    let now = jiff::Timestamp::from_millisecond(self.config().clock.now_ms())
                        .map_err(|e| CoreError::Invalid(e.to_string()))?;
                    schedule::table::approve_instance(
                        tx,
                        &ScheduledFlow {
                            flow_id: id.clone(),
                            version: u64::from(version),
                            spec,
                        },
                        now,
                        &self.config().schedule,
                    )?;
                    if install::is_disabled(tx, &id)? {
                        schedule::table::disable(tx, &id, now)?;
                    }
                } else {
                    schedule::table::remove(tx, &id)?;
                }
            }
            let reply =
                json!({"flow_id":id,"generation":incoming,"new":new,"previous_version":previous});
            record_generation(
                tx,
                package,
                agent,
                incoming,
                "ensure",
                Some(version),
                &reply,
            )?;
            Ok(reply)
        })
    }

    pub fn remove_instance(
        &self,
        package: &str,
        agent: &str,
        incoming: i64,
    ) -> Result<Value, PackageError> {
        decide(self, |tx| {
            if let Some(reply) = generation(tx, package, agent, incoming, "remove", None)? {
                return Ok(reply);
            }
            let id = instance_id(package, agent);
            let changed = tx.execute(
                "UPDATE flows SET removed=1 WHERE flow_id=?1 AND package IS NOT NULL AND removed=0",
                [&id],
            )?;
            let reply = json!({"flow_id":id,"generation":incoming,"removed":changed != 0});
            record_generation(tx, package, agent, incoming, "remove", None, &reply)?;
            Ok(reply)
        })
    }
}
