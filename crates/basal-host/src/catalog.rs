//! What the fleet declares: module events, module ops and known agents.
//!
//! basal validates a flow's manifest against this before it can be
//! approved (every event and version it binds to exists, every op it lists
//! exists and says whether it only reads, no op can run a shell, every
//! agent it names exists), and checks the shell marker again before each
//! dispatch, so an op that a module marks shell-capable after a flow was
//! approved is still refused. In production the catalog is the daemon's
//! module catalog; [`MockCatalog`] stands in for it in tests.

#[cfg(any(test, feature = "test-support"))]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Read-only tool used by install validation and the event-body fetch that
/// runs before an event-triggered script starts.
pub const EVENT_BODY_OP: &str = "events_get";

/// The marker a module declares on an op: whether it only reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Query,
    Mutate,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Mutate => "mutate",
        }
    }
}

/// One module op as its module declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpDecl {
    /// `None` when the module did not mark the op. A flow may not list an
    /// unmarked op: nothing would say whether calling it changes anything.
    pub kind: Option<OpKind>,
    /// The op copies a call's cause into the events it emits as a
    /// consequence, so loop protection can follow it.
    pub cause_echo: bool,
    /// The op can run commands (a shell, or something that wraps one). A
    /// flow never reaches such an op, listed or not.
    pub shell_capable: bool,
}

/// Whether an event's payload carries text from outside the fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOrigin {
    External,
    Internal,
}

/// Where an event's body lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventBody {
    /// The payload carries the body.
    Inline,
    /// The payload carries an id; the module's `resolve_op` reads the body.
    Resolved { resolve_op: String },
}

/// One versioned event as its module declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventDecl {
    pub origin: EventOrigin,
    pub body: EventBody,
    /// Header names the publisher allows in admission filters. Their values
    /// can skip notices, but do not prove anything about the verified body.
    pub headers: Vec<String>,
}

/// The refusal code for an op that needed core's agent registry while it
/// could not be read (core restarting, a timeout, an undecodable reply). It
/// is never a verdict about the agent: the caller should try again.
pub const AGENT_REGISTRY_UNAVAILABLE: &str = "agent_registry_unavailable";

/// The declarations basal validates manifests against. Implementations
/// must be safe to call from several threads.
pub trait Catalog: Send + Sync {
    fn supports_flow_scopes(&self, _module: &str) -> bool {
        false
    }
    /// The declaration of `module`'s event `name` at `version`, if declared.
    fn event(&self, module: &str, name: &str, version: u32) -> Option<EventDecl>;
    /// The declaration of `module`'s op `op`, if declared.
    fn op(&self, module: &str, op: &str) -> Option<OpDecl>;
    /// Whether calling `module`'s tool `op` can make `module` emit an event,
    /// as the module declares it on the tool in its own `tool.catalog`
    /// answer. `Ok(Some(false))` is the module's promise that no call to the
    /// tool, with any arguments, makes it emit or relay an event. `Ok(None)`
    /// means nothing was declared: the field is absent, the tool is not in
    /// the answer, or the op is not a tool. Callers must treat `None` like
    /// `Some(true)`. `Err` means the answer could not be read or decoded, so
    /// nothing is known and a retry may help.
    ///
    /// Install's loop rule refuses a mutation on a module whose events
    /// trigger the flow, since the call could trigger the flow again; a tool
    /// declaring `Some(false)` is exempt. This can be a live call to the
    /// module, so it is read only for an op that rule would otherwise
    /// refuse, never on dispatch.
    fn emits_events(
        &self,
        _module: &str,
        _op: &str,
    ) -> Result<Option<bool>, crate::transport::WireError> {
        Ok(None)
    }
    /// Whether `agent` names a known agent. `Ok(false)` means the registry
    /// answered without it; a failed registry read must remain an error.
    fn agent_known(&self, agent: &str) -> Result<bool, crate::transport::WireError>;
    /// Resolves an agent's display name or stable id to its stable id.
    /// `Ok(None)` means the registry answered and no agent has that name or
    /// id. A failed registry read must remain an error: it says nothing about
    /// whether the agent exists, so callers must not treat it as unknown.
    fn agent_id(&self, agent: &str) -> Result<Option<String>, crate::transport::WireError>;
    /// Resolve related references from one registry read. An answered unknown
    /// reference is `None`; a failed registry read must remain an error.
    fn resolve_agents(
        &self,
        agents: &[&str],
    ) -> Result<Vec<Option<String>>, crate::transport::WireError>;
    /// The workspace registry must be readable before a workspace audience is admitted.
    fn workspace_known(&self, _id: &str) -> Result<bool, crate::transport::WireError> {
        Err(crate::transport::WireError::Unreadable(
            "workspace registry unavailable".into(),
        ))
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct Entries {
    flow_capable: BTreeSet<String>,
    events: BTreeMap<(String, String, u32), EventDecl>,
    ops: BTreeMap<(String, String), OpDecl>,
    /// What each tool declares for `emits_events`, or the error reading it
    /// returns. A tool without an entry declares nothing.
    emits_events: BTreeMap<(String, String), Result<Option<bool>, crate::transport::WireError>>,
    /// Every `emits_events` read, in order, so a test can see which tools
    /// install asked about.
    emits_events_reads: Vec<(String, String)>,
    agents: BTreeMap<String, String>,
    workspaces: BTreeSet<String>,
    /// When set, every agent registry read fails with this error, as when
    /// core's `agent.list` cannot be read.
    agent_registry_failure: Option<crate::transport::WireError>,
}

/// An in-memory catalog. Cloning shares the same entries, so a test can
/// change a declaration after a flow was approved and watch the runtime
/// react.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Default)]
pub struct MockCatalog {
    entries: Arc<RwLock<Entries>>,
}

#[cfg(any(test, feature = "test-support"))]
impl MockCatalog {
    pub fn set_flow_capable(&self, module: &str, capable: bool) {
        if capable {
            self.write().flow_capable.insert(module.into());
        } else {
            self.write().flow_capable.remove(module);
        }
    }
    pub fn new() -> Self {
        Self::default()
    }

    /// The catalog the test suites use: the mock host's ops, a GitHub-like
    /// event source with a mutation that does not echo causes, the shell
    /// tools the flow profile must never reach, and a few agents.
    pub fn standard() -> Self {
        let c = Self::new();
        let query = OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        };
        let mutate = OpDecl {
            kind: Some(OpKind::Mutate),
            cause_echo: false,
            shell_capable: false,
        };
        for op in ["echo", "fail", "unlisted"] {
            c.set_op("mock", op, query.clone());
        }
        for op in ["send", "long", "post"] {
            c.set_op("mock", op, mutate.clone());
        }
        c.set_op(
            "mock",
            "unmarked",
            OpDecl {
                kind: None,
                ..query
            },
        );
        c.set_op("plexus", "pr.get", query);
        c.set_op("plexus", "pr.comment", mutate.clone());
        c.set_op(
            "plexus",
            "pr.label",
            OpDecl {
                cause_echo: true,
                ..mutate
            },
        );
        c.set_op(
            "aft",
            "bash",
            OpDecl {
                shell_capable: true,
                ..mutate
            },
        );
        c.set_event(
            "plexus",
            "pull_request_review",
            1,
            EventDecl {
                headers: vec![],
                origin: EventOrigin::External,
                body: EventBody::Inline,
            },
        );
        c.set_event(
            "plexus",
            "pull_request",
            2,
            EventDecl {
                headers: vec![],
                origin: EventOrigin::External,
                body: EventBody::Resolved {
                    resolve_op: "pr.get".into(),
                },
            },
        );
        for agent in ["SYNAPSE", "BASAL", "ALF", "SUBC"] {
            c.add_agent(agent);
        }
        c
    }

    fn read(&self) -> RwLockReadGuard<'_, Entries> {
        // A panic in another test thread must not make the catalog unusable.
        self.entries.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Entries> {
        self.entries.write().unwrap_or_else(|p| p.into_inner())
    }

    pub fn set_op(&self, module: &str, op: &str, decl: OpDecl) {
        self.write()
            .ops
            .insert((module.to_owned(), op.to_owned()), decl);
    }

    /// Sets what `module`'s tool `op` declares for `emits_events`; `None`
    /// leaves the field absent.
    pub fn set_emits_events(&self, module: &str, op: &str, emits_events: Option<bool>) {
        self.write()
            .emits_events
            .insert((module.to_owned(), op.to_owned()), Ok(emits_events));
    }

    /// Makes reading `module`'s tool `op` declaration fail with `failure`,
    /// as when the module's `tool.catalog` answer cannot be read.
    pub fn fail_emits_events(&self, module: &str, op: &str, failure: crate::transport::WireError) {
        self.write()
            .emits_events
            .insert((module.to_owned(), op.to_owned()), Err(failure));
    }

    /// Every `(module, op)` whose `emits_events` was read, in order.
    pub fn emits_events_reads(&self) -> Vec<(String, String)> {
        self.read().emits_events_reads.clone()
    }

    pub fn set_event(&self, module: &str, name: &str, version: u32, decl: EventDecl) {
        self.write()
            .events
            .insert((module.to_owned(), name.to_owned(), version), decl);
    }

    pub fn add_agent(&self, agent: &str) {
        self.add_named_agent(agent, agent);
    }

    /// Makes every later agent registry read fail with `failure`, or answer
    /// again when `None`.
    pub fn fail_agent_registry(&self, failure: Option<crate::transport::WireError>) {
        self.write().agent_registry_failure = failure;
    }

    pub fn add_workspace(&self, id: &str) {
        self.write().workspaces.insert(id.to_owned());
    }

    pub fn add_named_agent(&self, agent_id: &str, name: &str) {
        let mut entries = self.write();
        entries
            .agents
            .insert(agent_id.to_owned(), agent_id.to_owned());
        entries.agents.insert(name.to_owned(), agent_id.to_owned());
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Catalog for MockCatalog {
    fn workspace_known(&self, id: &str) -> Result<bool, crate::transport::WireError> {
        Ok(self.read().workspaces.contains(id))
    }
    fn supports_flow_scopes(&self, module: &str) -> bool {
        self.read().flow_capable.contains(module)
    }
    fn event(&self, module: &str, name: &str, version: u32) -> Option<EventDecl> {
        self.read()
            .events
            .get(&(module.to_owned(), name.to_owned(), version))
            .cloned()
    }

    fn op(&self, module: &str, op: &str) -> Option<OpDecl> {
        self.read()
            .ops
            .get(&(module.to_owned(), op.to_owned()))
            .cloned()
    }

    fn emits_events(
        &self,
        module: &str,
        op: &str,
    ) -> Result<Option<bool>, crate::transport::WireError> {
        let key = (module.to_owned(), op.to_owned());
        let mut entries = self.write();
        entries.emits_events_reads.push(key.clone());
        entries.emits_events.get(&key).cloned().unwrap_or(Ok(None))
    }

    fn agent_known(&self, agent: &str) -> Result<bool, crate::transport::WireError> {
        Ok(self.agent_id(agent)?.is_some())
    }

    fn agent_id(&self, agent: &str) -> Result<Option<String>, crate::transport::WireError> {
        let entries = self.read();
        if let Some(failure) = &entries.agent_registry_failure {
            return Err(failure.clone());
        }
        Ok(entries.agents.get(agent).cloned())
    }
    fn resolve_agents(
        &self,
        agents: &[&str],
    ) -> Result<Vec<Option<String>>, crate::transport::WireError> {
        let entries = self.read();
        if let Some(failure) = &entries.agent_registry_failure {
            return Err(failure.clone());
        }
        Ok(agents
            .iter()
            .map(|agent| entries.agents.get(*agent).cloned())
            .collect())
    }
}
