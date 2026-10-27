//! Intake-only checks of the operations a module serves to flows.

use basal_host::{Catalog, subc_catalog::Surface};
use serde_json::Value;

use crate::InstallError;

pub(crate) fn validate(
    catalog: &dyn Catalog,
    module: &str,
    op: &str,
    tool: Option<&Value>,
) -> Result<(), InstallError> {
    if basal_host::core_host::is_core_carrier_op(module, op) {
        return Ok(());
    }
    let refused = |surface, allowed, reason| InstallError::FlowOpNotServed {
        module: module.into(),
        op: op.into(),
        surface,
        allowed,
        reason,
    };
    if let Some(tool) = tool {
        // An omitted modes field means classic-only in the provider contract.
        if !tool["modes"]
            .as_array()
            .is_some_and(|modes| modes.iter().any(|mode| mode == "flow"))
        {
            return Err(refused(
                Surface::Tool,
                None,
                "tool.catalog modes do not include flow".into(),
            ));
        }
    } else if let Some(allowed) = catalog.flow_operations(module).map_err(|error| {
        refused(
            Surface::Management,
            None,
            format!("cannot read management flow_operations: {error:?}"),
        )
    })? && !allowed.iter().any(|allowed| allowed == op)
    {
        return Err(refused(
            Surface::Management,
            Some(allowed),
            "not listed in the module's flow_operations".into(),
        ));
    }
    if !catalog.supports_flow_calls(module) {
        return Err(InstallError::FlowScopeUnsupported {
            module: module.into(),
            op: op.into(),
        });
    }
    Ok(())
}
