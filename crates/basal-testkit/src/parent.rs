//! A test parent: spawns the real worker, serves its host calls from the
//! mock, keeps an in-memory journal per run, and replays it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use basal_proto::{
    ActivationRequest, ActivationResult, ArgsDigest, Budgets, CallKind, HostCall, JsonText,
    Outcome, ParentMessage, PreludeHash, Profile, RecordedCall, RecordedOutcome, Refusal,
    Settlement, Welcome, WorkerMessage,
};

use crate::mock::{Answer, MockHost};
use crate::process::{ParentError, WorkerProcess};

/// One journaled call.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    pub position: u64,
    pub kind: CallKind,
    pub args: JsonText,
    pub outcome: Option<RecordedOutcome>,
}

/// A call dispatched to the mock and not yet delivered.
#[derive(Debug, Clone)]
struct Pending {
    ready_at: u64,
    /// `None` while a long-running call has not been completed.
    answer: Option<(Settlement, JsonText, Duration)>,
}

/// A run's journal and its in-flight calls.
#[derive(Debug, Clone, Default)]
pub struct Journal {
    pub entries: Vec<JournalEntry>,
    pub next_order: u64,
    pending: BTreeMap<u64, Pending>,
    tick: u64,
}

impl Journal {
    /// The recorded prefix as the worker receives it.
    pub fn prefix(&self) -> Vec<RecordedCall> {
        self.entries
            .iter()
            .map(|e| RecordedCall {
                position: e.position,
                kind: e.kind.clone(),
                args_digest: ArgsDigest::of(&e.args),
                outcome: e.outcome.clone(),
            })
            .collect()
    }

    /// Keeps only the first `calls` entries, as a crash before later records
    /// would. Dropped calls are forgotten entirely.
    pub fn truncate(&mut self, calls: usize) {
        self.entries.truncate(calls);
        self.pending.retain(|p, _| (*p as usize) < calls);
        self.next_order = self
            .entries
            .iter()
            .filter_map(|e| e.outcome.as_ref().map(|o| o.delivery_order + 1))
            .max()
            .unwrap_or(0);
    }

    fn record_outcome(&mut self, position: u64, settlement: Settlement, value: JsonText) -> u64 {
        let order = self.next_order;
        self.next_order += 1;
        if let Some(entry) = self.entries.get_mut(position as usize) {
            entry.outcome = Some(RecordedOutcome {
                settlement,
                value,
                delivery_order: order,
            });
        }
        order
    }
}

/// How an activation ended, from the parent's side.
#[derive(Debug, Clone, PartialEq)]
pub enum Ending {
    Finished(ActivationResult),
    /// The worker did not finish within the wall-clock deadline and was
    /// killed (the parent's backstop for a runaway engine).
    Hung,
    /// The channel failed: the worker died or sent garbage.
    Broken(ParentError),
}

/// What happened during one activation.
#[derive(Debug, Clone)]
pub struct Report {
    pub ending: Ending,
    /// Calls that crossed the channel during this activation.
    pub host_calls: Vec<HostCall>,
    /// Frames the worker refused (a test parent bug if any appear unasked).
    pub refusals: Vec<Refusal>,
    pub wall: Duration,
}

impl Report {
    pub fn result(&self) -> Option<&ActivationResult> {
        match &self.ending {
            Ending::Finished(r) => Some(r),
            _ => None,
        }
    }

    /// The completed value parsed as JSON. Panics with the whole report if
    /// the activation did not complete, which is what a test wants.
    pub fn value(&self) -> serde_json::Value {
        match &self.ending {
            Ending::Finished(ActivationResult::Completed { value }) => {
                serde_json::from_str(value.as_str())
                    .unwrap_or_else(|e| panic!("result is not JSON ({e}): {value:?}"))
            }
            _ => panic!("activation did not complete: {self:#?}"),
        }
    }
}

/// The test parent.
pub struct TestParent {
    binary: PathBuf,
    worker: Option<WorkerProcess>,
    pub welcome: Option<Welcome>,
    pub host: MockHost,
    pub journals: HashMap<String, Journal>,
    pub profile: Profile,
    pub budgets: Budgets,
    pub trigger: JsonText,
    /// The parent's wall-clock backstop per activation.
    pub deadline: Duration,
    next_activation: u64,
}

impl TestParent {
    pub fn new(binary: impl AsRef<Path>) -> Self {
        Self {
            binary: binary.as_ref().to_path_buf(),
            worker: None,
            welcome: None,
            host: MockHost::default(),
            journals: HashMap::new(),
            profile: Profile::Flow,
            budgets: Budgets::default(),
            trigger: JsonText::null(),
            deadline: Duration::from_secs(20),
            next_activation: 1,
        }
    }

    /// The live worker, spawning and greeting one if needed.
    pub fn worker(&mut self) -> Result<&mut WorkerProcess, ParentError> {
        if self.worker.is_none() {
            let (worker, welcome) = WorkerProcess::start(&self.binary, Duration::from_secs(10))?;
            self.welcome = Some(welcome);
            self.worker = Some(worker);
        }
        self.worker
            .as_mut()
            .ok_or_else(|| ParentError::Protocol("no worker".into()))
    }

    /// Kills the current worker; the next activation spawns a fresh one.
    pub fn kill_worker(&mut self) {
        if let Some(mut w) = self.worker.take() {
            w.kill();
        }
    }

    pub fn journal(&self, run: &str) -> &Journal {
        static EMPTY: std::sync::OnceLock<Journal> = std::sync::OnceLock::new();
        self.journals
            .get(run)
            .unwrap_or_else(|| EMPTY.get_or_init(Journal::default))
    }

    pub fn journal_mut(&mut self, run: &str) -> &mut Journal {
        self.journals.entry(run.to_owned()).or_default()
    }

    /// Completes a long-running call; the next activation can take it.
    pub fn complete(&mut self, run: &str, position: u64, settlement: Settlement, value: JsonText) {
        let journal = self.journal_mut(run);
        let tick = journal.tick;
        journal.pending.insert(
            position,
            Pending {
                ready_at: tick,
                answer: Some((settlement, value, Duration::ZERO)),
            },
        );
    }

    /// The prelude hash the worker reported, which activations must quote.
    fn prelude_hash(&self) -> PreludeHash {
        self.welcome
            .as_ref()
            .map(|w| w.prelude_hash)
            .unwrap_or(PreludeHash([0; 32]))
    }

    /// Runs the script for `run`, replaying whatever the run has journaled.
    pub fn run(&mut self, run: &str, script: &str) -> Report {
        let prefix = self.journal(run).prefix();
        self.run_with_prefix(run, script, prefix)
    }

    /// Runs with an explicit prefix, for tests that tamper with the journal
    /// as shipped (the journal itself still records new calls).
    pub fn run_with_prefix(
        &mut self,
        run: &str,
        script: &str,
        prefix: Vec<RecordedCall>,
    ) -> Report {
        let started = Instant::now();
        let mut report = Report {
            ending: Ending::Hung,
            host_calls: Vec::new(),
            refusals: Vec::new(),
            wall: Duration::ZERO,
        };
        if let Err(e) = self.worker() {
            report.ending = Ending::Broken(e);
            return report;
        }
        let request = ActivationRequest {
            activation_id: self.next_activation,
            profile: self.profile,
            prelude_hash: self.prelude_hash(),
            script: script.to_owned(),
            trigger: self.trigger.clone(),
            budgets: self.budgets,
            prefix,
        };
        self.next_activation += 1;
        report.ending = self.drive(run, request, &mut report.host_calls, &mut report.refusals);
        report.wall = started.elapsed();
        if !matches!(report.ending, Ending::Finished(_)) {
            // A hung or broken worker is never reused.
            self.kill_worker();
        }
        report
    }

    fn drive(
        &mut self,
        run: &str,
        request: ActivationRequest,
        host_calls: &mut Vec<HostCall>,
        refusals: &mut Vec<Refusal>,
    ) -> Ending {
        let deadline = Instant::now() + self.deadline;
        let activation_id = request.activation_id;
        let mut journal = self.journals.remove(run).unwrap_or_default();
        // Calls recorded without an outcome were in flight when the journal
        // was cut. Long-running ones stay pending; anything else is
        // dispatched again, as a recovering parent would for a query.
        for entry in &journal.entries {
            if entry.outcome.is_none() && !journal.pending.contains_key(&entry.position) {
                let call = HostCall {
                    position: entry.position,
                    kind: entry.kind.clone(),
                    args: entry.args.clone(),
                };
                if !call.kind.is_synchronous() {
                    let pending = pending_for(&mut self.host, &call, journal.tick);
                    journal.pending.insert(entry.position, pending);
                }
            }
        }
        let ending = (|| {
            let worker = match self.worker.as_mut() {
                Some(w) => w,
                None => return Ending::Broken(ParentError::Protocol("no worker".into())),
            };
            if let Err(e) = worker.send(&ParentMessage::Activate(Box::new(request))) {
                return Ending::Broken(e);
            }
            loop {
                let now = Instant::now();
                if now >= deadline {
                    return Ending::Hung;
                }
                let message = match worker.recv(deadline - now) {
                    Ok(m) => m,
                    Err(ParentError::Timeout) => return Ending::Hung,
                    Err(e) => return Ending::Broken(e),
                };
                let reply = match message {
                    WorkerMessage::Finished {
                        activation_id: id,
                        result,
                    } => {
                        if id != activation_id {
                            return Ending::Broken(ParentError::Protocol(format!(
                                "result for activation {id}, expected {activation_id}"
                            )));
                        }
                        return Ending::Finished(result);
                    }
                    WorkerMessage::Refused(r) => {
                        refusals.push(r);
                        continue;
                    }
                    WorkerMessage::HostCall(call) => {
                        host_calls.push(call.clone());
                        if call.position != journal.entries.len() as u64 {
                            // A sync call recorded without an outcome comes
                            // back at its old position; anything else must
                            // be the next position.
                            let reissued = journal
                                .entries
                                .get(call.position as usize)
                                .is_some_and(|e| e.outcome.is_none() && e.kind == call.kind);
                            if !reissued {
                                return Ending::Broken(ParentError::Protocol(format!(
                                    "call at position {}, journal has {}",
                                    call.position,
                                    journal.entries.len()
                                )));
                            }
                        } else {
                            journal.entries.push(JournalEntry {
                                position: call.position,
                                kind: call.kind.clone(),
                                args: call.args.clone(),
                                outcome: None,
                            });
                        }
                        if call.kind.is_synchronous() {
                            let (settlement, value) = self.host.answer_sync(&call);
                            let order =
                                journal.record_outcome(call.position, settlement, value.clone());
                            Some(ParentMessage::Deliver(Outcome {
                                position: call.position,
                                settlement,
                                value,
                                delivery_order: order,
                            }))
                        } else {
                            let pending = pending_for(&mut self.host, &call, journal.tick);
                            journal.pending.insert(call.position, pending);
                            None
                        }
                    }
                    WorkerMessage::Blocked { awaiting } => {
                        let ready = awaiting
                            .iter()
                            .filter_map(|p| {
                                journal.pending.get(p).and_then(|pending| {
                                    pending.answer.as_ref().map(|_| (pending.ready_at, *p))
                                })
                            })
                            .min();
                        match ready {
                            Some((ready_at, position)) => {
                                let Some(pending) = journal.pending.remove(&position) else {
                                    return Ending::Broken(ParentError::Protocol(
                                        "pending call vanished".into(),
                                    ));
                                };
                                let Some((settlement, value, sleep)) = pending.answer else {
                                    return Ending::Broken(ParentError::Protocol(
                                        "answer vanished".into(),
                                    ));
                                };
                                if !sleep.is_zero() {
                                    std::thread::sleep(sleep);
                                }
                                journal.tick = journal.tick.max(ready_at);
                                let order =
                                    journal.record_outcome(position, settlement, value.clone());
                                Some(ParentMessage::Deliver(Outcome {
                                    position,
                                    settlement,
                                    value,
                                    delivery_order: order,
                                }))
                            }
                            None => {
                                let long: Vec<u64> = awaiting
                                    .iter()
                                    .copied()
                                    .filter(|p| {
                                        journal.pending.get(p).is_some_and(|x| x.answer.is_none())
                                    })
                                    .collect();
                                if long.is_empty() {
                                    return Ending::Broken(ParentError::Protocol(format!(
                                        "blocked on {awaiting:?}, which the parent knows nothing about"
                                    )));
                                }
                                Some(ParentMessage::LongRunning { positions: long })
                            }
                        }
                    }
                    WorkerMessage::Welcome(_) => {
                        return Ending::Broken(ParentError::Protocol("unexpected Welcome".into()));
                    }
                };
                if let Some(reply) = reply
                    && let Err(e) = worker.send(&reply)
                {
                    return Ending::Broken(e);
                }
            }
        })();
        self.journals.insert(run.to_owned(), journal);
        ending
    }
}

fn pending_for(host: &mut MockHost, call: &HostCall, tick: u64) -> Pending {
    match host.answer(call) {
        Answer::Ready {
            delay_ticks,
            settlement,
            value,
            sleep,
        } => Pending {
            ready_at: tick + delay_ticks,
            answer: Some((settlement, value, sleep)),
        },
        Answer::LongRunning => Pending {
            ready_at: tick,
            answer: None,
        },
    }
}
