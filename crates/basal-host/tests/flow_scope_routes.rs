use basal_host::Host;
use basal_host::flow_scope::{FlowScope, RegisteredScope, ScopedRoutes, flow_route_key};
use basal_host::routing::ModuleOpsHost;
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{Transport, WireError};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use subc_protocol::{Principal, RouteTarget};

#[test]
fn route_cache_reuses_handles_and_replaces_changed_selectors() {
    let registered = |selector: FlowScope| RegisteredScope {
        selector,
        targets: ["broca".into()].into_iter().collect(),
    };
    let mut routes = ScopedRoutes::<u64>::default();
    let mut scope = FlowScope {
        owner: Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        scope_ref: "flow-a".into(),
        epoch: 1,
    };
    let target = RouteTarget::ManagementSurface {
        module_id: "broca".into(),
    };
    routes.configure("flow-a", Some(registered(scope.clone())));
    assert_eq!(
        routes
            .route("flow-a", &target, |actual| {
                assert_eq!(actual, &scope);
                Ok(7)
            })
            .unwrap(),
        Some(7)
    );
    assert_eq!(
        routes
            .route("flow-a", &target, |_| {
                panic!("a cached route must not be opened again")
            })
            .unwrap(),
        Some(7)
    );
    scope.epoch = 2;
    assert_eq!(
        routes.configure("flow-a", Some(registered(scope.clone()))),
        vec![7]
    );
    assert_eq!(
        routes
            .route("flow-a", &target, |actual| {
                assert_eq!(actual, &scope);
                Ok(13)
            })
            .unwrap(),
        Some(13)
    );
    scope.scope_ref = "flow-replacement".into();
    assert_eq!(
        routes.configure("flow-a", Some(registered(scope.clone()))),
        vec![13]
    );
    assert_eq!(
        routes
            .route("flow-a", &target, |actual| {
                assert_eq!(actual, &scope);
                Ok(17)
            })
            .unwrap(),
        Some(17)
    );
}

#[derive(Default)]
struct RouteTransport {
    routes: Mutex<ScopedRoutes<u64>>,
    configurations: Mutex<Vec<(String, Option<RegisteredScope>)>>,
}
impl Transport for RouteTransport {
    fn configure_flow(&self, key: &str, scope: Option<RegisteredScope>) {
        self.configurations
            .lock()
            .unwrap()
            .push((key.into(), scope.clone()));
        self.routes.lock().unwrap().configure(key, scope);
    }
    fn catalog(&self) -> Result<Value, WireError> {
        panic!("configure must not query providers")
    }
    fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
        panic!("configure must not dispatch")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("configure must not dispatch")
    }
}
fn codemode_scope() -> RegisteredScope {
    RegisteredScope {
        selector: FlowScope {
            owner: Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
            scope_ref: "scope:codemode".into(),
            epoch: 7,
        },
        targets: ["reader".into()].into_iter().collect(),
    }
}
fn route_host() -> (ModuleOpsHost, Arc<RouteTransport>) {
    let transport = Arc::new(RouteTransport::default());
    (
        ModuleOpsHost::new(
            transport.clone(),
            Arc::new(SubcCatalog::new(transport.clone())),
        ),
        transport,
    )
}
fn target() -> RouteTarget {
    RouteTarget::ToolProvider {
        module_id: "reader".into(),
    }
}

#[test]
fn configure_flow_registers_a_codemode_route_key() {
    let (host, wire) = route_host();
    host.configure_flow("codemode:run-1", Some(codemode_scope()));
    assert_eq!(
        *wire.configurations.lock().unwrap(),
        vec![("codemode:run-1".into(), Some(codemode_scope()))]
    );
    assert_eq!(
        wire.routes
            .lock()
            .unwrap()
            .route("codemode:run-1", &target(), |actual| {
                assert_eq!(actual, &codemode_scope().selector);
                Ok(11)
            })
            .unwrap(),
        Some(11)
    );
}

#[test]
fn configure_flow_releases_a_codemode_route_key_without_releasing_other_users() {
    let (host, wire) = route_host();
    host.configure_flow("codemode:run-1", Some(codemode_scope()));
    host.configure_flow("flow:other", Some(codemode_scope()));
    assert_eq!(
        wire.routes
            .lock()
            .unwrap()
            .route("flow:other", &target(), |_| Ok(13))
            .unwrap(),
        Some(13)
    );
    host.configure_flow("codemode:run-1", None);
    assert_eq!(
        wire.configurations.lock().unwrap().last(),
        Some(&("codemode:run-1".into(), None))
    );
    assert!(
        matches!(wire.routes.lock().unwrap().route("codemode:run-1", &target(), |_| panic!("released scopes cannot open routes")), Err(WireError::Typed(refusal)) if refusal.reason == basal_host::flow_refusal::RefusalReason::NoFlowScope)
    );
    assert_eq!(
        wire.routes
            .lock()
            .unwrap()
            .route("flow:other", &target(), |_| panic!(
                "the other user's handle remains cached"
            ))
            .unwrap(),
        Some(13)
    );
    host.configure_flow("flow:other", None);
    assert!(
        wire.routes
            .lock()
            .unwrap()
            .route("flow:other", &target(), |_| panic!(
                "the last user released this route"
            ))
            .is_err()
    );
}

#[test]
fn configure_flow_release_of_an_unregistered_codemode_key_is_a_noop() {
    let (host, wire) = route_host();
    host.configure_flow("flow:other", Some(codemode_scope()));
    assert_eq!(
        wire.routes
            .lock()
            .unwrap()
            .route("flow:other", &target(), |_| Ok(17))
            .unwrap(),
        Some(17)
    );
    host.configure_flow("codemode:never-registered", None);
    host.configure_flow("codemode:never-registered", None);
    assert_eq!(
        wire.routes
            .lock()
            .unwrap()
            .route("flow:other", &target(), |_| panic!(
                "an unknown release must not evict registered routes"
            ))
            .unwrap(),
        Some(17)
    );
}

#[test]
fn flow_versions_keep_distinct_routes_during_activation_overlap() {
    let mut routes = ScopedRoutes::<u64>::default();
    let v1 = flow_route_key("flow-a", 1);
    let v2 = flow_route_key("flow-a", 2);
    let mut first = codemode_scope();
    first.selector.scope_ref = "approved-v1".into();
    let mut second = first.clone();
    second.selector.scope_ref = "approved-v2".into();
    routes.configure(&v1, Some(first.clone()));
    assert_eq!(routes.route(&v1, &target(), |_| Ok(11)).unwrap(), Some(11));
    assert!(routes.configure(&v2, Some(second.clone())).is_empty());
    assert_eq!(
        routes
            .route(&v2, &target(), |scope| {
                assert_eq!(scope, &second.selector);
                Ok(22)
            })
            .unwrap(),
        Some(22)
    );
    assert_eq!(
        routes
            .route(&v1, &target(), |_| panic!(
                "the in-flight v1 route stays open"
            ))
            .unwrap(),
        Some(11)
    );
    assert_eq!(
        routes.retain_flow_routes(&[v2.clone()].into_iter().collect()),
        vec![11]
    );
    assert!(
        routes
            .route(&v1, &target(), |_| panic!("retired version cannot reopen"))
            .is_err()
    );
    assert_eq!(
        routes
            .route(&v2, &target(), |_| panic!("v2 stays cached"))
            .unwrap(),
        Some(22)
    );
}

#[test]
fn flow_versions_sharing_a_scope_release_only_the_last_handle() {
    let mut routes = ScopedRoutes::<u64>::default();
    let v1 = flow_route_key("flow-a", 1);
    let v2 = flow_route_key("flow-a", 2);
    routes.configure(&v1, Some(codemode_scope()));
    routes.configure(&v2, Some(codemode_scope()));
    assert_eq!(routes.route(&v1, &target(), |_| Ok(11)).unwrap(), Some(11));
    assert_eq!(
        routes
            .route(&v2, &target(), |_| panic!(
                "versions share the scope's handle"
            ))
            .unwrap(),
        Some(11)
    );
    assert!(
        routes
            .retain_flow_routes(&[v2.clone()].into_iter().collect())
            .is_empty()
    );
    assert!(
        routes
            .route(&v1, &target(), |_| panic!("v1 was released"))
            .is_err()
    );
    assert_eq!(
        routes
            .route(&v2, &target(), |_| panic!("v2 still owns the handle"))
            .unwrap(),
        Some(11)
    );
    assert_eq!(routes.retain_flow_routes(&Default::default()), vec![11]);
    assert!(routes.retain_flow_routes(&Default::default()).is_empty());
}
