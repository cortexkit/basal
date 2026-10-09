//! The file, git and network built-ins: `fs.read`, `fs.list`, `fs.stat`,
//! `fs.write`, the git reads and `net.fetch`.
//!
//! The QuickJS worker has no file or socket access, and keeps none. A
//! script reaches files, repositories and the network only through these
//! host calls, which basal's parent carries out itself. Each built-in is a
//! permission line in the flow's manifest (`fs`, `git`, `net`). The parent
//! checks every call against the run's approved manifest before it is
//! journaled ([`authorize`]), and the call travels to [`BuiltinHost`] with
//! the scope it was granted ([`Grant`]), so the host checks the same scope
//! again at the moment it acts. Like every host call, a built-in's result is
//! journaled, so a replay returns the recorded result and never reads,
//! runs or fetches again.
//!
//! The dispatch class of each call ([`class`]) is what the journal does
//! with a call whose outcome it never learned:
//! - `fs.read`, `fs.list`, `fs.stat`, every git read, and `net.fetch` with
//!   `GET` or `HEAD` are queries, sent again after a crash;
//! - `fs.write` is a mutation that honours its key: writing the same bytes
//!   again leaves the same file, so it is sent again under the same key;
//! - `net.fetch` with any other method is a mutation that does not, so an
//!   unknown outcome stops the run for the operator to reconcile.

pub mod fs;
pub mod git;
pub mod net;

use std::path::PathBuf;
use std::sync::Arc;

use basal_proto::{CallKind, JsonText, Primitive};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::core_host::{sample, system_now};
use crate::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, TransportError,
};

pub use net::{HostRule, NetConfig, Resolve, StaticResolver, SystemResolver};

/// The rejection codes the built-ins answer with, as the script sees them
/// in `e.data.code`.
pub mod codes {
    /// Not granted by the manifest, or outside the granted scope.
    pub const DENIED: &str = "denied";
    /// Arguments that do not have the call's shape.
    pub const INVALID_ARGUMENTS: &str = "invalid_arguments";
    /// The file, directory, revision or repository does not exist.
    pub const NOT_FOUND: &str = "not_found";
    /// A file, listing, git output or response body over its cap.
    pub const TOO_LARGE: &str = "too_large";
    /// Text that is not UTF-8.
    pub const NOT_UTF8: &str = "not_utf8";
    /// A file system error other than the above.
    pub const IO: &str = "io_error";
    /// git ran and failed.
    pub const GIT: &str = "git_failed";
    /// The server's certificate did not verify.
    pub const TLS: &str = "tls_failed";
    /// A response basal cannot read, or a redirect it will not follow.
    pub const NET: &str = "net_failed";
    /// The operation ran past its time limit.
    pub const TIMEOUT: &str = "timeout";
    /// The call's dispatch class no longer matches the one journaled with it.
    pub const CLASS_CHANGED: &str = "class_changed";
}

/// Why a built-in call was refused or failed with a definite answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    pub code: &'static str,
    pub message: String,
}

impl Denial {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(codes::DENIED, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_ARGUMENTS, message)
    }

    /// The rejection a script sees.
    pub fn outcome(&self) -> HostOutcome {
        HostOutcome::rejected(text(&json!({ "message": self.message, "code": self.code })).unwrap_or_else(|_| {
            JsonText::new(json!({"message":"built-in result exceeds the encoded limit", "code":codes::TOO_LARGE}).to_string()).expect("small host-owned refusal")
        }))
    }
}

/// How a built-in call ended without a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A definite answer: the call was refused, or it ran and failed.
    Refused(Denial),
    /// The network could not carry the call; whether a request may have
    /// reached the server decides what the runtime may do next.
    Transport(TransportError),
}

impl From<Denial> for Failure {
    fn from(d: Denial) -> Self {
        Self::Refused(d)
    }
}

fn text(value: &Value) -> Result<JsonText, Denial> {
    JsonText::new(value.to_string()).map_err(|_| {
        Denial::new(
            codes::TOO_LARGE,
            "built-in result exceeds the encoded limit",
        )
    })
}

// A JSON control character expands to six bytes. Reserve room for a response's
// escaped headers and framing before assigning the body or file-text budget.
pub const MAX_TEXT_RESULT_BYTES: usize =
    (basal_proto::MAX_VALUE_BYTES - 6 * net::MAX_HEAD_BYTES - 4096) / 6;

/// What the approved manifest grants one call, carried with it from the
/// parent's check to the host. For the `fs` built-ins `roots` holds the
/// roots of the call's kind (read roots for reads, write roots for
/// `fs.write`); for git it holds the approved repositories; for
/// `net.fetch`, `hosts` holds every approved host with its methods. Paths
/// are absolute with `~` already expanded, not yet resolved: the host
/// resolves them again each time it acts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<HostRule>,
}

/// Whether `kind` is a file, git or network built-in.
pub fn is_builtin(kind: &CallKind) -> bool {
    matches!(kind, CallKind::Primitive(p) if p.is_builtin())
}

/// The dispatch class of a built-in call, from the script's arguments.
/// `None` when `kind` is not a built-in. Arguments `net.fetch` cannot read
/// a method from are classed as a mutation that does not honour keys, the
/// class the runtime never sends twice; such a call is refused before it is
/// sent anyway.
pub fn class(kind: &CallKind, args: &Value) -> Option<CallClass> {
    let CallKind::Primitive(p) = kind else {
        return None;
    };
    match p {
        Primitive::FsRead
        | Primitive::FsList
        | Primitive::FsStat
        | Primitive::GitLog
        | Primitive::GitRevParse
        | Primitive::GitDescribeTags
        | Primitive::GitShow
        | Primitive::GitDiff => Some(CallClass::Query),
        Primitive::FsWrite => Some(CallClass::Mutation {
            honours_idempotency_keys: true,
        }),
        Primitive::NetFetch => Some(match net::method_of(args) {
            Ok(m) if net::is_query_method(&m) => CallClass::Query,
            _ => CallClass::Mutation {
                honours_idempotency_keys: false,
            },
        }),
        _ => None,
    }
}

/// The dispatch class of a built-in call from its journaled request (the
/// envelope [`envelope`] builds).
pub fn request_class(kind: &CallKind, request: &Value) -> Option<CallClass> {
    class(kind, request.get("args").unwrap_or(&Value::Null))
}

/// The request journaled and dispatched for a built-in call: the script's
/// arguments and the scope the manifest grants it.
pub fn envelope(args: &Value, grant: &Grant) -> Value {
    json!({ "args": args, "grant": grant })
}

/// Expands a leading `~` to `$HOME`. Returns `None` for a path that is not
/// absolute after expansion, or when `$HOME` is unset or relative.
pub fn expand_home(text: &str) -> Option<PathBuf> {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())
    };
    if text == "~" {
        return home();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home().map(|h| h.join(rest));
    }
    let path = PathBuf::from(text);
    path.is_absolute().then_some(path)
}

/// One built-in call, its arguments checked for shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    FsRead { path: String, max_bytes: u64 },
    FsList { path: String },
    FsStat { path: String },
    FsWrite { path: String, text: String },
    Git { repo: String, op: git::Op },
    NetFetch(net::Request),
}

/// An options argument: absent, null or an object whose keys are all
/// among `known`. Unknown keys are refused so a misspelt option is an
/// error rather than silently ignored.
pub(crate) fn options<'a>(
    args: &'a Value,
    field: &str,
    known: &[&str],
) -> Result<Option<&'a Map<String, Value>>, Denial> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(o)) => {
            if let Some(k) = o.keys().find(|k| !known.contains(&k.as_str())) {
                return Err(Denial::invalid(format!("unknown option {k:?}")));
            }
            Ok(Some(o))
        }
        Some(_) => Err(Denial::invalid(format!("{field} must be an object"))),
    }
}

pub(crate) fn string_arg(args: &Value, field: &str) -> Result<String, Denial> {
    args.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Denial::invalid(format!("{field} must be a string")))
}

/// Reads a built-in call's arguments.
pub fn parse(primitive: Primitive, args: &Value) -> Result<Call, Denial> {
    match primitive {
        Primitive::FsRead => {
            let path = string_arg(args, "path")?;
            let opts = options(args, "options", &["maxBytes"])?;
            let max_bytes = match opts.and_then(|o| o.get("maxBytes")) {
                None | Some(Value::Null) => fs::DEFAULT_READ_BYTES,
                Some(v) => match v.as_u64() {
                    Some(n) if (1..=fs::MAX_READ_BYTES).contains(&n) => n,
                    _ => {
                        return Err(Denial::invalid(format!(
                            "maxBytes must be a whole number from 1 to {}",
                            fs::MAX_READ_BYTES
                        )));
                    }
                },
            };
            Ok(Call::FsRead { path, max_bytes })
        }
        Primitive::FsList => Ok(Call::FsList {
            path: string_arg(args, "path")?,
        }),
        Primitive::FsStat => Ok(Call::FsStat {
            path: string_arg(args, "path")?,
        }),
        Primitive::FsWrite => {
            let path = string_arg(args, "path")?;
            let text = string_arg(args, "text")?;
            fs::check_write_size(text.len())?;
            Ok(Call::FsWrite { path, text })
        }
        Primitive::GitLog
        | Primitive::GitRevParse
        | Primitive::GitDescribeTags
        | Primitive::GitShow
        | Primitive::GitDiff => Ok(Call::Git {
            repo: string_arg(args, "repo")?,
            op: git::parse(primitive, args)?,
        }),
        Primitive::NetFetch => Ok(Call::NetFetch(net::parse_request(args)?)),
        other => Err(Denial::invalid(format!(
            "{} is not a built-in",
            other.name()
        ))),
    }
}

/// Checks a call against the scope its manifest grants, without acting:
/// for paths, resolution and the root check; for git, the repository; for
/// `net.fetch`, the URL, its host and the method. The parent runs this
/// before it journals a call, and the host runs it again before it acts.
pub fn authorize(call: &Call, grant: &Grant) -> Result<(), Denial> {
    match call {
        Call::FsRead { path, .. } | Call::FsList { path } | Call::FsStat { path } => {
            fs::resolve(path, &grant.roots, fs::Purpose::Read).map(|_| ())
        }
        Call::FsWrite { path, .. } => {
            fs::resolve(path, &grant.roots, fs::Purpose::Write).map(|_| ())
        }
        Call::Git { repo, .. } => git::repo(repo, &grant.roots).map(|_| ()),
        Call::NetFetch(request) => {
            let url = net::parse_url(&request.url)?;
            net::allowed_url(&grant.hosts, &url, &request.method)
        }
    }
}

/// Settings of the built-ins.
#[derive(Clone, Default)]
pub struct BuiltinConfig {
    pub net: NetConfig,
}

/// Carries out built-in calls. Every call arrives as the envelope the
/// parent journaled; the host checks its scope again, then acts.
pub struct BuiltinHost {
    net: net::Client,
    /// Where `fs.write` records its temporary files, once a runtime has
    /// bound one ([`Host::bind_fs_temps`]).
    temps: std::sync::RwLock<Option<Arc<dyn fs::TempLedger>>>,
}

impl Default for BuiltinHost {
    fn default() -> Self {
        Self::new(BuiltinConfig::default())
    }
}

impl BuiltinHost {
    pub fn new(config: BuiltinConfig) -> Self {
        Self {
            net: net::Client::new(config.net),
            temps: std::sync::RwLock::new(None),
        }
    }

    /// Runs one built-in call from its journaled envelope, outside any
    /// journaled call: an `fs.write` gets a temporary file name of its own
    /// and no record of it.
    pub fn run(&self, primitive: Primitive, envelope: &Value) -> Result<Value, Failure> {
        self.run_call(primitive, envelope, None)
    }

    /// Runs one built-in call from its journaled envelope. `call_key` is the
    /// journaled call's idempotency key, which names an `fs.write`
    /// temporary file and the record of it in the bound ledger.
    pub fn run_call(
        &self,
        primitive: Primitive,
        envelope: &Value,
        call_key: Option<&str>,
    ) -> Result<Value, Failure> {
        let args = envelope.get("args").unwrap_or(&Value::Null);
        let grant: Grant =
            serde_json::from_value(envelope.get("grant").cloned().unwrap_or(Value::Null))
                .map_err(|e| Denial::invalid(format!("the call's grant does not decode: {e}")))?;
        let call = parse(primitive, args)?;
        // Each implementation checks its grant at the descriptor or connection
        // it acts on; resolving it here as well doubles filesystem traversal.
        match call {
            Call::FsRead { path, max_bytes } => Ok(fs::read(&path, &grant.roots, max_bytes)?),
            Call::FsList { path } => Ok(fs::list(&path, &grant.roots)?),
            Call::FsStat { path } => Ok(fs::stat(&path, &grant.roots)?),
            Call::FsWrite { path, text } => Ok(match call_key {
                Some(key) => {
                    let ledger = self.temps.read().unwrap_or_else(|p| p.into_inner()).clone();
                    fs::write_call(&path, &grant.roots, &text, key, ledger.as_deref())?
                }
                None => fs::write(&path, &grant.roots, &text)?,
            }),
            Call::Git { repo, op } => Ok(git::run(&repo, &grant.roots, &op)?),
            Call::NetFetch(request) => self.net.fetch(&request, &grant.hosts),
        }
    }

    fn dispatch_checked(
        &self,
        request: &CallRequest,
        expected: Option<CallClass>,
    ) -> Result<Dispatched, TransportError> {
        let CallKind::Primitive(p) = request.kind else {
            return Ok(Dispatched::Completed(
                Denial::invalid("not a built-in").outcome(),
            ));
        };
        let envelope: Value = match serde_json::from_str(request.args.as_str()) {
            Ok(v) => v,
            Err(_) => {
                return Ok(Dispatched::Completed(
                    Denial::invalid("the call's request is not JSON").outcome(),
                ));
            }
        };
        // The class was journaled from the same arguments; a mismatch means
        // the rules changed between the journal and this send, and a query
        // the runtime may repeat must not turn into something it may not.
        if let Some(expected) = expected
            && request_class(&request.kind, &envelope) != Some(expected)
        {
            return Ok(Dispatched::Completed(
                Denial::new(
                    codes::CLASS_CHANGED,
                    "the call's class differs from the one journaled with it",
                )
                .outcome(),
            ));
        }
        match self.run_call(p, &envelope, Some(&request.idempotency_key)) {
            Ok(value) => Ok(Dispatched::Completed(match text(&value) {
                Ok(value) => HostOutcome::fulfilled(value),
                Err(denial) => denial.outcome(),
            })),
            Err(Failure::Refused(d)) => Ok(Dispatched::Completed(d.outcome())),
            Err(Failure::Transport(e)) => Err(e),
        }
    }
}

impl Host for BuiltinHost {
    fn classify(&self, kind: &CallKind) -> CallClass {
        // Without the arguments, the most careful answer for `net.fetch`.
        // The runtime classes built-ins from their arguments ([`class`]).
        class(kind, &Value::Null).unwrap_or(CallClass::Mutation {
            honours_idempotency_keys: false,
        })
    }

    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        self.dispatch_checked(request, None)
    }

    fn dispatch_classified(
        &self,
        request: &CallRequest,
        class: CallClass,
    ) -> Result<Dispatched, TransportError> {
        self.dispatch_checked(request, Some(class))
    }

    fn now_ms(&self) -> f64 {
        system_now()
    }

    fn random(&self) -> f64 {
        sample()
    }

    fn attach(&self, _: Arc<dyn CompletionSink>) {}

    fn bind_fs_temps(&self, ledger: Arc<dyn fs::TempLedger>) {
        *self.temps.write().unwrap_or_else(|p| p.into_inner()) = Some(ledger);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoded_result_limit_never_fulfills_null() {
        assert_eq!(
            text(&json!("\u{0001}".repeat(200_000))).unwrap_err().code,
            codes::TOO_LARGE
        );
        assert!(text(&json!({"text":"\u{0001}".repeat(MAX_TEXT_RESULT_BYTES)})).is_ok());
    }
}
