use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use basal_core::{Config, Durability, InstallGate, InstallRequest, NoHooks, Runtime, Store};
use basal_host::{
    Catalog,
    catalog_snapshot::CatalogSnapshot,
    subc_catalog::SubcCatalog,
    transport::{Transport, WireError},
};
use serde_json::{Value, json};

struct Wire {
    daemon: Mutex<Value>,
    catalogs: AtomicUsize,
    providers: AtomicUsize,
    disconnected: AtomicBool,
}

impl Wire {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            daemon: Mutex::new(json!({"generation":1,"modules":[{
            "module_id":"provider","control_ops":[],"capabilities":{"provides":["call-modes/v1","flow-scopes/v1"]},
            "flow_operations":["read","other"],"roles":[
                {"role":"management_surface","operations":[{"name":"read","kind":"query"},{"name":"other","kind":"query"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"},
                {"role":"tool_provider","tools":[{"name":"tool","execution_mode":"pure","schema":{"type":"object"}}],"identity_scope":[],"concurrency":"serial","emits_push":false,"sub_supervises":false}
            ]}]})),
            catalogs: AtomicUsize::new(0),
            providers: AtomicUsize::new(0),
            disconnected: AtomicBool::new(false),
        })
    }
    fn counts(&self) -> (usize, usize) {
        (
            self.catalogs.swap(0, Ordering::SeqCst),
            self.providers.swap(0, Ordering::SeqCst),
        )
    }
}

impl Transport for Wire {
    fn catalog(&self) -> Result<Value, WireError> {
        panic!("request catalog must stay typed")
    }
    fn catalog_snapshot(&self) -> Result<Arc<CatalogSnapshot>, WireError> {
        self.catalogs.fetch_add(1, Ordering::SeqCst);
        if self.disconnected.load(Ordering::SeqCst) {
            return Err(WireError::Unreadable("daemon disconnected".into()));
        }
        Ok(CatalogSnapshot::new(
            serde_json::from_value(self.daemon.lock().unwrap().clone()).unwrap(),
        ))
    }
    fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
        self.providers.fetch_add(1, Ordering::SeqCst);
        panic!("this fixture has no agent or workspace lookups")
    }
    fn tool(&self, _: &str, name: &str, _: Value, _: &str) -> Result<Value, WireError> {
        self.providers.fetch_add(1, Ordering::SeqCst);
        Ok(match name {
            "role.describe" => {
                json!({"majors":[{"version":"tool-provider/v1","ops":["role.describe","tool.catalog"],"stability":"alpha"}],"implementation_version":"1"})
            }
            "tool.catalog" => {
                json!({"generation":"g","catalog_digest":"d","tools":[{"name":"tool","schema_digest":"0".repeat(64),"semantics":1,"capabilities":[],"input_schema":{"type":"object"},"modes":["flow"],"emits_events":false}]})
            }
            _ => panic!("unexpected provider call"),
        })
    }
}

fn manifest(id: &str) -> Value {
    json!({"id":id,"version":1,"purpose":"Check request catalog work.","explanation":"Reads declared provider queries on a timer.","trigger":{"schedule":{"interval":"1h"}},"ops":[{"module":"provider","op":"read"},{"module":"provider","op":"other"},{"module":"provider","op":"tool"}]})
}

fn runtime(wire: &Arc<Wire>, name: &str) -> Runtime {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../target/scratch/catalog-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    Runtime::new(
        Arc::new(Store::open(root.join("store.db"), Durability::default()).unwrap()),
        Arc::new(basal_host::mock::MockHost::new()),
        Arc::new(SubcCatalog::new(wire.clone())),
        Arc::new(NoHooks),
        None,
        Config {
            install_gate: InstallGate::Off,
            ..Config::default()
        },
    )
    .unwrap()
}

fn request(m: &Value) -> InstallRequest {
    InstallRequest {
        script: "return 1;".into(),
        manifest: m.to_string(),
        author: "operator".into(),
        loop_override: false,
    }
}

#[test]
fn validation_uses_one_typed_snapshot_and_next_request_observes_revocation() {
    let wire = Wire::new();
    let rt = runtime(&wire, "validation");
    let m = manifest("snapshot-package");
    rt.register_package("return 1;", &m.to_string()).unwrap();
    assert_eq!(wire.counts(), (1, 2), "all accepted grants still validate");
    rt.register_package("return 1;", &m.to_string()).unwrap();
    assert_eq!(
        wire.counts(),
        (1, 2),
        "identical bytes still validate afresh"
    );
    let catalog = SubcCatalog::new(wire.clone());
    let frozen = catalog.request_snapshot().unwrap();
    wire.daemon.lock().unwrap()["generation"] = json!(2);
    wire.daemon.lock().unwrap()["modules"][0]["roles"][0]["operations"] = json!([]);
    assert!(frozen.lookup_op("provider", "read").unwrap().is_some());
    assert!(frozen.supports_flow_calls("provider"));
    assert!(frozen.flow_operations("provider").unwrap().is_some());
    assert_eq!(wire.counts(), (1, 0));
    assert!(
        catalog
            .request_snapshot()
            .unwrap()
            .lookup_op("provider", "read")
            .unwrap()
            .is_none()
    );
    let error = rt
        .register_package("return 1;", &m.to_string())
        .unwrap_err();
    assert!(format!("{error:?}").contains("UnknownOp"));
    assert_eq!(wire.counts(), (2, 0));
    wire.disconnected.store(true, Ordering::SeqCst);
    let failed = catalog.request_snapshot().unwrap();
    assert!(matches!(
        failed.lookup_op("provider", "read"),
        Err(WireError::Unreadable(_))
    ));
    assert!(matches!(
        failed.flow_operations("provider"),
        Err(WireError::Unreadable(_))
    ));
    assert!(!failed.supports_flow_calls("provider"));
    assert_eq!(
        wire.counts(),
        (1, 0),
        "a failed snapshot is not silently refreshed"
    );
    wire.disconnected.store(false, Ordering::SeqCst);
    wire.daemon.lock().unwrap()["modules"] = json!([]);
    assert!(matches!(
        catalog
            .request_snapshot()
            .unwrap()
            .lookup_op("provider", "read"),
        Err(WireError::Typed(_))
    ));
    assert_eq!(
        wire.counts(),
        (1, 0),
        "a missing provider is not an absent operation"
    );
}

#[test]
fn cheap_package_refusals_precede_catalog_and_provider_validation() {
    let wire = Wire::new();
    let rt = runtime(&wire, "package");
    let mut m = manifest("cheap-package");
    rt.register_package("return 1;", &m.to_string()).unwrap();
    wire.counts();
    assert!(
        format!(
            "{:?}",
            rt.register_package("return 2;", &m.to_string())
                .unwrap_err()
        )
        .contains("package_version_conflict")
    );
    assert_eq!(wire.counts(), (0, 0));
    m["version"] = json!(2);
    m["ops"][0]["op"] = json!("missing");
    assert!(
        format!(
            "{:?}",
            rt.register_package("return 1;", &m.to_string())
                .unwrap_err()
        )
        .contains("ChangesRequired")
    );
    assert_eq!(wire.counts(), (0, 0));
    m["changes"] = json!("Adds a missing operation.");
    assert!(
        format!(
            "{:?}",
            rt.register_package("return 1;", &m.to_string())
                .unwrap_err()
        )
        .contains("UnknownOp")
    );
    assert_eq!(wire.counts(), (1, 0));
}

#[test]
fn cheap_install_refusals_precede_catalog_and_provider_validation() {
    let wire = Wire::new();
    let rt = runtime(&wire, "install");
    let mut m = manifest("cheap-install");
    rt.install(&request(&m)).unwrap();
    assert_eq!(wire.counts(), (1, 2));
    let mut changed = request(&m);
    changed.script = "return 2;".into();
    assert!(format!("{:?}", rt.install(&changed).unwrap_err()).contains("VersionExists"));
    assert_eq!(wire.counts(), (0, 0));
    m["version"] = json!(2);
    m["ops"][0]["op"] = json!("missing");
    assert!(format!("{:?}", rt.install(&request(&m)).unwrap_err()).contains("ChangesRequired"));
    assert_eq!(wire.counts(), (0, 0));
    m["changes"] = json!("Adds a missing operation.");
    assert!(format!("{:?}", rt.install(&request(&m)).unwrap_err()).contains("UnknownOp"));
    assert_eq!(wire.counts(), (1, 0));
}
