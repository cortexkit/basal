use basal_host::flow_scope::{FlowScope, ScopedRoutes};
use subc_protocol::{Principal, RouteTarget};

#[test]
fn route_cache_reuses_handles_and_replaces_changed_selectors() {
    let mut routes=ScopedRoutes::<u64>::default();
    let mut scope=FlowScope {owner:Principal::Reserved {module_id:"prefrontal-core".into()},scope_ref:"flow-a".into(),epoch:1};
    let target=RouteTarget::ManagementSurface {module_id:"broca".into()};
    routes.configure("flow-a",true,Some(scope.clone()));
    assert_eq!(routes.route("flow-a",&target,|actual| {assert_eq!(actual,&scope);Ok(7)}).unwrap(),Some(7));
    assert_eq!(routes.route("flow-a",&target,|_|Ok(11)).unwrap(),Some(7));
    scope.epoch=2;
    assert_eq!(routes.configure("flow-a",true,Some(scope.clone())),vec![7]);
    assert_eq!(routes.route("flow-a",&target,|actual|{assert_eq!(actual,&scope);Ok(13)}).unwrap(),Some(13));
    scope.scope_ref="flow-replacement".into();
    assert_eq!(routes.configure("flow-a",true,Some(scope.clone())),vec![13]);
    assert_eq!(routes.route("flow-a",&target,|actual|{assert_eq!(actual,&scope);Ok(17)}).unwrap(),Some(17));
}
