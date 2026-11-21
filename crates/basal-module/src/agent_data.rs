//! Basal's `agent-data/v1` participant: what Entorhinal calls to move an
//! agent's flows to another computer (`cortexkit-role-agent-data`,
//! CONTRACT.md).
//!
//! The ten ops are served on the internal service `agent-data`, to the
//! attested `reserved:entorhinal` only (`serve.rs` decides that before any of
//! this runs). One ref names a whole agent: `{ref_kind: "agent", ref:
//! <agent_id>}` covers every flow that belongs to the agent.
//!
//! - **Source:** `prepare` freezes the agent and encodes its flows as a
//!   `basal-flows/1` stream into a private staging file in the same store
//!   transaction, then admits the bytes through Engram's door and checks
//!   them with `session.cover`. `export` checks the cover again; `release`
//!   turns the freeze into a tombstone; `abort` lifts it.
//! - **Receiver:** `import` reads Basal's stream for the cut through the
//!   door into staging and checks its digest, format and contents, writing
//!   no live row. `activate` writes the flows in one transaction. A flow
//!   approved on the source waits disabled until this computer's prefrontal
//!   reports the same code active, which the engine checks outside the
//!   store's write lock (`basal_core::agent_move`).
//!
//! Every state-changing op records its first reply in the store with the
//! step it reached, so a repeat of the same request gets the same reply and
//! a different one is `id_reused`. `prepare` is the one op whose work spans
//! a door call after its commit: a repeat finds the cut prepared without a
//! reply and resumes the upload.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use basal_core::agent_move::{self, ActivateRefusal, Cut, DecodeError, FORMAT, Local, STREAM_CAP};
use basal_core::{CoreError, Runtime};
use basal_host::door::{CHUNK, Door};
use basal_host::transport::WireError;
use cortexkit_role_agent_data::{
    EmptyReply, Op,
    busy::{BusyEntry, BusyReply, BusyState},
    describe::{DescribeReply, Store, sync_class},
    diverged::{DivergedEntry, DivergedReply, DivergedState},
    errors::{
        BusyDetail, BusyRef, Detail, DigestMismatchDetail, ErrorBody, IncompatibleVersionDetail,
        InvalidStateDetail, ParticipantFailedDetail, RefusalCode, UnknownRefDetail,
        allowed_refusals,
    },
    export::{ExportReply, ExportedItem},
    import::{ImportReply, ImportedItem},
    prepare::{PrepareReply, PreparedItem},
    refs::{Ref, Refs},
    release::ReleaseReply,
    request::Request,
    status::StatusReply,
    step::{self, Step, StepRefusal},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The only ref kind basal holds: one ref per agent.
pub const REF_KIND: &str = "agent";
/// The store `describe` names.
pub const STORE: &str = "basal-flows";
/// Basal's principal, the stream every item names.
pub const STREAM: &str = "reserved:basal";

type Refusal = ErrorBody;

struct Downloaded {
    bytes: Vec<u8>,
    end: u64,
    digest: String,
}

enum DoorReadFailure<'a> {
    Pending,
    MissingItem,
    Final(&'a str),
    Other,
}

fn door_read_failure(error: &WireError) -> DoorReadFailure<'_> {
    match error.refusal_code() {
        // Older doors use not_found for cuts they cannot read yet.
        Some("cut_import_pending" | "not_found") => DoorReadFailure::Pending,
        Some("cut_item_missing") => DoorReadFailure::MissingItem,
        Some(
            code @ ("cut_not_published"
            | "cut_takeover_closed"
            | "cut_range_not_local"
            | "verify_failed"),
        ) => DoorReadFailure::Final(code),
        _ => DoorReadFailure::Other,
    }
}

fn read_cut_page(
    door: &dyn Door,
    cut_id: &str,
    session: &str,
    offset: u64,
    limit: u64,
) -> Result<Value, WireError> {
    door.call(
        "session.read",
        json!({"sessionRef": session, "stream": STREAM, "cutId": cut_id, "fromOffset": offset, "limit": limit}),
    )
}

fn refuse(code: RefusalCode, message: impl Into<String>) -> Refusal {
    ErrorBody::new(code, message)
}

fn typed(detail: Detail, message: impl Into<String>) -> Refusal {
    ErrorBody::with_typed_detail(detail, message)
}

fn unavailable(message: impl std::fmt::Display) -> Refusal {
    refuse(RefusalCode::Unavailable, message.to_string())
}

fn import_read_refusal(error: WireError) -> Refusal {
    let message = format!("reading the cut: {error:?}");
    match door_read_failure(&error) {
        DoorReadFailure::Pending => refuse(RefusalCode::CutNotReady, message),
        DoorReadFailure::Final("verify_failed") => refuse(RefusalCode::DigestMismatch, message),
        DoorReadFailure::Final(code) => {
            ErrorBody::with_participant_failed_detail(ParticipantFailedDetail::new(code), message)
        }
        _ => unavailable(message),
    }
}

fn source_door_refusal(action: &str, error: WireError) -> Refusal {
    let message = format!("{action}: {error:?}");
    // Prepare and export cannot refuse with cut_not_ready or digest_mismatch.
    match door_read_failure(&error) {
        DoorReadFailure::Final(code) => {
            ErrorBody::with_participant_failed_detail(ParticipantFailedDetail::new(code), message)
        }
        _ => unavailable(message),
    }
}

fn store_error(error: CoreError) -> Refusal {
    unavailable(format!("basal's store: {error}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `sessionRef` of an agent's stream for one cut: stable across a
/// retried `prepare`, distinct per agent and cut.
pub fn session_ref(agent: &str, cut_id: &str) -> String {
    hex(&Sha256::digest(
        format!("{STORE}:{agent}:{cut_id}").as_bytes(),
    ))[..32]
        .to_owned()
}

/// The participant's own state: the door it ships bytes through and where
/// it stages them. Its durable state is in the store.
pub struct AgentData {
    door: Mutex<Arc<dyn Door>>,
    dir: PathBuf,
    /// State-changing ops run one at a time, so two repeats of one request
    /// can't both start its work.
    serial: Mutex<()>,
}

impl AgentData {
    pub fn new(door: Arc<dyn Door>, dir: PathBuf) -> Self {
        Self {
            door: Mutex::new(door),
            dir,
            serial: Mutex::new(()),
        }
    }

    /// Replaces the door, for tests that stand one in.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_door(&self, door: Arc<dyn Door>) {
        *self.door.lock().unwrap_or_else(|p| p.into_inner()) = door;
    }

    fn door(&self) -> Arc<dyn Door> {
        self.door.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Removes staging left behind for cuts that no longer need it: one
    /// finished (`activated`, `aborted`, `released`) before its directory
    /// was deleted, or one with no checkpoint and no freeze (a prepare or
    /// import that never committed). A cut still in progress keeps its
    /// staging. Runs at startup, before any request; returns the cuts swept.
    pub fn sweep(&self, rt: &Runtime) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut swept = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if cortexkit_role_agent_data::refs::check_cut_id(&name).is_err() {
                continue;
            }
            let state = rt.store().read(|c| {
                let frozen: bool = c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM agent_move_holds WHERE kind='frozen' AND cut_id=?1)",
                    [&name],
                    |r| r.get(0),
                )?;
                Ok((agent_move::cut(c, &name)?, frozen))
            });
            let finished = match state {
                Ok((Some(cut), _)) => {
                    matches!(cut.step.as_str(), "activated" | "aborted" | "released")
                }
                Ok((None, frozen)) => !frozen,
                Err(error) => {
                    tracing::warn!(target: "agent_data", cut_id = %name, "can't read a staged cut's state: {error}");
                    false
                }
            };
            if finished {
                match std::fs::remove_dir_all(entry.path()) {
                    Ok(()) => swept.push(name),
                    Err(error) => {
                        tracing::warn!(target: "agent_data", cut_id = %name, "removing leftover staging: {error}")
                    }
                }
            }
        }
        if !swept.is_empty() {
            tracing::info!(target: "agent_data", cuts = ?swept, "removed leftover move staging");
        }
        swept
    }

    fn staging(&self, cut_id: &str) -> PathBuf {
        self.dir.join(cut_id)
    }

    /// Runs one op. A refusal outside the op's row of the contract's table
    /// would be a participant bug, so any such code goes out as
    /// `unavailable`.
    pub fn handle(&self, rt: &Runtime, method: &str, params: &Value) -> Result<Value, ErrorBody> {
        let Some(op) = Op::from_method(method) else {
            return Err(refuse(
                RefusalCode::InvalidRequest,
                format!("{method:?} is not an agent-data/v1 op"),
            ));
        };
        self.dispatch(rt, op, method, params).map_err(|mut error| {
            if !allowed_refusals(op).contains(&error.code) {
                tracing::warn!(target: "agent_data", method, code = error.code.as_str(), "refusal outside the op's contract row sent as unavailable");
                error = refuse(RefusalCode::Unavailable, error.message);
            }
            error
        })
    }

    fn dispatch(
        &self,
        rt: &Runtime,
        op: Op,
        method: &str,
        params: &Value,
    ) -> Result<Value, Refusal> {
        let request = Request::decode(method, params)?;
        match &request {
            Request::Describe(_) => Ok(to_value(DescribeReply::new(
                vec![REF_KIND.into()],
                vec![Store::new(STORE, sync_class::AGENT)],
                false,
            ))),
            Request::Busy(r) => self.busy(rt, &r.refs),
            Request::Status(r) => {
                let cut = rt
                    .store()
                    .read(|c| agent_move::cut(c, r.cut_id.as_str()))
                    .map_err(store_error)?;
                Ok(to_value(StatusReply::new(
                    cut.map_or(Step::None, |c| parse_step(&c.step)),
                )))
            }
            Request::Diverged(r) => self.diverged(rt, r.cut_id.as_str(), &r.refs),
            _ => {
                let _serial = self.serial.lock().unwrap_or_else(|p| p.into_inner());
                self.change(rt, op, &request)
            }
        }
    }

    fn busy(&self, rt: &Runtime, refs: &Refs) -> Result<Value, Refusal> {
        for r in refs.iter() {
            if r.ref_kind != REF_KIND {
                return Err(unknown_ref(r));
            }
        }
        let entries = rt
            .store()
            .read(|c| {
                refs.iter()
                    .map(|r| {
                        Ok(match agent_move::busy(c, &r.r#ref)? {
                            None => BusyEntry::new(BusyState::Idle),
                            Some(busy) => BusyEntry::new(BusyState::Busy)
                                .with_reason_code(busy.code)
                                .with_reason(busy.reason),
                        })
                    })
                    .collect::<basal_core::Result<Vec<_>>>()
            })
            .map_err(store_error)?;
        Ok(to_value(BusyReply::new(entries)))
    }

    fn diverged(&self, rt: &Runtime, cut_id: &str, refs: &Refs) -> Result<Value, Refusal> {
        let locals = rt
            .store()
            .read(|c| {
                refs.iter()
                    .map(|r| {
                        if r.ref_kind != REF_KIND {
                            return Ok(None);
                        }
                        Ok(Some((
                            agent_move::local(c, &r.r#ref)?,
                            agent_move::has_live_flows(c, &r.r#ref)?,
                        )))
                    })
                    .collect::<basal_core::Result<Vec<_>>>()
            })
            .map_err(store_error)?;
        // Live flows that match the last cut are at the cut only if the
        // incoming cut carries a basal item to replace them with. A cut
        // without one (the source had nothing for the agent) would leave
        // them here beside the agent's copy elsewhere, so they are a fork.
        // The cut is read only in that case, the rare one.
        let mut entries = Vec::with_capacity(refs.len());
        for (r, local) in refs.iter().zip(locals) {
            let entry = match local {
                None => DivergedEntry::new(DivergedState::Unknown),
                Some((Local::Advanced, _)) => DivergedEntry::new(DivergedState::Advanced),
                Some((Local::AtCut, false)) => DivergedEntry::new(DivergedState::AtCut),
                Some((Local::AtCut, true)) => self.probe_cut_item(cut_id, &r.r#ref)?,
            };
            entries.push(entry);
        }
        Ok(to_value(DivergedReply::new(entries)))
    }

    fn probe_cut_item(&self, cut_id: &str, agent: &str) -> Result<DivergedEntry, Refusal> {
        let session = session_ref(agent, cut_id);
        // One byte is enough to distinguish a listed item from a missing one.
        match read_cut_page(self.door().as_ref(), cut_id, &session, 0, 1) {
            Ok(_) => Ok(DivergedEntry::new(DivergedState::AtCut)),
            Err(error) => match door_read_failure(&error) {
                DoorReadFailure::MissingItem => Ok(DivergedEntry::new(DivergedState::Advanced)),
                DoorReadFailure::Pending => Err(refuse(
                    RefusalCode::CutNotReady,
                    format!("reading the cut: {error:?}"),
                )),
                DoorReadFailure::Final(code) => {
                    Ok(DivergedEntry::with_reason(DivergedState::Unknown, code)
                        .expect("a known door code is a valid reason"))
                }
                DoorReadFailure::Other => Err(unavailable(format!("reading the cut: {error:?}"))),
            },
        }
    }

    /// A state-changing op: a repeat first, then the step machine and the
    /// ref set, then the op's own work.
    fn change(&self, rt: &Runtime, op: Op, request: &Request) -> Result<Value, Refusal> {
        let cut_id = request
            .cut_id()
            .expect("state-changing ops carry a cut")
            .as_str()
            .to_owned();
        let refs = request
            .refs()
            .expect("state-changing ops carry refs")
            .clone();
        let name = op_name(op);
        let (recorded, cut) = rt
            .store()
            .read(|c| {
                Ok((
                    agent_move::recorded_reply(c, &cut_id, name)?,
                    agent_move::cut(c, &cut_id)?,
                ))
            })
            .map_err(store_error)?;
        if let Some((first, reply)) = recorded {
            let first = Request::decode(op.method(), &first)
                .map_err(|_| unavailable("a recorded request no longer decodes"))?;
            if first.with_sorted_refs() == request.with_sorted_refs() {
                return Ok(reply);
            }
            return Err(refuse(
                RefusalCode::IdReused,
                format!("cut {cut_id} already has another {name} request"),
            ));
        }
        let current = cut.as_ref().map_or(Step::None, |c| parse_step(&c.step));
        // A prepare whose commit landed but whose reply was lost resumes its
        // upload instead of meeting its own step.
        let resuming = op == Op::Prepare && current == Step::Prepared;
        if !resuming {
            step::advance(op, current).map_err(|refusal| match refusal {
                StepRefusal::CutUnknown => refuse(
                    RefusalCode::CutUnknown,
                    format!("no {name} can start: this computer has no state for cut {cut_id}"),
                ),
                StepRefusal::InvalidState { step } => typed(
                    Detail::InvalidState(InvalidStateDetail::new(step)),
                    format!("cut {cut_id} is {}", step.as_str()),
                ),
            })?;
        }
        let started = cut
            .as_ref()
            .and_then(|c| serde_json::from_value::<Refs>(c.refs.clone()).ok());
        if resuming {
            if started.as_ref().is_some_and(|s| !s.same_set(&refs)) {
                return Err(refuse(
                    RefusalCode::IdReused,
                    format!("cut {cut_id} was prepared for other refs"),
                ));
            }
        } else {
            step::check_ref_set(op, current, started.as_ref(), &refs).map_err(|detail| {
                typed(
                    Detail::InvalidState(detail),
                    format!("cut {cut_id} started with another ref set"),
                )
            })?;
        }
        let params = request.params();
        match request {
            Request::Prepare(_) => self.prepare(rt, &cut_id, &refs, cut, &params),
            Request::Export(_) => self.export(
                rt,
                &cut_id,
                cut.expect("the step machine required a cut"),
                &params,
            ),
            Request::Import(_) => self.import(rt, &cut_id, &refs, &params),
            Request::Activate(_) => self.activate(
                rt,
                &cut_id,
                cut.expect("the step machine required a cut"),
                &params,
            ),
            Request::Abort(_) => self.abort(rt, &cut_id, &refs, cut, &params),
            Request::Release(r) => self.release(
                rt,
                &cut_id,
                cut.expect("the step machine required a cut"),
                &r.holder,
                &params,
            ),
            _ => unreachable!("reads are answered before"),
        }
    }

    fn prepare(
        &self,
        rt: &Runtime,
        cut_id: &str,
        refs: &Refs,
        cut: Option<Cut>,
        params: &Value,
    ) -> Result<Value, Refusal> {
        let agent = source_agent(refs)?;
        let session = session_ref(&agent, cut_id);
        let dir = self.staging(cut_id);
        let cut = match cut {
            Some(cut) => cut,
            None => self.cut_source(rt, cut_id, refs, &agent, &session)?,
        };
        if cut.session_ref.is_none() {
            // Nothing to carry: the cut gets no item from basal, so a
            // receiver without basal can still take the agent. The freeze
            // stands all the same, and `release` still leaves a tombstone.
            let reply = to_value(PrepareReply::new(vec![], vec![]));
            self.record(rt, cut_id, Op::Prepare, params, &reply, None)?;
            return Ok(reply);
        }
        let end = cut.end_offset.unwrap_or(0);
        let digest = cut.digest.clone().unwrap_or_default();
        let bytes = std::fs::read(dir.join("payload")).map_err(|e| {
            refuse(
                RefusalCode::ParticipantFailed,
                format!("the staged cut is unreadable: {e}"),
            )
        })?;
        if bytes.len() as u64 != end || hex(&Sha256::digest(&bytes)) != digest {
            return Err(ErrorBody::with_participant_failed_detail(
                ParticipantFailedDetail::new("staging_digest_mismatch"),
                "the staged cut no longer matches its recorded digest",
            ));
        }
        self.upload(&session, &bytes)?;
        let reply = to_value(PrepareReply::new(
            vec![PreparedItem::new(
                REF_KIND, agent, session, STREAM, end, digest, FORMAT, true,
            )],
            vec![],
        ));
        self.record(rt, cut_id, Op::Prepare, params, &reply, None)?;
        Ok(reply)
    }

    /// Takes a new cut of the agent's flows in three steps, each safe to
    /// repeat after a crash:
    ///
    /// 1. Commit the freeze, rechecking in the same transaction that nothing
    ///    is busy. From then on the flows admit nothing and changes to them
    ///    are refused, so their rows stay as they are.
    /// 2. Encode the frozen rows from a read, with no write lock held, and
    ///    write and sync the staging file.
    /// 3. Commit the `prepared` checkpoint naming those bytes.
    ///
    /// A repeat after step 1 or 2 finds the freeze its own and encodes the
    /// same rows again.
    fn cut_source(
        &self,
        rt: &Runtime,
        cut_id: &str,
        refs: &Refs,
        agent: &str,
        session: &str,
    ) -> Result<Cut, Refusal> {
        let now = rt.config().clock.now_ms();
        rt.store()
            .write(|tx| agent_move::freeze(tx, agent, cut_id, now))
            .map_err(store_error)?
            .map_err(|busy| busy_refusal(agent, busy))?;
        let (encoded, fingerprint, queued) = rt
            .store()
            .read(|c| {
                Ok((
                    agent_move::encode(c, agent, cut_id)?,
                    agent_move::fingerprint(c, agent)?,
                    agent_move::queued_events(c, agent)?,
                ))
            })
            .map_err(store_error)?;
        let bytes = match encoded {
            Ok(bytes) if carries_nothing(&bytes) => {
                let cut = Cut {
                    cut_id: cut_id.into(),
                    agent: Some(agent.to_owned()),
                    side: "source".into(),
                    step: "prepared".into(),
                    refs: to_value(refs),
                    session_ref: None,
                    end_offset: None,
                    digest: None,
                    fingerprint: Some(fingerprint),
                    dropped: Some(json!({"queued_event": queued})),
                };
                self.save_source_cut(rt, agent, &cut, now)?;
                return Ok(cut);
            }
            Ok(bytes) => bytes,
            Err(message) => {
                // The agent can't move: give its flows back before refusing.
                rt.store()
                    .write(|tx| agent_move::lift(tx, agent, cut_id))
                    .map_err(store_error)?;
                rt.agent_move_changed();
                return Err(refuse(
                    RefusalCode::NotExportable,
                    format!("agent {agent}: {message}"),
                ));
            }
        };
        write_private(&self.staging(cut_id).join("payload"), &bytes)
            .map_err(|e| unavailable(format!("staging the cut: {e}")))?;
        let cut = Cut {
            cut_id: cut_id.into(),
            agent: Some(agent.to_owned()),
            side: "source".into(),
            step: "prepared".into(),
            refs: to_value(refs),
            session_ref: Some(session.to_owned()),
            end_offset: Some(bytes.len() as u64),
            digest: Some(hex(&Sha256::digest(&bytes))),
            fingerprint: Some(fingerprint),
            // The backlogged events the cut leaves behind; `release` adds
            // how many it discarded in the end.
            dropped: Some(json!({"queued_event": queued})),
        };
        self.save_source_cut(rt, agent, &cut, now)?;
        Ok(cut)
    }

    /// Commits a source cut's `prepared` checkpoint, under its own freeze.
    fn save_source_cut(
        &self,
        rt: &Runtime,
        agent: &str,
        cut: &Cut,
        now: i64,
    ) -> Result<(), Refusal> {
        rt.store()
            .write(|tx| {
                // Only this cut's own freeze may carry the checkpoint; ops are
                // serialized, so anything else means the store changed under it.
                match agent_move::hold(tx, agent)? {
                    Some(agent_move::Hold::Frozen { cut_id: held }) if held == cut.cut_id => {}
                    Some(agent_move::Hold::Moved { .. }) => {}
                    _ => return Err(CoreError::Unavailable("the cut's freeze is gone".into())),
                }
                agent_move::save_cut(tx, cut, now)
            })
            .map_err(store_error)
    }

    /// Admits `bytes` as the stream `session` through the door, resuming
    /// after whatever an earlier attempt already admitted, then checks the
    /// door has recorded all of it.
    fn upload(&self, session: &str, bytes: &[u8]) -> Result<(), Refusal> {
        let door = self.door();
        let mut offset = 0usize;
        while offset < bytes.len() {
            let chunk = &bytes[offset..(offset + CHUNK).min(bytes.len())];
            let next = offset + chunk.len();
            match door.call(
                "session.admit",
                json!({"sessionRef": session, "fromOffset": offset, "bytes": hex(chunk)}),
            ) {
                Ok(reply) if reply["durableOffset"].as_u64() == Some(next as u64) => {}
                Err(WireError::RefusedDetails { code, detail, .. })
                    if code == "offset_mismatch"
                        && detail["expectedOffset"]
                            .as_u64()
                            .is_some_and(|at| at >= next as u64 && at <= bytes.len() as u64) =>
                {
                    // An earlier attempt admitted this chunk: check the door
                    // holds the same bytes before going on.
                    let read = door
                        .call(
                            "session.read",
                            json!({"sessionRef": session, "fromOffset": offset, "limit": chunk.len()}),
                        )
                        .map_err(|e| source_door_refusal("reading back an admitted chunk", e))?;
                    if read["bytes"].as_str() != Some(hex(chunk).as_str()) {
                        return Err(refuse(
                            RefusalCode::ParticipantFailed,
                            "the door holds other bytes for this cut's stream",
                        ));
                    }
                }
                Err(error) => return Err(source_door_refusal("admitting the cut", error)),
                Ok(reply) => {
                    return Err(unavailable(format!(
                        "admitting the cut through the door: unexpected reply {reply:?}"
                    )));
                }
            }
            offset = next;
        }
        self.cover(session, bytes.len() as u64)
    }

    fn cover(&self, session: &str, end: u64) -> Result<(), Refusal> {
        let cover = self
            .door()
            .call("session.cover", json!({"sessionRef": session}))
            .map_err(|e| source_door_refusal("checking the cut's cover", e))?;
        if cover["durableOffset"].as_u64() != Some(end)
            || cover["cloudOffset"].as_u64().unwrap_or(0) < end
        {
            return Err(unavailable("the cut's stream is not covered yet"));
        }
        Ok(())
    }

    fn export(
        &self,
        rt: &Runtime,
        cut_id: &str,
        cut: Cut,
        params: &Value,
    ) -> Result<Value, Refusal> {
        // A cut with nothing to carry admitted no stream, so there is
        // nothing for the door to cover.
        let items = match cut.session_ref.clone() {
            None => vec![],
            Some(session) => {
                let end = cut.end_offset.unwrap_or(0);
                self.cover(&session, end)?;
                vec![ExportedItem::new(session, end)]
            }
        };
        let reply = to_value(ExportReply::new(items));
        self.record(
            rt,
            cut_id,
            Op::Export,
            params,
            &reply,
            Some(Cut {
                step: "exported".into(),
                ..cut
            }),
        )?;
        Ok(reply)
    }

    fn import(
        &self,
        rt: &Runtime,
        cut_id: &str,
        refs: &Refs,
        params: &Value,
    ) -> Result<Value, Refusal> {
        let agent = receiver_agent(refs)?;
        let session = session_ref(&agent, cut_id);
        let Some(Downloaded { bytes, end, digest }) = self.download(cut_id, &session)? else {
            return self.import_nothing(rt, cut_id, refs, &agent, params);
        };
        let dir = self.staging(cut_id);
        let staged = match agent_move::stream::decode(&bytes) {
            Ok(staged) => staged,
            Err(DecodeError::Incompatible { found }) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(typed(
                    Detail::IncompatibleVersion(IncompatibleVersionDetail::new(found, FORMAT)),
                    "this computer's basal can't read the cut's format",
                ));
            }
            Err(DecodeError::Damaged(message)) => {
                return Err(ErrorBody::with_digest_mismatch_detail(
                    DigestMismatchDetail::new("staging_digest_mismatch"),
                    message,
                ));
            }
        };
        if staged.agent != agent || staged.cut_id != cut_id {
            return Err(refuse(
                RefusalCode::DigestMismatch,
                "the cut's stream names another agent or cut",
            ));
        }
        let (busy, local, collision) = rt
            .store()
            .read(|c| {
                Ok((
                    agent_move::busy(c, &agent)?,
                    agent_move::local(c, &agent)?,
                    agent_move::collision(c, &agent, &staged)?,
                ))
            })
            .map_err(store_error)?;
        if let Some(busy) = busy {
            return Err(busy_refusal(&agent, busy));
        }
        if local == Local::Advanced {
            return Err(advanced(&agent));
        }
        if let Some(collision) = collision {
            return Err(collided(&collision));
        }
        write_private(&dir.join("incoming"), &bytes)
            .map_err(|e| unavailable(format!("staging the cut: {e}")))?;
        let dropped = staged.dropped();
        let mut item = ImportedItem::new(session.clone(), end, digest.clone());
        if let Some(dropped) = &dropped {
            item = item
                .with_dropped(dropped.clone())
                .map_err(|e| refuse(RefusalCode::ParticipantFailed, e))?;
        }
        let reply = to_value(ImportReply::new(vec![item]));
        let cut = Cut {
            cut_id: cut_id.into(),
            agent: Some(agent),
            side: "receiver".into(),
            step: "imported".into(),
            refs: to_value(refs),
            session_ref: Some(session),
            end_offset: Some(end),
            digest: Some(digest),
            fingerprint: None,
            dropped: dropped.map(|d| json!(d)),
        };
        self.record(rt, cut_id, Op::Import, params, &reply, Some(cut))?;
        Ok(reply)
    }

    /// Imports a cut that carries no basal item: the source had nothing for
    /// the agent. Nothing is staged; `activate` only replaces a tombstone of
    /// the agent here. Live flows of the agent here would stay beside its
    /// copy elsewhere, so they are refused as a fork.
    fn import_nothing(
        &self,
        rt: &Runtime,
        cut_id: &str,
        refs: &Refs,
        agent: &str,
        params: &Value,
    ) -> Result<Value, Refusal> {
        let live = rt
            .store()
            .read(|c| agent_move::has_live_flows(c, agent))
            .map_err(store_error)?;
        if live {
            return Err(advanced(agent));
        }
        let reply = to_value(ImportReply::new(vec![]));
        let cut = Cut {
            cut_id: cut_id.into(),
            agent: Some(agent.to_owned()),
            side: "receiver".into(),
            step: "imported".into(),
            refs: to_value(refs),
            session_ref: None,
            end_offset: None,
            digest: None,
            fingerprint: None,
            dropped: None,
        };
        self.record(rt, cut_id, Op::Import, params, &reply, Some(cut))?;
        Ok(reply)
    }

    /// Reads the known stream and checks it against the sealed length and
    /// digest from its first page. Only an absent item has no download.
    fn download(&self, cut_id: &str, session: &str) -> Result<Option<Downloaded>, Refusal> {
        let door = self.door();
        let mut reply = match read_cut_page(door.as_ref(), cut_id, session, 0, CHUNK as u64) {
            Ok(reply) => reply,
            Err(error) if matches!(door_read_failure(&error), DoorReadFailure::MissingItem) => {
                return Ok(None);
            }
            Err(error) => return Err(import_read_refusal(error)),
        };
        let damaged = |message: &str| refuse(RefusalCode::DigestMismatch, message.to_owned());
        let end = reply["streamEnd"]
            .as_u64()
            .ok_or_else(|| damaged("the cut's item has no length"))?;
        if end > STREAM_CAP {
            return Err(damaged("the cut's stream is larger than a move carries"));
        }
        let digest = reply["digest"]
            .as_str()
            .ok_or_else(|| damaged("the cut's item has no digest"))?
            .to_owned();
        let mut bytes = Vec::with_capacity(usize::try_from(end).unwrap_or(0));
        loop {
            let offset = bytes.len() as u64;
            let limit = (end - offset).min(CHUNK as u64);
            if reply["fromOffset"].as_u64() != Some(offset)
                || reply["endOffset"].as_u64() != Some(offset + limit)
                || reply["streamEnd"].as_u64() != Some(end)
                || reply["digest"].as_str() != Some(digest.as_str())
            {
                return Err(damaged(
                    "the door's read of the cut doesn't match the sealed item",
                ));
            }
            let chunk = reply["bytes"]
                .as_str()
                .and_then(agent_move::stream::unhex)
                .ok_or_else(|| damaged("the door's read carries no readable bytes"))?;
            if chunk.len() as u64 != limit {
                return Err(damaged("a short read of the cut"));
            }
            bytes.extend(chunk);
            let offset = bytes.len() as u64;
            if offset == end {
                break;
            }
            reply = read_cut_page(
                door.as_ref(),
                cut_id,
                session,
                offset,
                (end - offset).min(CHUNK as u64),
            )
            .map_err(import_read_refusal)?;
        }
        if hex(&Sha256::digest(&bytes)) != digest {
            return Err(ErrorBody::with_digest_mismatch_detail(
                DigestMismatchDetail::new("staging_digest_mismatch"),
                "the cut's bytes don't hash to its sealed digest",
            ));
        }
        Ok(Some(Downloaded { bytes, end, digest }))
    }

    fn activate(
        &self,
        rt: &Runtime,
        cut_id: &str,
        cut: Cut,
        params: &Value,
    ) -> Result<Value, Refusal> {
        let agent = cut.agent.clone().unwrap_or_default();
        let dir = self.staging(cut_id);
        if cut.session_ref.is_none() {
            return self.activate_nothing(rt, cut_id, cut, params);
        }
        let bytes = std::fs::read(dir.join("incoming")).map_err(|e| {
            ErrorBody::with_digest_mismatch_detail(
                DigestMismatchDetail::new("staging_digest_mismatch"),
                format!("the staged cut is unreadable: {e}"),
            )
        })?;
        if cut.digest.as_deref() != Some(hex(&Sha256::digest(&bytes)).as_str()) {
            return Err(ErrorBody::with_digest_mismatch_detail(
                DigestMismatchDetail::new("staging_digest_mismatch"),
                "the staged cut no longer matches its recorded digest",
            ));
        }
        let staged = agent_move::stream::decode(&bytes).map_err(|e| {
            ErrorBody::with_digest_mismatch_detail(
                DigestMismatchDetail::new("staging_digest_mismatch"),
                format!("{e:?}"),
            )
        })?;
        let reply = to_value(EmptyReply::new());
        let now = rt.config().clock.now_ms();
        let name = op_name(Op::Activate);
        let outcome = rt
            .store()
            .write(|tx| {
                let fingerprint = match agent_move::activate(tx, &agent, cut_id, &staged, now)? {
                    Ok(fingerprint) => fingerprint,
                    Err(ActivateRefusal::Busy(busy)) => return Ok(Err(busy_refusal(&agent, busy))),
                    Err(ActivateRefusal::Advanced) => return Ok(Err(advanced(&agent))),
                    Err(ActivateRefusal::Collision(collision)) => {
                        return Ok(Err(collided(&collision)));
                    }
                    Err(ActivateRefusal::Rejected(message)) => {
                        return Ok(Err(ErrorBody::with_participant_failed_detail(
                            ParticipantFailedDetail::new("moved_rows_rejected"),
                            format!("this store refused the moved flows: {message}"),
                        )));
                    }
                };
                let cut = Cut {
                    step: "activated".into(),
                    fingerprint: Some(fingerprint),
                    ..cut.clone()
                };
                agent_move::save_cut(tx, &cut, now)?;
                agent_move::record_reply(tx, cut_id, name, params, &reply)?;
                Ok(Ok(()))
            })
            .map_err(store_error)?;
        outcome?;
        let _ = std::fs::remove_dir_all(&dir);
        // Moved flows are due for an approval check now; the engine runs it
        // without the store's write lock.
        rt.agent_move_changed();
        Ok(reply)
    }

    /// Activates a cut that carried no basal item. No flow arrives; a
    /// tombstone of the agent here is replaced as on any move back, since
    /// the agent now lives on this computer and may install flows here.
    fn activate_nothing(
        &self,
        rt: &Runtime,
        cut_id: &str,
        cut: Cut,
        params: &Value,
    ) -> Result<Value, Refusal> {
        let agent = cut.agent.clone().unwrap_or_default();
        let empty = agent_move::stream::Staged {
            agent: agent.clone(),
            cut_id: cut_id.into(),
            counts: Default::default(),
            records: vec![],
        };
        let reply = to_value(EmptyReply::new());
        let now = rt.config().clock.now_ms();
        let name = op_name(Op::Activate);
        rt.store()
            .write(|tx| {
                // A live flow that appeared since `import` would be lost
                // beside the agent's copy elsewhere.
                if agent_move::has_live_flows(tx, &agent)? {
                    return Ok(Err(advanced(&agent)));
                }
                let fingerprint = match agent_move::activate(tx, &agent, cut_id, &empty, now)? {
                    Ok(fingerprint) => fingerprint,
                    Err(ActivateRefusal::Busy(busy)) => return Ok(Err(busy_refusal(&agent, busy))),
                    Err(ActivateRefusal::Advanced) => return Ok(Err(advanced(&agent))),
                    Err(ActivateRefusal::Collision(collision)) => {
                        return Ok(Err(collided(&collision)));
                    }
                    Err(ActivateRefusal::Rejected(message)) => {
                        return Ok(Err(ErrorBody::with_participant_failed_detail(
                            ParticipantFailedDetail::new("moved_rows_rejected"),
                            format!("this store refused the move: {message}"),
                        )));
                    }
                };
                let cut = Cut {
                    step: "activated".into(),
                    fingerprint: Some(fingerprint),
                    ..cut.clone()
                };
                agent_move::save_cut(tx, &cut, now)?;
                agent_move::record_reply(tx, cut_id, name, params, &reply)?;
                Ok(Ok(()))
            })
            .map_err(store_error)??;
        rt.agent_move_changed();
        Ok(reply)
    }

    fn abort(
        &self,
        rt: &Runtime,
        cut_id: &str,
        refs: &Refs,
        cut: Option<Cut>,
        params: &Value,
    ) -> Result<Value, Refusal> {
        let reply = to_value(EmptyReply::new());
        let now = rt.config().clock.now_ms();
        let name = op_name(Op::Abort);
        let cut = cut.unwrap_or_else(|| Cut {
            cut_id: cut_id.into(),
            agent: refs
                .iter()
                .find(|r| r.ref_kind == REF_KIND)
                .map(|r| r.r#ref.clone()),
            side: "unknown".into(),
            step: "aborted".into(),
            refs: to_value(refs),
            session_ref: None,
            end_offset: None,
            digest: None,
            fingerprint: None,
            dropped: None,
        });
        rt.store()
            .write(|tx| {
                // A prepare whose freeze committed before its checkpoint left
                // no source row, so lift whatever freeze this cut holds; `lift`
                // touches only a freeze taken under this cut's id.
                if cut.side != "receiver"
                    && let Some(agent) = &cut.agent
                {
                    agent_move::lift(tx, agent, cut_id)?;
                }
                agent_move::save_cut(
                    tx,
                    &Cut {
                        step: "aborted".into(),
                        ..cut.clone()
                    },
                    now,
                )?;
                agent_move::record_reply(tx, cut_id, name, params, &reply)
            })
            .map_err(store_error)?;
        let _ = std::fs::remove_dir_all(self.staging(cut_id));
        rt.agent_move_changed();
        Ok(reply)
    }

    fn release(
        &self,
        rt: &Runtime,
        cut_id: &str,
        cut: Cut,
        holder: &str,
        params: &Value,
    ) -> Result<Value, Refusal> {
        let agent = cut.agent.clone().unwrap_or_default();
        let reply = to_value(ReleaseReply::new(true));
        let now = rt.config().clock.now_ms();
        let name = op_name(Op::Release);
        rt.release_agent(&agent, cut_id, holder, |tx, released| {
            // Events queued during the freeze reached no cut, so the
            // receiver's `dropped` count can't include them: the source's
            // checkpoint records how many it discarded in the end.
            let mut dropped = cut.dropped.clone().unwrap_or_else(|| json!({}));
            dropped["discarded_at_release"] = json!(released.queued_events);
            agent_move::save_cut(
                tx,
                &Cut {
                    step: "released".into(),
                    dropped: Some(dropped),
                    ..cut.clone()
                },
                now,
            )?;
            agent_move::record_reply(tx, cut_id, name, params, &reply)
        })
        .map_err(store_error)?;
        let _ = std::fs::remove_dir_all(self.staging(cut_id));
        Ok(reply)
    }

    /// Records `op`'s reply, with the cut's new checkpoint when it has one,
    /// in one transaction.
    fn record(
        &self,
        rt: &Runtime,
        cut_id: &str,
        op: Op,
        params: &Value,
        reply: &Value,
        cut: Option<Cut>,
    ) -> Result<(), Refusal> {
        let now = rt.config().clock.now_ms();
        rt.store()
            .write(|tx| {
                if let Some(cut) = &cut {
                    agent_move::save_cut(tx, cut, now)?;
                }
                agent_move::record_reply(tx, cut_id, op_name(op), params, reply)
            })
            .map_err(store_error)
    }
}

/// Whether an encoded stream holds no record at all: no flow, instance,
/// `kv` entry, run history or instance generation. Such a cut ships no item.
fn carries_nothing(bytes: &[u8]) -> bool {
    agent_move::stream::decode(bytes).is_ok_and(|staged| staged.records.is_empty())
}

fn op_name(op: Op) -> &'static str {
    op.method().trim_start_matches("agent_data.")
}

/// A release records its request in the same transaction that blocks local
/// writes, so the destination box id does not need separate storage.
pub(crate) fn released_holder_box(
    conn: &rusqlite::Connection,
    cut_id: &str,
) -> basal_core::Result<Option<String>> {
    Ok(
        agent_move::recorded_reply(conn, cut_id, op_name(Op::Release))?
            .and_then(|(request, _)| request["holderBox"].as_str().map(str::to_owned)),
    )
}

fn parse_step(text: &str) -> Step {
    serde_json::from_value(json!(text)).unwrap_or(Step::None)
}

fn to_value<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("contract types serialize")
}

fn unknown_ref(r: &Ref) -> Refusal {
    typed(
        Detail::UnknownRef(UnknownRefDetail::new(r.ref_kind.clone(), r.r#ref.clone())),
        format!("basal holds refs of kind {REF_KIND:?} only"),
    )
}

/// The agent a source-side op names: one ref of kind `agent`. Basal holds
/// every agent (one without flows moves as an empty stream), so only
/// another kind is `unknown_ref`.
fn source_agent(refs: &Refs) -> Result<String, Refusal> {
    if let Some(other) = refs.iter().find(|r| r.ref_kind != REF_KIND) {
        return Err(unknown_ref(other));
    }
    receiver_agent(refs)
}

fn receiver_agent(refs: &Refs) -> Result<String, Refusal> {
    match refs.as_slice() {
        [r] if r.ref_kind == REF_KIND => Ok(r.r#ref.clone()),
        _ => Err(refuse(
            RefusalCode::InvalidRequest,
            "a move names exactly one ref, {ref_kind: \"agent\", ref: <agent id>}",
        )),
    }
}

fn busy_refusal(agent: &str, busy: agent_move::Busy) -> Refusal {
    typed(
        Detail::Busy(BusyDetail::new(vec![
            BusyRef::new(REF_KIND, agent, busy.reason.clone()).with_reason_code(busy.code),
        ])),
        busy.reason,
    )
}

fn advanced(agent: &str) -> Refusal {
    refuse(
        RefusalCode::LocalAdvanced,
        format!("this computer's flows for agent {agent} have data the cut doesn't hold"),
    )
}

fn collided(collision: &agent_move::Collision) -> Refusal {
    refuse(
        RefusalCode::LocalAdvanced,
        format!(
            "flow {} already exists on this computer and belongs to {}; rename or remove one of them, then move again",
            collision.flow_id,
            collision.owner.as_deref().unwrap_or("no recorded owner"),
        ),
    )
}

/// Writes `bytes` to `path` in a directory only this user can read, and
/// syncs both, so a staged cut survives a crash once the op that wrote it
/// has committed.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .expect("staging files live in a cut's directory");
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
#[path = "agent_data_tests.rs"]
mod tests;
