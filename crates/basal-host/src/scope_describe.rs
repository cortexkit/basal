//! Admission-time attestations use a registered module's control connection,
//! not a consumer route: the daemon exposes `scope.describe` only to modules.

use std::fmt;
pub use subc_client_rs::ScopeCallError;
use subc_client_rs::{ModuleHandle, ScopeDescribeReply};
use subc_protocol::Principal;
use subc_protocol::scope::{ScopeStamp, ScopeStatus};

/// The complete daemon description, including the live stamp when present.
/// A missing stamp or a non-live status is an answer, not a transport error;
/// admission decides whether that answer attests the requested run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScopeDescription {
    pub status: ScopeStatus,
    pub scope_epoch: Option<u64>,
    pub daemon_incarnation: String,
    pub owner_synced: bool,
    pub owner_configured: bool,
    pub scope: Option<ScopeStamp>,
}

impl From<ScopeDescribeReply> for ScopeDescription {
    fn from(reply: ScopeDescribeReply) -> Self {
        Self {
            status: reply.status,
            scope_epoch: reply.scope_epoch,
            daemon_incarnation: reply.daemon_incarnation,
            owner_synced: reply.owner_synced,
            owner_configured: reply.owner_configured,
            scope: reply.scope,
        }
    }
}

/// No error supplies scope authority. The original typed daemon failure is
/// retained so admission can refuse an unverifiable scope without dispatching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeDescribeError {
    /// This host has no registered module connection to describe through.
    Unavailable,
    Daemon(ScopeCallError),
}

impl fmt::Display for ScopeDescribeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("scope describe connection is unavailable"),
            Self::Daemon(error) => write!(f, "scope describe: {error}"),
        }
    }
}

impl std::error::Error for ScopeDescribeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unavailable => None,
            Self::Daemon(error) => Some(error),
        }
    }
}

/// Blocking adapter for calls from the admission thread. The supplied runtime
/// must also drive the module's serve future while a describe reply is pending.
pub struct ModuleScopeDescriber {
    handle: ModuleHandle,
    runtime: tokio::runtime::Handle,
}

impl ModuleScopeDescriber {
    pub fn new(handle: ModuleHandle, runtime: tokio::runtime::Handle) -> Self {
        Self { handle, runtime }
    }

    pub fn describe(
        &self,
        owner: &Principal,
        scope_ref: &str,
    ) -> Result<ScopeDescription, ScopeDescribeError> {
        self.runtime
            .block_on(
                self.handle
                    .scope_describe(owner.clone(), scope_ref.to_owned()),
            )
            .map(ScopeDescription::from)
            .map_err(ScopeDescribeError::Daemon)
    }
}
