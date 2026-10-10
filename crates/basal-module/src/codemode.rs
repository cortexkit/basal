//! Exposes codemode operations without running JavaScript on the request
//! handler's thread. Admission writes the run, then basal-core's Supervisor
//! drives its fresh worker and tool calls on a separate blocking thread.

use std::sync::Arc;

use basal_core::channel::{ChannelError, WorkerChannel, WorkerSource};
use basal_core::codemode::admission::{self, Admission, Platform};
use basal_core::codemode::supervisor::Supervisor;
use basal_core::{Clock, CoreError, Store};
use basal_host::Host;
use serde_json::Value;

use crate::pool::{Binding, Pool};

pub(crate) enum Error {
    Core(CoreError),
    Refused { code: &'static str, message: String },
}

impl From<CoreError> for Error {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
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
        )?;
        Ok(Self {
            supervisor,
            store,
            host: hosts.host.clone(),
            clock: config.clock.clone(),
        })
    }

    pub(crate) fn handle(&self, method: &str, params: Value) -> Result<Value, Error> {
        let id = if method == "codemode.run" {
            let admitted = admission::admit(
                &self.store,
                self.host.as_ref(),
                &self.clock,
                Platform::current(),
                &params,
            )?;
            match admitted {
                Admission::Refused(refusal) => {
                    return Err(Error::Refused {
                        code: refusal.code,
                        message: refusal.message,
                    });
                }
                Admission::Existing(run) => run.run_id,
                Admission::Admitted { run, start } => {
                    // Return the committed admission state even when a short
                    // program can finish before the handler sends its reply.
                    let response = self.result(&run.run_id)?;
                    if let Some(start) = start {
                        self.supervisor.start(*run, *start)?;
                    }
                    return Ok(response);
                }
            }
        } else {
            params
                .get("run_id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Refused {
                    code: "invalid_request",
                    message: "run_id must be a string".into(),
                })?
                .to_owned()
        };
        if method == "codemode.cancel" {
            self.known(self.supervisor.cancel(&id)?, &id)
        } else {
            self.result(&id)
        }
    }

    fn result(&self, id: &str) -> Result<Value, Error> {
        self.known(self.supervisor.result(id)?, id)
    }

    fn known(&self, result: Option<Value>, id: &str) -> Result<Value, Error> {
        result.ok_or_else(|| Error::Refused {
            code: "unknown_run",
            message: format!("no codemode run {id}"),
        })
    }

    pub(crate) fn stop(&self) -> Result<(), CoreError> {
        for id in self.store.read(basal_core::codemode::store::running_runs)? {
            self.supervisor.cancel(&id)?;
        }
        Ok(())
    }
}
