//! Connect the blocking admission host to the SDK's live module control
//! connection. HELLO_ACK starts store preparation before the SDK returns its
//! handle, so host construction waits off the async runtime until attachment.

use std::sync::{Arc, Condvar, Mutex, Weak};

use basal_host::routing::ModuleOpsHost;
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::Transport;
use subc_client_rs::ModuleHandle;

#[derive(Default)]
struct State {
    connection: Option<(ModuleHandle, tokio::runtime::Handle)>,
    hosts: Vec<Weak<ModuleOpsHost>>,
}

#[derive(Clone, Default)]
pub struct ScopeConnection(Arc<(Mutex<State>, Condvar)>);

impl ScopeConnection {
    /// Attach each newly registered SDK connection before polling its serve
    /// future. Existing hosts must not retain a disconnected describe handle.
    pub fn attach(&self, handle: ModuleHandle, runtime: tokio::runtime::Handle) {
        let mut state = self.0.0.lock().unwrap();
        state.hosts.retain(|host| {
            if let Some(host) = host.upgrade() {
                host.set_scope_describer(handle.clone(), runtime.clone());
                true
            } else {
                false
            }
        });
        state.connection = Some((handle, runtime));
        self.0.1.notify_all();
    }

    /// Build hosts on the module's startup thread, never on a Tokio worker.
    pub fn module_ops(
        &self,
        transport: Arc<dyn Transport>,
        catalog: Arc<SubcCatalog>,
    ) -> Arc<ModuleOpsHost> {
        let mut state = self
            .0
            .1
            .wait_while(self.0.0.lock().unwrap(), |s| s.connection.is_none())
            .unwrap();
        let (handle, runtime) = state.connection.as_ref().unwrap();
        let host = Arc::new(
            ModuleOpsHost::new(transport, catalog)
                .with_scope_describer(handle.clone(), runtime.clone()),
        );
        state.hosts.push(Arc::downgrade(&host));
        host
    }
}
