//! Exposes codemode operations without running JavaScript on the request
//! handler's thread. Admission writes the run, then basal-core's Supervisor
//! drives its fresh worker and tool calls on a separate blocking thread.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use basal_core::channel::{ChannelError, WorkerChannel, WorkerSource};
use basal_core::codemode::admission::{self, Admission, Platform};
use basal_core::codemode::supervisor::Supervisor;
use basal_core::{Clock, CoreError, Store};
use basal_host::Host;
use cortexkit_role_tool_provider::call::ToolCallRequest;
use cortexkit_role_tool_provider::{call, late_results, withdraw};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use subc_protocol::ErrorBody;

use crate::pool::{Binding, Pool};

fn storage(error: CoreError) -> ErrorBody {
    ErrorBody::new("storage_unavailable", error.to_string())
}

struct FreshWorkers(Pool);

impl WorkerSource for FreshWorkers {
    fn worker(&self) -> Result<Box<dyn WorkerChannel>, ChannelError> {
        self.0
            .acquire(Binding::Codemode)
            .map(|lease| Box::new(lease) as Box<dyn WorkerChannel>)
            .map_err(|error| ChannelError::Broken(error.to_string()))
    }
}

#[derive(Clone)]
pub(crate) struct Codemode {
    supervisor: Supervisor,
    store: Arc<Store>,
    host: Arc<dyn Host>,
    clock: Clock,
    transport: Arc<dyn basal_host::transport::Transport>,
    admission: Arc<Mutex<()>>,
    records: Arc<Mutex<()>>,
    late_lock: Arc<Mutex<()>>,
    incarnation: String,
}

impl Codemode {
    pub(crate) fn new(
        store: Arc<Store>,
        hosts: &crate::module::Hosts,
        pool: Pool,
        config: &basal_core::Config,
    ) -> Result<Self, CoreError> {
        let supervisor = Supervisor::new(
            store.clone(),
            hosts.transport.clone(),
            hosts.catalog.clone(),
            Arc::new(FreshWorkers(pool)),
            config.clock.clone(),
            basal_proto::PreludeHash::of(include_str!(
                "../../basal-worker/src/codemode_prelude.js"
            )),
            config.shell_denylist.clone(),
        )?
        .with_models(hosts.host.clone(), config.selector.clone());
        let late_lock = Arc::new(Mutex::new(()));
        let weak_store = Arc::downgrade(&store);
        let serial = late_lock.clone();
        let clock = config.clock.clone();
        supervisor.on_terminal(Arc::new(move |id, result| {
            if let Some(store) = weak_store.upgrade() {
                let _serial = serial.lock().unwrap();
                publish_result(&store, id, result).map_err(|e| CoreError::Invalid(e.message))?;
                maintain_late(&store, &clock).map_err(|e| CoreError::Invalid(e.message))?;
            }
            Ok(())
        }));
        let codemode = Self {
            supervisor,
            store,
            host: hosts.host.clone(),
            clock: config.clock.clone(),
            transport: hosts.transport.clone(),
            admission: Arc::new(Mutex::new(())),
            records: Arc::new(Mutex::new(())),
            late_lock,
            incarnation: random_id().map_err(|e| CoreError::Invalid(e.message))?,
        };
        codemode
            .publish()
            .map_err(|e| CoreError::Invalid(e.message))?;
        Ok(codemode)
    }

    pub(crate) fn begin(
        &self,
        context: &crate::tool::Context,
        request: &ToolCallRequest,
    ) -> Result<String, ErrorBody> {
        self.begin_guarded(context, request, || false)
    }

    pub(crate) fn begin_guarded(
        &self,
        context: &crate::tool::Context,
        request: &ToolCallRequest,
        cancelled: impl Fn() -> bool,
    ) -> Result<String, ErrorBody> {
        let pin = call::check_call(request).map_err(crate::tool::provider_error)?;
        if let Some(pin) = pin {
            let current = crate::tool::catalog_tool();
            if pin.schema_digest != current.schema_digest {
                return Err(ErrorBody::new("tool_schema_changed","codemode input schema changed")
                    .with_detail(json!({"tool":"codemode","expected":pin.schema_digest,"current":current.schema_digest})));
            }
            if pin.semantics != current.semantics {
                return Err(ErrorBody::new("tool_semantics_changed","codemode semantics changed")
                    .with_detail(json!({"tool":"codemode","expected":pin.semantics,"current":current.semantics})));
            }
        }
        let carrier = crate::tool::principal(&context.principal)?;
        let Some(scope) = &context.scope else {
            return Err(ErrorBody::new("no_scope", "codemode needs an agent scope"));
        };
        let crate::caller::Caller::Agent { agent_id, .. } =
            crate::caller::from_route(Some(&context.principal), Some(scope))
        else {
            return Err(ErrorBody::new(
                "scope_mismatch",
                "the stamp does not identify a core-owned agent",
            ));
        };
        if scope.attributes.run_id.is_some() {
            return Err(ErrorBody::new(
                "scope_mismatch",
                "codemode cannot recurse from a run scope",
            ));
        }
        let id = match &request.call_key {
            Some(key) => run_id(&agent_id, key),
            None => random_id()?,
        };
        let _admission = self.admission.lock().unwrap();
        if cancelled() {
            self.supervisor.cancel(&id).map_err(storage)?;
            return Err(ErrorBody::new(
                "cancelled",
                "codemode cancelled before admission",
            ));
        }
        if let Some(key) = &request.call_key {
            let recorded: Option<String> = self
                .store
                .read(|conn| {
                    Ok(conn.query_row(
                "SELECT run_id FROM codemode_tool_calls WHERE carrier=?1 AND call_key=?2",
                rusqlite::params![carrier,key],|row| row.get(0)).optional()?)
                })
                .map_err(storage)?;
            if recorded.is_some_and(|recorded| recorded != id) {
                return Err(crate::tool::invalid(
                    "call_key",
                    "this carrier already used the key for another agent",
                ));
            }
        }
        let record_scope = serde_json::to_string(&context.identity()).unwrap();
        let custodian = if context.principal == subc_protocol::Principal::Direct {
            crate::tool::principal(&scope.owner)?
        } else {
            carrier.clone()
        };
        let invocation = admission::ToolInvocation {
            run_id: &id,
            agent_id: &agent_id,
            invoking_scope: basal_host::run_scope::Scope {
                reference: scope.scope_ref.clone(),
                epoch: scope.scope_epoch,
            },
            carrier: &carrier,
            call_key: request.call_key.as_deref(),
            record_scope: &record_scope,
            custodian: &custodian,
            bind_identity: &context.bind_identity,
        };
        match admission::admit_tool(
            &self.store,
            self.host.as_ref(),
            self.transport.as_ref(),
            &self.clock,
            Platform::current(),
            &invocation,
            &request.arguments,
        )
        .map_err(storage)?
        {
            Admission::Refused(refusal) => Err(ErrorBody::new(refusal.code, refusal.message)),
            Admission::Existing(_) => {
                if let Some(key) = &request.call_key {
                    self.store.write(|tx| {
                        tx.execute("INSERT OR IGNORE INTO codemode_tool_calls(carrier,call_key,run_id,scope,custodian) VALUES (?1,?2,?3,?4,?5)",
                            rusqlite::params![carrier,key,id,record_scope,custodian])?;
                        Ok(())
                    }).map_err(storage)?;
                }
                Ok(id)
            }
            Admission::Admitted { run, start } => {
                if cancelled() {
                    self.supervisor.cancel(&id).map_err(storage)?;
                } else if let Some(start) = start {
                    self.supervisor.start(*run, *start).map_err(storage)?;
                }
                Ok(id)
            }
        }
    }

    pub(crate) fn wait(&self, id: &str, timeout: Duration) -> Result<Option<Value>, ErrorBody> {
        let result = self
            .supervisor
            .wait(id, timeout)
            .map_err(storage)?
            .map(|result| render(id, result));
        if result.is_some() {
            self.publish()?;
        }
        Ok(result)
    }

    pub(crate) fn cancel(&self, id: &str) -> Result<(), ErrorBody> {
        self.supervisor.cancel(id).map_err(storage)?;
        self.publish()
    }

    pub(crate) fn role(
        &self,
        context: &crate::tool::Context,
        request: &ToolCallRequest,
    ) -> Result<Value, ErrorBody> {
        if request.name != "tool.withdraw" {
            call::check_call(request).map_err(crate::tool::provider_error)?;
        }
        match request.name.as_str() {
            "role.describe" => Ok(crate::tool::describe()),
            "tool.catalog" => crate::tool::catalog(request.arguments.clone()),
            "tool.withdraw" => self.withdraw(context, request),
            "late_results" => self.late(context, request.arguments.clone()),
            "late_results.ack" => self.ack(context, request.arguments.clone()),
            _ => Err(ErrorBody::new("unknown_tool", "basal serves only codemode")
                .with_detail(json!({"tool":request.name}))),
        }
    }

    fn withdraw(
        &self,
        context: &crate::tool::Context,
        request: &ToolCallRequest,
    ) -> Result<Value, ErrorBody> {
        let arguments =
            withdraw::parse_withdraw_request(request).map_err(crate::tool::provider_error)?;
        let caller = crate::tool::principal(&context.principal)?;
        let scope = context.identity();
        let resolution = withdraw::resolve_carrier(&caller, scope.as_ref(), &arguments)
            .map_err(|e| crate::tool::provider_error(e.error()))?;
        let carrier = match &resolution {
            withdraw::CarrierResolution::Carrier(carrier)
            | withdraw::CarrierResolution::OwnerAsCarrier(carrier) => carrier,
        };
        let _records = self.records.lock().unwrap();
        let record = self.store.read(|conn| Ok(conn.query_row(
            "SELECT run_id,scope,withdrawal FROM codemode_tool_calls WHERE carrier=?1 AND call_key=?2",
            rusqlite::params![carrier,arguments.call_key],|row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?))
        ).optional()?)).map_err(storage)?;
        let Some((id, record_scope, answer)) = record else {
            return if matches!(resolution, withdraw::CarrierResolution::OwnerAsCarrier(_)) {
                Err(crate::tool::provider_error(
                    withdraw::CallerProblem::CarrierRequired.error(),
                ))
            } else {
                Ok(json!({"answer":"unknown_call"}))
            };
        };
        let record_scope: Option<cortexkit_role_tool_provider::scope::ScopeIdentity> =
            serde_json::from_str(&record_scope)
                .map_err(|e| crate::tool::invalid("scope", e.to_string()))?;
        withdraw::check_record_scope(record_scope.as_ref(), scope.as_ref())
            .map_err(|e| crate::tool::provider_error(e.error()))?;
        if let Some(answer) = answer {
            return serde_json::from_str(&answer)
                .map_err(|e| crate::tool::invalid("record", e.to_string()));
        }
        let result = self.supervisor.result(&id).map_err(storage)?;
        let answer = if let Some(result) = result {
            if result["status"] == "running" {
                let (settled, started) =
                    self.supervisor.cancel_with_started(&id).map_err(storage)?;
                if settled.is_none() {
                    json!({"answer":"completed","outcome":"unknown","result_retained":false})
                } else if let Some(settled) = settled
                    && settled["status"] != "cancelled"
                    && settled["status"] != "running"
                {
                    json!({"answer":"completed","result":render(&id, settled),"result_retained":true})
                } else if started {
                    json!({"answer":"already_started","outcome":"unknown"})
                } else {
                    json!({"answer":"withdrawn"})
                }
            } else {
                json!({"answer":"completed","result":render(&id, result),"result_retained":true})
            }
        } else {
            json!({"answer":"completed","outcome":"unknown","result_retained":false})
        };
        self.store.write(|tx| {
            tx.execute("UPDATE codemode_tool_calls SET withdrawal=?3 WHERE carrier=?1 AND call_key=?2 AND withdrawal IS NULL",rusqlite::params![carrier,arguments.call_key,answer.to_string()])?;
            Ok(())
        }).map_err(storage)?;
        Ok(answer)
    }

    fn publish(&self) -> Result<(), ErrorBody> {
        let _records = self.late_lock.lock().unwrap();
        let ids = self
            .store
            .read(|conn| {
                let mut statement = conn
                    .prepare("SELECT DISTINCT run_id FROM codemode_tool_calls WHERE published=0")?;
                Ok(statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .map_err(storage)?;
        for id in ids {
            if let Some(result) = self.supervisor.result(&id).map_err(storage)?
                && result["status"] != "running"
            {
                publish_result(&self.store, &id, result)?;
            }
        }
        maintain_late(&self.store, &self.clock)
    }

    fn head(&self, custodian: &str) -> Result<u64, ErrorBody> {
        self.store.read(|conn| Ok(conn.query_row(
            "SELECT MAX(COALESCE((SELECT MAX(seq) FROM codemode_late_results WHERE custodian=?1),0),COALESCE((SELECT through_seq FROM codemode_late_acks WHERE custodian=?1),0))",
            [custodian],|row| row.get(0))?)).map_err(storage)
    }

    fn late(&self, context: &crate::tool::Context, arguments: Value) -> Result<Value, ErrorBody> {
        let custodian = crate::tool::principal(&context.principal)?;
        let request: late_results::LateResultsRequest = serde_json::from_value(arguments)
            .map_err(|e| crate::tool::invalid("arguments", e.to_string()))?;
        self.publish()?;
        let head = self.head(&custodian)?;
        late_results::check_since(&self.incarnation, head, request.since.as_ref())
            .map_err(crate::tool::provider_error)?;
        let limit = request.limit.unwrap_or(100).clamp(1, 1000) as usize;
        let since = request.since.map_or(0, |cursor| cursor.seq);
        let rows = self.store.read(|conn| {
            let mut statement = conn.prepare("SELECT seq,entry FROM codemode_late_results WHERE custodian=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?;
            Ok(statement.query_map(rusqlite::params![custodian,since,limit + 1],|row| Ok((row.get::<_,u64>(0)?,row.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
        }).map_err(storage)?;
        let more = rows.len() > limit;
        let mut cursor = since;
        let entries = rows
            .into_iter()
            .take(limit)
            .map(|(seq, entry)| {
                cursor = seq;
                serde_json::from_str::<Value>(&entry).expect("stored late entry")
            })
            .collect::<Vec<_>>();
        Ok(
            json!({"entries":entries,"cursor":{"provider_incarnation":self.incarnation,"seq":cursor},"more":more}),
        )
    }

    fn ack(&self, context: &crate::tool::Context, arguments: Value) -> Result<Value, ErrorBody> {
        let _records = self.late_lock.lock().unwrap();
        let custodian = crate::tool::principal(&context.principal)?;
        let request: late_results::AckRequest = serde_json::from_value(arguments)
            .map_err(|e| crate::tool::invalid("arguments", e.to_string()))?;
        late_results::check_since(
            &self.incarnation,
            self.head(&custodian)?,
            Some(&request.through),
        )
        .map_err(crate::tool::provider_error)?;
        self.store.write(|tx| {
            tx.execute("INSERT INTO codemode_late_acks(custodian,through_seq) VALUES (?1,?2) ON CONFLICT(custodian) DO UPDATE SET through_seq=MAX(through_seq,excluded.through_seq)",rusqlite::params![custodian,request.through.seq])?;
            tx.execute("DELETE FROM codemode_late_results WHERE custodian=?1 AND seq<=?2",rusqlite::params![custodian,request.through.seq])?;
            Ok(())
        }).map_err(storage)?;
        Ok(json!({}))
    }

    pub(crate) fn stop(&self) -> Result<(), CoreError> {
        self.supervisor.shutdown()
    }
}

fn random_id() -> Result<String, ErrorBody> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| ErrorBody::new("entropy_unavailable", e.to_string()))?;
    Ok(blake3::Hash::from_bytes(bytes).to_hex().to_string())
}

/// Length framing keeps pairs such as (ab,c) and (a,bc) distinct, following
/// basal's existing idempotency-key convention, in a separate hash domain.
pub fn run_id(agent: &str, key: &str) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"basal/codemode/run/v1\0");
    for field in [agent, key] {
        hash.update(&(field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize().to_hex().to_string()
}

fn render_text(result: &mut Value) -> String {
    let status = result["status"]
        .as_str()
        .unwrap_or("failed")
        .replace("budget_exhausted:", "budget_exhausted: ");
    let calls = result["calls"].as_array().cloned().unwrap_or_default();
    let mut warnings = result["warnings"].as_array().cloned().unwrap_or_default();
    if result["keyless"] == true {
        warnings.push(json!({"code":"keyless","message":"This call had no key and can't be recovered if its reply is lost."}));
    }
    if calls
        .iter()
        .any(|call| call["outcome"] == "outcome_unknown")
    {
        warnings.push(json!({"code":"calls_unsettled","message":"Some calls have unknown outcomes and may still be running; they were never retried."}));
    }
    let mut text = format!(
        "{status} · {} ms · {} calls · person wait {} ms\n",
        result["duration_ms"],
        result
            .get("call_count")
            .and_then(Value::as_u64)
            .unwrap_or(calls.len() as u64),
        result["person_wait_ms"]
    );
    if let Some(error) = result.get("error") {
        text.push_str(&format!("Error: {error}\n"));
    }
    if let Some(value) = result.get("value") {
        text.push_str(&format!("\nReturn value:\n{value}\n"));
    }
    text.push_str(&format!(
        "\nOutput:\n{}\n\nTool | Outcome | Waited on person | Duration (ms)\n",
        result["output"].as_str().unwrap_or("")
    ));
    for call in &calls {
        text.push_str(&format!(
            "{} | {} | {} | {}\n",
            call["tool"].as_str().unwrap_or(""),
            call["outcome"].as_str().unwrap_or(""),
            call["waited_on_person"],
            call.get("duration_ms")
                .map_or_else(|| "—".into(), Value::to_string)
        ));
    }
    for warning in &warnings {
        text.push_str(&format!(
            "Warning: {}\n",
            warning
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| warning.to_string(), str::to_owned)
        ));
    }
    result["warnings"] = warnings.into();
    text
}

fn render(id: &str, mut result: Value) -> Value {
    let text = render_text(&mut result);
    result["run_id"] = id.into();
    result["termination_reason"] = result["status"].clone();
    let code = result["error"]["code"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            let status = result["status"].as_str()?;
            (status != "completed" && status != "running").then(|| status.to_owned())
        });
    let mut reply = cortexkit_role_tool_provider::result::ToolResult::new(vec![
        cortexkit_role_tool_provider::result::ContentPart::text(text),
    ]);
    if let Some(code) = code {
        let code = if let Some(budget) = code.strip_prefix("budget_exhausted:") {
            result["budget"] = budget.into();
            "budget_exhausted".to_owned()
        } else {
            code
        };
        reply = reply.with_error(code);
    }
    reply.structured_content = result.as_object().cloned();
    bounded_result(reply)
}

pub(crate) fn running(id: &str) -> Value {
    bounded_result(
        cortexkit_role_tool_provider::result::ToolResult::new(vec![
            cortexkit_role_tool_provider::result::ContentPart::text(
                "running: the final result will be retained in late_results.",
            ),
        ])
        .with_structured_content(
            json!({"status":"running","run_id":id})
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
}

fn serialized_size(value: &impl serde::Serialize) -> usize {
    serde_json::to_vec(value).expect("JSON result").len()
}

const TEXT_TRUNCATED: &str = "\n[Model text truncated for the 3 MiB result limit.]\n";
const STRUCTURED_OMITTED: &str = "\nStructured fields were omitted for size.\n";

fn bound_text(text: &mut String) {
    use cortexkit_role_tool_provider::result::MAX_RESULT_BYTES;
    // Leave room for identity, outcome, budget counters and omission markers.
    // Count escaped JSON bytes: control characters can expand sixfold.
    let limit = MAX_RESULT_BYTES - 4096;
    if serialized_size(text) < limit {
        return;
    }
    let mut low = 0;
    let mut high = text.len();
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let mut end = middle;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = format!("{}{TEXT_TRUNCATED}", &text[..end]);
        if serialized_size(&candidate) < limit {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    while !text.is_char_boundary(low) {
        low -= 1;
    }
    text.truncate(low);
    text.push_str(TEXT_TRUNCATED);
}

fn bounded_result(mut reply: cortexkit_role_tool_provider::result::ToolResult) -> Value {
    use cortexkit_role_tool_provider::result::{ContentPart, MAX_RESULT_BYTES, validate};
    let ContentPart::Text { text } = &mut reply.content[0] else {
        unreachable!("codemode emits one text part");
    };
    bound_text(text);
    let mut omitted = std::collections::BTreeSet::new();
    while serialized_size(&reply) >= MAX_RESULT_BYTES {
        let fields = reply.structured_content.as_mut().expect("codemode fields");
        // Keep outcome, identity, error code and budget accounting intact. A
        // successful run may have made writes; size must never turn it into an
        // error that encourages the caller to retry those writes.
        let largest = fields
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "run_id"
                        | "status"
                        | "termination_reason"
                        | "budget"
                        | "duration_ms"
                        | "person_wait_ms"
                        | "call_count"
                        | "keyless"
                        | "error"
                ) && !omitted.contains(key.as_str())
            })
            .map(|(key, value)| (key.clone(), serialized_size(value)))
            .chain(
                fields
                    .get("error")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .filter(|(key, _)| key.as_str() != "code")
                    .map(|(key, value)| (format!("error.{key}"), serialized_size(value)))
                    .filter(|(key, _)| !omitted.contains(key)),
            )
            .max_by_key(|(_, bytes)| *bytes)
            .expect("oversized results have a reducible field");
        let (key, bytes) = largest;
        let marker = json!({"omitted":true,"original_bytes":bytes});
        if let Some(member) = key.strip_prefix("error.") {
            fields.get_mut("error").unwrap()[member] = marker;
        } else {
            fields.insert(key.clone(), marker);
        }
        if omitted.is_empty() {
            let ContentPart::Text { text } = &mut reply.content[0] else {
                unreachable!()
            };
            text.push_str(STRUCTURED_OMITTED);
        }
        omitted.insert(key);
    }
    validate(&reply).expect("bounded codemode content/v1 result");
    serde_json::to_value(reply).expect("JSON result")
}

fn publish_result(store: &Store, id: &str, result: Value) -> Result<(), ErrorBody> {
    let records = store.read(|conn| {
        let mut statement = conn.prepare("SELECT carrier,call_key,scope,custodian FROM codemode_tool_calls WHERE run_id=?1 AND published=0")?;
        Ok(statement.query_map([id],|row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }).map_err(storage)?;
    for (carrier, key, scope, custodian) in records {
        let scope: cortexkit_role_tool_provider::scope::ScopeIdentity =
            serde_json::from_str::<Option<_>>(&scope)
                .map_err(|e| crate::tool::invalid("record", e.to_string()))?
                .ok_or_else(|| crate::tool::invalid("record", "missing scope"))?;
        let settled_at: i64 = store
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT ended_at FROM codemode_runs WHERE run_id=?1",
                    [id],
                    |row| row.get(0),
                )?)
            })
            .map_err(storage)?;
        let event_id = run_id(&carrier, &format!("{key}:{id}:terminal"));
        let entry = late_results::LateEntry::new(
            late_results::kinds::RESULT,
            scope.owner,
            scope.scope_ref,
            scope.scope_epoch,
            custodian.clone(),
            key.clone(),
            event_id.clone(),
            settled_at.max(0) as u64,
        )
        .with_result(render(id, result.clone()));
        store.write(|tx| {
            tx.execute("INSERT OR IGNORE INTO codemode_late_results(custodian,entry,event_id,settled_at) VALUES (?1,?2,?3,?4)",
                rusqlite::params![custodian,serde_json::to_string(&entry).unwrap(),event_id,settled_at])?;
            tx.execute("UPDATE codemode_tool_calls SET published=1 WHERE carrier=?1 AND call_key=?2",rusqlite::params![carrier,key])?;
            Ok(())
        }).map_err(storage)?;
    }
    Ok(())
}

fn maintain_late(store: &Store, clock: &Clock) -> Result<(), ErrorBody> {
    store.write(|tx| {
        tx.execute("UPDATE codemode_late_results SET entry=json_set(json_remove(entry,'$.result'),'$.kind','expired','$.reduced',json('true'),'$.outcome','expired'), expired=1 WHERE expired=0 AND settled_at<=?1",[clock.now_ms().saturating_sub(24 * 60 * 60 * 1000)])?;
        tx.execute("DELETE FROM codemode_late_results WHERE seq IN (SELECT seq FROM (SELECT seq,ROW_NUMBER() OVER (PARTITION BY custodian,json_extract(entry,'$.owner'),json_extract(entry,'$.ref'),json_extract(entry,'$.scope_epoch') ORDER BY seq DESC) AS n FROM codemode_late_results WHERE expired=0) WHERE n>1000)",[])?;
        tx.execute("DELETE FROM codemode_late_results WHERE seq IN (SELECT seq FROM (SELECT seq,ROW_NUMBER() OVER (PARTITION BY json_extract(entry,'$.owner') ORDER BY seq DESC) AS n FROM codemode_late_results WHERE expired=1) WHERE n>1000)",[])?;
        Ok(())
    }).map_err(storage)
}

#[cfg(test)]
#[path = "codemode_result_tests.rs"]
mod result_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_retention_bounds_each_session_then_expired_owner() {
        let root = std::env::temp_dir().join(format!("basal-late-caps-{}", random_id().unwrap()));
        let store =
            Store::open(root.join("basal.db"), basal_core::Durability::throwaway()).unwrap();
        let clock = Clock::manual(100);
        store.write(|tx| {
            for session in ["one","two"] {
                for n in 0..1002 {
                    let event = format!("{session}:{n}");
                    let entry = late_results::LateEntry::new("result","reserved:prefrontal-core",session,7,"reserved:broca",event.clone(),event.clone(),100).with_result(json!(42));
                    tx.execute("INSERT INTO codemode_late_results(custodian,entry,event_id,settled_at) VALUES ('reserved:broca',?1,?2,100)",rusqlite::params![serde_json::to_string(&entry).unwrap(),event])?;
                }
            }
            Ok(())
        }).unwrap();
        maintain_late(&store, &clock).unwrap();
        store
            .read(|conn| {
                let count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM codemode_late_results", [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 2000);
                let oldest: String = conn.query_row(
                    "SELECT event_id FROM codemode_late_results ORDER BY seq LIMIT 1",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(oldest, "one:2");
                Ok(())
            })
            .unwrap();
        clock.advance(24 * 60 * 60 * 1000);
        maintain_late(&store, &clock).unwrap();
        store
            .read(|conn| {
                let (count, expired): (i64, i64) = conn.query_row(
                    "SELECT COUNT(*),SUM(expired) FROM codemode_late_results",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!((count, expired), (1000, 1000));
                let entry: String = conn.query_row(
                    "SELECT entry FROM codemode_late_results ORDER BY seq LIMIT 1",
                    [],
                    |row| row.get(0),
                )?;
                let entry: Value = serde_json::from_str(&entry).unwrap();
                assert_eq!(entry["event_id"], "two:2");
                assert_eq!(entry["kind"], "expired");
                assert!(entry.get("result").is_none());
                Ok(())
            })
            .unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
