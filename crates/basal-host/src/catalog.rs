//! What the fleet declares: module events, module ops and known agents.
//!
//! basal validates a flow's manifest against this before it can be
//! approved (every event and version it binds to exists, every op it lists
//! exists and says whether it only reads, no op can run a shell, every
//! agent it names exists), and checks the shell marker again before each
//! dispatch, so an op that a module marks shell-capable after a flow was
//! approved is still refused. In production the catalog is the daemon's
//! module catalog; [`MockCatalog`] stands in for it in tests.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

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
}

/// The declarations basal validates manifests against. Implementations
/// must be safe to call from several threads.
pub trait Catalog: Send + Sync {
    fn supports_flow_scopes(&self, _module: &str) -> bool { false }
    /// The declaration of `module`'s event `name` at `version`, if declared.
    fn event(&self, module: &str, name: &str, version: u32) -> Option<EventDecl>;
    /// The declaration of `module`'s op `op`, if declared.
    fn op(&self, module: &str, op: &str) -> Option<OpDecl>;
    /// Whether `agent` names a known agent.
    fn agent_known(&self, agent: &str) -> bool;
}

#[derive(Default)]
struct Entries {
    flow_capable: BTreeSet<String>,
    events: BTreeMap<(String, String, u32), EventDecl>,
    ops: BTreeMap<(String, String), OpDecl>,
    agents: BTreeSet<String>,
}

/// An in-memory catalog. Cloning shares the same entries, so a test can
/// change a declaration after a flow was approved and watch the runtime
/// react.
#[derive(Clone, Default)]
pub struct MockCatalog {
    entries: Arc<RwLock<Entries>>,
}

impl MockCatalog {
    pub fn set_flow_capable(&self, module: &str, capable: bool) {
        if capable { self.write().flow_capable.insert(module.into()); }
        else { self.write().flow_capable.remove(module); }
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
                ..query.clone()
            },
        );
        c.set_op("plexus", "pr.get", query.clone());
        c.set_op("plexus", "pr.comment", mutate.clone());
        c.set_op(
            "plexus",
            "pr.label",
            OpDecl {
                cause_echo: true,
                ..mutate.clone()
            },
        );
        c.set_op(
            "aft",
            "bash",
            OpDecl {
                shell_capable: true,
                ..mutate.clone()
            },
        );
        c.set_event(
            "plexus",
            "pull_request_review",
            1,
            EventDecl {
                origin: EventOrigin::External,
                body: EventBody::Inline,
            },
        );
        c.set_event(
            "plexus",
            "pull_request",
            2,
            EventDecl {
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

    pub fn remove_op(&self, module: &str, op: &str) {
        self.write().ops.remove(&(module.to_owned(), op.to_owned()));
    }

    pub fn set_event(&self, module: &str, name: &str, version: u32, decl: EventDecl) {
        self.write()
            .events
            .insert((module.to_owned(), name.to_owned(), version), decl);
    }

    pub fn add_agent(&self, agent: &str) {
        self.write().agents.insert(agent.to_owned());
    }
}

impl Catalog for MockCatalog {
    fn supports_flow_scopes(&self, module: &str) -> bool { self.read().flow_capable.contains(module) }
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

    fn agent_known(&self, agent: &str) -> bool {
        self.read().agents.contains(agent)
    }
}
