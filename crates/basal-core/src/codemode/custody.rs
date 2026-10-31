//! One sleeping intake worker per provider, shared by all of basal's runs.

use super::*;
use basal_host::transport::RunToolOptions;
use serde::Deserialize;
use std::sync::Weak;

#[derive(Clone)]
pub(in crate::codemode::supervisor) struct Target {
    pub run: String,
    pub scope: ScopeIdentity,
    pub call: HostCall,
    pub events: mpsc::Sender<Event>,
    pub options: RunToolOptions,
    pub held: bool,
}
impl Target {
    fn is_held(&self) -> bool {
        self.held
    }
    fn route(&self) -> String {
        format!("codemode:{}", self.run)
    }
}

#[derive(Default)]
struct State {
    calls: BTreeMap<(String, String), Target>,
    live: BTreeSet<String>,
    // Keep each run's route alive until its page has been acknowledged, even
    // after the last call on that page has settled and left the call map.
    busy: BTreeMap<(String, String), mpsc::Sender<Event>>,
}

#[derive(Deserialize)]
struct Page {
    entries: Vec<Value>,
    cursor: Cursor,
    more: bool,
}
struct Pending {
    page: Page,
    routes: Vec<Target>,
    reread: bool,
}
#[derive(Default)]
struct Position {
    cursor: Option<Cursor>,
    pending: Option<Pending>,
}
#[derive(Default)]
struct Stream {
    position: Mutex<Position>,
    wake: Mutex<()>,
    changed: Condvar,
    stopped: AtomicBool,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Stream {
    fn notify(&self) {
        let _guard = self.wake.lock().unwrap();
        self.changed.notify_all();
    }
}

#[derive(Default)]
pub(in crate::codemode::supervisor) struct Custody {
    state: Mutex<State>,
    streams: Mutex<BTreeMap<String, Arc<Stream>>>,
    /// Called by a provider's intake worker each time it finds nothing held
    /// and is about to sleep until custody changes.
    #[cfg(test)]
    idle: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Custody {
    #[cfg(test)]
    pub(in crate::codemode::supervisor) fn on_idle(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.idle.lock().unwrap() = Some(hook);
    }
    fn stream(&self, module: &str) -> Arc<Stream> {
        self.streams
            .lock()
            .unwrap()
            .entry(module.into())
            .or_default()
            .clone()
    }
    fn notify(&self, module: &str) {
        self.stream(module).notify();
    }
    pub(in crate::codemode::supervisor) fn register(&self, module: &str, target: Target) {
        let mut state = self.state.lock().unwrap();
        state.live.insert(target.run.clone());
        state.calls.insert(
            (
                module.into(),
                store::call_key(&target.run, target.call.position),
            ),
            target,
        );
    }
    pub(in crate::codemode::supervisor) fn mark_held(
        &self,
        module: &str,
        run: &str,
        position: u64,
        held: bool,
    ) {
        if let Some(target) = self
            .state
            .lock()
            .unwrap()
            .calls
            .get_mut(&(module.into(), store::call_key(run, position)))
        {
            target.held = held;
        }
        self.notify(module);
    }
    pub(in crate::codemode::supervisor) fn has_held(&self, route: &str, module: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|((provider, _), target)| {
                provider == module && route == target.route() && target.is_held()
            })
    }
    fn held_target(&self, module: &str) -> Option<Target> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .find(|((provider, _), target)| provider == module && target.is_held())
            .map(|(_, target)| target.clone())
    }
    fn live(&self, run: &str) -> bool {
        self.state.lock().unwrap().live.contains(run)
    }
    pub(in crate::codemode::supervisor) fn remove(&self, module: &str, run: &str, position: u64) {
        self.state
            .lock()
            .unwrap()
            .calls
            .remove(&(module.into(), store::call_key(run, position)));
        self.notify(module);
    }
    pub(in crate::codemode::supervisor) fn end(&self, run: &str) {
        let mut state = self.state.lock().unwrap();
        state.live.remove(run);
        state.calls.retain(|_, target| target.run != run);
        state.busy.retain(|(_, owner), _| owner != run);
        drop(state);
        let streams: Vec<_> = self.streams.lock().unwrap().values().cloned().collect();
        for stream in streams {
            stream.notify();
        }
    }
    pub(in crate::codemode::supervisor) fn is_idle(&self, run: &str) -> bool {
        !self
            .state
            .lock()
            .unwrap()
            .busy
            .keys()
            .any(|(_, owner)| owner == run)
    }
    fn busy(&self, module: &str, target: &Target) {
        self.state
            .lock()
            .unwrap()
            .busy
            .insert((module.into(), target.run.clone()), target.events.clone());
    }
    fn finished(&self, module: &str) {
        let mut state = self.state.lock().unwrap();
        let senders: Vec<_> = state
            .busy
            .iter()
            .filter(|((provider, _), _)| provider == module)
            .map(|(_, sender)| sender.clone())
            .collect();
        state.busy.retain(|(provider, _), _| provider != module);
        drop(state);
        for sender in senders {
            let _ = sender.send(Event::CustodyChanged);
        }
    }
    fn candidate(&self, module: &str, stream: &Stream) -> Option<Target> {
        let position = stream.position.lock().unwrap();
        position
            .pending
            .as_ref()
            .and_then(|pending| {
                pending
                    .routes
                    .iter()
                    .find(|target| self.live(&target.run))
                    .cloned()
            })
            .or_else(|| self.held_target(module))
    }
    pub(in crate::codemode::supervisor) fn ensure_worker(
        self: &Arc<Self>,
        module: &str,
        transport: Arc<dyn Transport>,
    ) -> Result<()> {
        let stream = self.stream(module);
        let mut thread = stream.thread.lock().unwrap();
        if thread.is_none() {
            let weak = Arc::downgrade(self);
            let module = module.to_owned();
            let worker_stream = stream.clone();
            *thread = Some(
                std::thread::Builder::new()
                    .name(format!("codemode:late:{module}"))
                    .spawn(move || {
                        Self::work(weak, worker_stream, module, transport);
                    })
                    .map_err(|e| CoreError::Invalid(e.to_string()))?,
            );
        }
        drop(thread);
        stream.notify();
        Ok(())
    }
    fn work(weak: Weak<Self>, stream: Arc<Stream>, module: String, transport: Arc<dyn Transport>) {
        let mut due = Instant::now();
        let mut backoff = Duration::from_millis(25);
        loop {
            let mut wake = stream.wake.lock().unwrap();
            let target = loop {
                if stream.stopped.load(Ordering::SeqCst) {
                    return;
                }
                let Some(custody) = weak.upgrade() else {
                    return;
                };
                let target = custody.candidate(&module, &stream);
                #[cfg(test)]
                if target.is_none() {
                    let hook = custody.idle.lock().unwrap().clone();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                drop(custody);
                if let Some(target) = target {
                    let wait = due.saturating_duration_since(Instant::now());
                    if wait.is_zero() {
                        break target;
                    }
                    wake = stream.changed.wait_timeout(wake, wait).unwrap().0;
                } else {
                    wake = stream.changed.wait(wake).unwrap();
                    due = Instant::now();
                }
            };
            drop(wake);
            let Some(custody) = weak.upgrade() else {
                return;
            };
            let result = custody.pull(
                transport.as_ref(),
                &target.route(),
                &module,
                &target.options,
            );
            match result {
                Ok(true) => {
                    due = Instant::now();
                    backoff = Duration::from_millis(25);
                }
                Ok(false) => {
                    due = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                }
                Err(error) => {
                    if definitive_scope_end(&error) {
                        custody.end(&target.run);
                        let _ = target
                            .events
                            .send(Event::ScopeEnded("provider_scope_ended".into()));
                    } else {
                        tracing::warn!(provider = module, ?error, "late result intake will retry");
                    }
                    due = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                }
            }
        }
    }
    pub(in crate::codemode::supervisor) fn shutdown(&self) {
        let streams: Vec<_> = self.streams.lock().unwrap().values().cloned().collect();
        for stream in &streams {
            stream.stopped.store(true, Ordering::SeqCst);
            stream.notify();
        }
        for stream in streams {
            if let Some(thread) = stream.thread.lock().unwrap().take() {
                let _ = thread.join();
            }
        }
    }

    pub(in crate::codemode::supervisor) fn pull(
        &self,
        transport: &dyn Transport,
        route: &str,
        module: &str,
        options: &RunToolOptions,
    ) -> std::result::Result<bool, WireError> {
        let stream = self.stream(module);
        let mut position = stream.position.lock().unwrap();
        let reread = position
            .pending
            .as_ref()
            .is_some_and(|pending| pending.reread);
        if position.pending.is_none() || reread {
            if !reread && !self.has_held(route, module) {
                return Ok(false);
            }
            let target = position
                .pending
                .as_ref()
                .and_then(|pending| {
                    pending
                        .routes
                        .iter()
                        .find(|target| target.route() == route && self.live(&target.run))
                        .cloned()
                })
                .or_else(|| {
                    self.state
                        .lock()
                        .unwrap()
                        .calls
                        .values()
                        .find(|target| target.route() == route && target.is_held())
                        .cloned()
                });
            let Some(target) = target else {
                return Ok(false);
            };
            self.busy(module, &target);
            let mut request = LateResultsRequest::new().with_limit(100);
            request.since = position.cursor.clone();
            let reply = transport.role_for_run(
                route,
                module,
                "late_results",
                serde_json::to_value(request).unwrap(),
                options,
            );
            let value = match reply {
                Err(
                    WireError::Refused { ref code, .. }
                    | WireError::RefusedDetails { ref code, .. },
                ) if code == CURSOR_INCARNATION_CHANGED => {
                    position.cursor = None;
                    if !reread {
                        self.finished(module);
                    }
                    return Ok(true);
                }
                Ok(value) => value,
                Err(error) => {
                    if !reread {
                        self.finished(module);
                    }
                    return Err(error);
                }
            };
            let page: Page = match serde_json::from_value(value) {
                Ok(page) => page,
                Err(error) => {
                    if !reread {
                        self.finished(module);
                    }
                    return Err(WireError::Unreadable(error.to_string()));
                }
            };
            if page.entries.is_empty() {
                position.cursor = Some(page.cursor);
                position.pending = None;
                self.finished(module);
                return Ok(page.more);
            }
            let routes = position
                .pending
                .take()
                .map_or_else(|| vec![target], |pending| pending.routes);
            position.pending = Some(Pending {
                page,
                routes,
                reread: false,
            });
        }
        let pending = position.pending.as_mut().unwrap();
        let mut seen = BTreeSet::<IntakeIdentity>::new();
        for raw in &pending.page.entries {
            if !matches!(
                raw.get("kind").and_then(Value::as_str),
                Some("held" | "result" | "not_started" | "expired")
            ) {
                // Consume progress and future non-result entries without ending the call.
                tracing::debug!(
                    provider = module,
                    kind = ?raw.get("kind"),
                    "skipping non-terminal late entry"
                );
                continue;
            }
            if raw.get("custodian").and_then(Value::as_str) != Some("reserved:basal") {
                tracing::warn!(provider = module, "skipping foreign late-result custodian");
                continue;
            }
            let Some(call_key) = raw.get("call_key").and_then(Value::as_str) else {
                continue;
            };
            let target = self
                .state
                .lock()
                .unwrap()
                .calls
                .get(&(module.into(), call_key.into()))
                .cloned();
            if let Some(target) = target {
                if raw.get("owner").and_then(Value::as_str) != Some(target.scope.owner.as_str())
                    || raw.get("ref").and_then(Value::as_str)
                        != Some(target.scope.scope_ref.as_str())
                    || raw.get("scope_epoch").and_then(Value::as_u64)
                        != Some(target.scope.scope_epoch)
                {
                    tracing::warn!(provider = module, "skipping late entry from another scope");
                    continue;
                }
                let intake = intake_identity(module, raw);
                if !seen.insert(intake.clone()) {
                    continue;
                }
                let entry = match serde_json::from_value::<LateEntry>(raw.clone()) {
                    Ok(entry) => Ok(entry),
                    Err(error) => {
                        tracing::warn!(provider = module, ?error, "skipping malformed late entry");
                        if raw.get("kind").and_then(Value::as_str) != Some("held") {
                            continue;
                        }
                        Err(error.to_string())
                    }
                };
                self.busy(module, &target);
                if !pending.routes.iter().any(|route| route.run == target.run) {
                    pending.routes.push(target.clone());
                }
                let (ack, done) = mpsc::sync_channel(1);
                if target
                    .events
                    .send(Event::LateAnswer {
                        call: target.call.clone(),
                        entry,
                        intake,
                        ack,
                    })
                    .is_ok()
                {
                    let until = Instant::now() + Duration::from_secs(2);
                    loop {
                        match done.recv_timeout(Duration::from_millis(25)) {
                            Ok(()) => break,
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                // The target driver ended or failed intake. Its
                                // result cannot make another run lose custody.
                                self.remove(module, &target.run, target.call.position);
                                break;
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) if !self.live(&target.run) => {
                                break;
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < until => {
                                continue;
                            }
                            Err(_) => {
                                return Err(WireError::TimedOut(
                                    "late intake still pending".into(),
                                ));
                            }
                        }
                    }
                }
            }
        }
        let ack = transport.role_for_run(
            route,
            module,
            "late_results.ack",
            serde_json::to_value(AckRequest {
                through: pending.page.cursor.clone(),
            })
            .unwrap(),
            options,
        );
        if matches!(&ack, Err(WireError::Refused {code,..}|WireError::RefusedDetails {code,..}) if code==CURSOR_INCARNATION_CHANGED)
        {
            // A restart can invalidate an acknowledgement too. Keep the route
            // barrier while rereading and deduplicating the new incarnation.
            position.pending.as_mut().unwrap().reread = true;
            position.cursor = None;
            return Ok(true);
        }
        ack?;
        let pending = position.pending.take().unwrap();
        position.cursor = Some(pending.page.cursor);
        self.finished(module);
        Ok(pending.page.more)
    }
}
impl Drop for Custody {
    fn drop(&mut self) {
        for stream in self.streams.get_mut().unwrap().values() {
            stream.stopped.store(true, Ordering::SeqCst);
            stream.notify();
        }
    }
}

pub(in crate::codemode::supervisor) fn definitive_scope_end(error: &WireError) -> bool {
    use basal_host::flow_refusal::RefusalReason;
    match error {
        WireError::ScopeCut(_) => true,
        WireError::Typed(error) => matches!(
            error.reason,
            RefusalReason::ScopeEnded | RefusalReason::ScopeNotLive
        ),
        WireError::Refused { code, .. } | WireError::RefusedDetails { code, .. } => matches!(
            code.as_str(),
            "scope_ended" | "scope_expired" | "scope_parent_ended" | "scope_not_live"
        ),
        _ => false,
    }
}
