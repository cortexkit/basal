mod common;
use basal_host::{OpDecl, OpKind};
use basal_module::caller::Caller;
use common::{Options, events_manifest, fixture};
use serde_json::json;

#[test]
fn core_op_install_requires_catalog_capability_for_every_author() {
    let f=fixture("core-op-cap",Options::default());
    f.catalog.set_op("prefrontal-core","board.read",OpDecl{kind:Some(OpKind::Query),cause_echo:false,shell_capable:false});
    for caller in [Caller::Operator, common::agent("SYNAPSE")] {
        let mut manifest=events_manifest(if caller==Caller::Operator {"flow-op-global"} else {"flow-op-owned"});
        manifest["ops"]=json!([{"module":"prefrontal-core","op":"board.read"}]);
        let params=json!({"script":"return 7;","manifest":manifest.to_string()});
        let error=f.module.handle(&caller,"flow.install",params.clone()).unwrap_err();
        assert!(error.message.contains("prefrontal-core") && error.message.contains("board.read"),"{error:?}");
        f.catalog.set_flow_capable("prefrontal-core",true);
        assert!(f.module.handle(&caller,"flow.install",params).is_ok());
        f.catalog.set_flow_capable("prefrontal-core",false);
    }
}

#[test]
fn dry_run_rechecks_core_op_catalog_capability() {
    let f=fixture("dry-core-cap",Options::default());
    f.catalog.set_op("prefrontal-core","board.read",OpDecl{kind:Some(OpKind::Query),cause_echo:false,shell_capable:false});
    let mut manifest=events_manifest("flow-dry-core");
    manifest["ops"]=json!([{"module":"prefrontal-core","op":"board.read"}]);
    f.catalog.set_flow_capable("prefrontal-core",true);
    f.module.handle(&Caller::Operator,"flow.install",json!({"script":"return 7;","manifest":manifest.to_string()})).unwrap();
    let params=json!({"flow_id":"flow-dry-core","trigger":{}});
    assert!(f.module.handle(&Caller::Operator,"flow.dry_run",params.clone()).is_ok());
    f.catalog.set_flow_capable("prefrontal-core",false);
    let error=f.module.handle(&Caller::Operator,"flow.dry_run",params).unwrap_err();
    assert!(error.message.contains("prefrontal-core") && error.message.contains("board.read"),"{error:?}");
}
