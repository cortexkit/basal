//! A deterministic Broca with the service's request and response JSON formats.
//! Admitted runs, text and usage can be synced to a file, preserving the
//! simulated remote service's state when the caller process is killed.

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Run {
    route: Route,
    bytes: Vec<u8>,
    run_id: String,
    submission_id: String,
    pending: bool,
    status: RunStatusResponse,
    events: Vec<SubscribeEvent>,
    messages: Vec<SessionReadMessage>,
    archived: bool,
    read_refusal: Option<String>,
    drop_finish: bool,
    redeliver: bool,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    runs: BTreeMap<String, Run>,
    sends: Vec<(Route, Vec<u8>)>,
    subscriptions: Vec<(Route, SubscribeParams)>,
    status_calls: usize,
    read_calls: usize,
    pending_next: bool,
    cut_next_reply: bool,
}
#[derive(Clone)]
pub struct FakeBroca {
    state: Arc<Mutex<State>>,
    path: Option<PathBuf>,
}
impl Default for FakeBroca {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            path: None,
        }
    }
}
impl FakeBroca {
    pub fn persistent(path: impl AsRef<Path>) -> Result<Self, BrocaError> {
        let path = path.as_ref().to_owned();
        let state = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| BrocaError::Store(e.to_string()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(BrocaError::Store(e.to_string())),
        };
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            path: Some(path),
        })
    }
    fn save(&self, state: &State) -> Result<(), BrocaError> {
        if let Some(path) = &self.path {
            use std::io::Write;
            let bytes = serde_json::to_vec(state).map_err(|e| BrocaError::Store(e.to_string()))?;
            let tmp = path.with_extension("tmp");
            let mut file =
                std::fs::File::create(&tmp).map_err(|e| BrocaError::Store(e.to_string()))?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|e| BrocaError::Store(e.to_string()))?;
            std::fs::rename(&tmp, path).map_err(|e| BrocaError::Store(e.to_string()))?;
        }
        Ok(())
    }
    pub fn pending_next(&self) {
        lock(&self.state).pending_next = true;
    }
    pub fn cut_next_reply(&self) {
        lock(&self.state).cut_next_reply = true;
    }
    pub fn sends(&self) -> Vec<(Route, Vec<u8>)> {
        lock(&self.state).sends.clone()
    }
    pub fn subscriptions(&self) -> Vec<(Route, SubscribeParams)> {
        lock(&self.state).subscriptions.clone()
    }
    pub fn calls(&self) -> (usize, usize) {
        let s = lock(&self.state);
        (s.status_calls, s.read_calls)
    }
    pub fn keys(&self) -> Vec<String> {
        lock(&self.state).runs.keys().cloned().collect()
    }
    pub fn configure(
        &self,
        send_id: &str,
        archived: bool,
        drop_finish: bool,
        redeliver: bool,
        read_refusal: Option<String>,
    ) -> Result<(), BrocaError> {
        let mut s = lock(&self.state);
        let r = s
            .runs
            .get_mut(send_id)
            .ok_or_else(|| BrocaError::Invalid("unknown send id".into()))?;
        r.archived = archived;
        r.drop_finish = drop_finish;
        r.redeliver = redeliver;
        r.read_refusal = read_refusal;
        self.save(&s)
    }
    pub fn assistant(&self, send_id: &str, content: Vec<ContentBlock>) -> Result<(), BrocaError> {
        let mut s = lock(&self.state);
        let r = s
            .runs
            .get_mut(send_id)
            .ok_or_else(|| BrocaError::Invalid("unknown send id".into()))?;
        let ordinal = r.messages.len() as u64;
        r.messages.push(SessionReadMessage {
            ordinal,
            mid: format!("m:{ordinal}"),
            message: Message {
                role: "assistant".into(),
                content: content.clone(),
                cache_prefix_blocks: None,
                origin: None,
            },
        });
        push(
            r,
            ControlUnit::AssistantMessage {
                message: AssistantMessage {
                    message_id: format!("m:{ordinal}"),
                    content,
                },
            },
        );
        self.save(&s)
    }
    pub fn finish(
        &self,
        send_id: &str,
        text: &str,
        reason: RunFinishReason,
        usage: Option<Usage>,
    ) -> Result<(), BrocaError> {
        self.assistant(
            send_id,
            vec![
                ContentBlock::Reasoning {
                    text: "private reasoning".into(),
                    signature: None,
                },
                ContentBlock::Text { text: text.into() },
            ],
        )?;
        let mut s = lock(&self.state);
        let r = s
            .runs
            .get_mut(send_id)
            .ok_or_else(|| BrocaError::Invalid("unknown send id".into()))?;
        let metadata = Terminal {
            usage,
            ..Terminal::default()
        };
        r.status = match reason {
            RunFinishReason::Completed => RunStatusResponse::Completed {
                metadata: metadata.clone(),
            },
            RunFinishReason::MaxSteps => RunStatusResponse::MaxSteps {
                metadata: metadata.clone(),
            },
            RunFinishReason::Cancelled => RunStatusResponse::Cancelled {
                metadata: metadata.clone(),
            },
            RunFinishReason::Interrupted => RunStatusResponse::Interrupted {
                metadata: metadata.clone(),
            },
            RunFinishReason::Error => RunStatusResponse::Error {
                metadata: metadata.clone(),
            },
            RunFinishReason::TransformUnavailable => RunStatusResponse::TransformUnavailable {
                metadata: metadata.clone(),
            },
        };
        push(
            r,
            ControlUnit::RunFinished {
                run_id: Some(r.run_id.clone()),
                reason,
                metadata,
            },
        );
        self.save(&s)
    }
    pub fn set_status(&self, send_id: &str, status: RunStatusResponse) -> Result<(), BrocaError> {
        let mut s = lock(&self.state);
        s.runs
            .get_mut(send_id)
            .ok_or_else(|| BrocaError::Invalid("unknown send id".into()))?
            .status = status;
        self.save(&s)
    }
    pub fn forget_stream(&self, send_id: &str) -> Result<(), BrocaError> {
        let mut s = lock(&self.state);
        s.runs
            .get_mut(send_id)
            .ok_or_else(|| BrocaError::Invalid("unknown send id".into()))?
            .events
            .clear();
        self.save(&s)
    }
}
fn push(run: &mut Run, unit: ControlUnit) {
    let cursor = Cursor {
        wal_seq: run.events.len() as u64 / 2 + 1,
        sub_index: run.events.len() as u32 % 2,
    };
    run.events.push(SubscribeEvent::Control {
        cursor,
        unit: Box::new(unit),
    });
}
fn refused(code: &str) -> BrocaError {
    BrocaError::Refused {
        code: code.into(),
        detail: format!("fake refused {code}"),
    }
}
impl Transport for FakeBroca {
    fn send(&self, route: &Route, params: &[u8]) -> Result<SendResult, BrocaError> {
        let params_decoded: SendParams =
            serde_json::from_slice(params).map_err(|e| BrocaError::Wire(e.to_string()))?;
        let key = params_decoded
            .send_id
            .ok_or_else(|| refused("invalid_params"))?;
        let mut s = lock(&self.state);
        s.sends.push((route.clone(), params.to_vec()));
        if let Some(old) = s.runs.get(&key) {
            if old.bytes != params || &old.route != route {
                return Err(refused("send_id_reuse"));
            }
        } else {
            let pending = s.pending_next;
            s.pending_next = false;
            let mut run = Run {
                route: route.clone(),
                bytes: params.to_vec(),
                run_id: format!("run:{key}"),
                submission_id: format!("submission:{key}"),
                pending,
                status: RunStatusResponse::Active,
                events: vec![],
                messages: vec![],
                archived: false,
                read_refusal: None,
                drop_finish: false,
                redeliver: false,
            };
            push(
                &mut run,
                ControlUnit::RunStarted {
                    run_id: format!("run:{key}"),
                    submission_id: Some(format!("submission:{key}")),
                },
            );
            s.runs.insert(key.clone(), run);
        }
        let r = s
            .runs
            .get(&key)
            .ok_or_else(|| BrocaError::Wire("fake run disappeared".into()))?;
        let result = if let Some((reason, _)) = r.status.terminal() {
            SendResult::Finished {
                run_id: r.run_id.clone(),
                reason,
            }
        } else if r.pending {
            SendResult::Pending {
                submission_id: r.submission_id.clone(),
            }
        } else {
            SendResult::Active {
                run_id: r.run_id.clone(),
            }
        };
        let cut = s.cut_next_reply;
        s.cut_next_reply = false;
        self.save(&s)?;
        if cut {
            return Err(BrocaError::Unavailable {
                proven_unsent: false,
                detail: "reply lost after admission".into(),
            });
        }
        Ok(result)
    }
    fn subscribe(
        &self,
        route: &Route,
        params: &SubscribeParams,
    ) -> Result<Vec<SubscribeEvent>, BrocaError> {
        let mut s = lock(&self.state);
        s.subscriptions.push((route.clone(), params.clone()));
        let r = s
            .runs
            .values()
            .find(|r| &r.route == route)
            .ok_or_else(|| refused("not_found"))?;
        if r.archived {
            return Err(refused("cursor_expired"));
        }
        let from = match &params.from {
            Some(FromWire::Cursor(c)) => Some(*c),
            _ => None,
        };
        let result = r
            .events
            .iter()
            .filter(|e| match e {
                SubscribeEvent::Control { cursor, unit } => {
                    (r.redeliver || from.is_none_or(|c| *cursor > c))
                        && !(r.drop_finish && matches!(&**unit, ControlUnit::RunFinished { .. }))
                }
                _ => true,
            })
            .cloned()
            .collect();
        self.save(&s)?;
        Ok(result)
    }
    fn status(
        &self,
        route: &Route,
        params: &StatusParams,
    ) -> Result<RunStatusResponse, BrocaError> {
        let mut s = lock(&self.state);
        s.status_calls += 1;
        let status = s
            .runs
            .values()
            .find(|r| &r.route == route && params.run_id.as_ref() == Some(&r.run_id))
            .map(|r| r.status.clone())
            .unwrap_or(RunStatusResponse::Unknown);
        self.save(&s)?;
        Ok(status)
    }
    fn read(&self, route: &Route, _: &ReadParams) -> Result<SessionReadResponse, BrocaError> {
        let mut s = lock(&self.state);
        s.read_calls += 1;
        let r = s
            .runs
            .values()
            .find(|r| &r.route == route)
            .ok_or_else(|| refused("not_found"))?;
        if let Some(code) = &r.read_refusal {
            return Err(refused(code));
        }
        let usage = r.status.terminal().and_then(|(_, t)| t.usage.clone());
        let result = SessionReadResponse {
            messages: r.messages.iter().rev().take(1).cloned().collect(),
            next_from_ordinal: None,
            head: r.events.last().and_then(|e| match e {
                SubscribeEvent::Control { cursor, .. } => Some(*cursor),
                _ => None,
            }),
            lineage_id: Some(r.run_id.clone()),
            lineage_state: SessionReadLineageState {
                last_run_id: Some(r.run_id.clone()),
                state: if r.status.terminal().is_some() {
                    "completed"
                } else {
                    "active"
                }
                .into(),
                reason: None,
                error: None,
                usage,
            },
            tools: None,
            tools_run_id: None,
        };
        self.save(&s)?;
        Ok(result)
    }
}

/// Saved calls retained in memory while tests drop and reopen a host.
/// Process-kill tests use the core's SQLite implementation instead.
#[derive(Default)]
pub struct MemoryStore {
    calls: Mutex<BTreeMap<String, StoredCall>>,
}
impl StateStore for MemoryStore {
    fn load(&self) -> Result<Vec<StoredCall>, BrocaError> {
        Ok(lock(&self.calls).values().cloned().collect())
    }
    fn save(&self, call: &StoredCall) -> Result<(), BrocaError> {
        lock(&self.calls).insert(call.send_id.clone(), call.clone());
        Ok(())
    }
}
