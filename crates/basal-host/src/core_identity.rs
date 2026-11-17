//! Prefrontal's rename aliases. Stored identities stay unchanged; only routing
//! and identity comparisons resolve the aliases.

pub const CORE_MODULE: &str = "prefrontal";
pub const LEGACY_CORE_MODULE: &str = "prefrontal-core";
pub const CORE_PRINCIPAL: &str = "reserved:prefrontal";
pub const LEGACY_CORE_PRINCIPAL: &str = "reserved:prefrontal-core";

pub fn is_core_module(id: &str) -> bool {
    matches!(id, CORE_MODULE | LEGACY_CORE_MODULE)
}

pub fn is_core_principal(principal: &str) -> bool {
    matches!(principal, CORE_PRINCIPAL | LEGACY_CORE_PRINCIPAL)
}

pub fn same_module(left: &str, right: &str) -> bool {
    left == right || (is_core_module(left) && is_core_module(right))
}

pub fn resolve(listed: impl Fn(&str) -> bool) -> Option<&'static str> {
    [CORE_MODULE, LEGACY_CORE_MODULE]
        .into_iter()
        .find(|id| listed(id))
}

pub fn catalog_module<'a>(
    catalog: &'a crate::catalog_snapshot::CatalogSnapshot,
    id: &str,
) -> Option<&'a crate::catalog_snapshot::Module> {
    let id = if is_core_module(id) {
        resolve(|id| catalog.module(id).is_some())?
    } else {
        id
    };
    catalog.module(id)
}

/// Resolve routing identity without catalog I/O, including on async threads.
/// Without a catalogued core, use its legacy id and let the daemon report a
/// route miss rather than refusing the call locally.
pub fn core_module<T: crate::transport::Transport + ?Sized>(transport: &T) -> &'static str {
    transport.core_module()
}

pub fn routed_module<'a>(
    catalog: &crate::catalog_snapshot::CatalogSnapshot,
    id: &'a str,
) -> &'a str {
    if is_core_module(id) {
        resolve(|id| catalog.module(id).is_some()).unwrap_or(LEGACY_CORE_MODULE)
    } else {
        id
    }
}

/// Use the existing acknowledgement key without changing historical entries.
pub fn custodian_aliases(principal: &str) -> (&str, &str) {
    if is_core_principal(principal) {
        (LEGACY_CORE_PRINCIPAL, CORE_PRINCIPAL)
    } else {
        (principal, principal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::transport::{Transport, WireError};
    use serde_json::{Value, json};

    #[test]
    fn only_prefrontal_aliases_are_equivalent() {
        for module in [CORE_MODULE, LEGACY_CORE_MODULE] {
            assert!(is_core_module(module));
            assert!(same_module(module, CORE_MODULE));
            assert!(same_module(module, LEGACY_CORE_MODULE));
        }
        for principal in [CORE_PRINCIPAL, LEGACY_CORE_PRINCIPAL] {
            assert!(is_core_principal(principal));
        }
        assert!(!is_core_module("prefrontal-routing"));
        assert!(!is_core_principal("agent:prefrontal"));
        assert!(!same_module(CORE_MODULE, "broca"));
    }

    #[tokio::test]
    async fn routing_identity_never_reads_the_catalog_in_async_context() {
        struct NoCatalog;
        impl Transport for NoCatalog {
            fn catalog(&self) -> Result<Value, WireError> {
                panic!("routing identity must not read the catalog")
            }
            fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
                unreachable!()
            }
            fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
                unreachable!()
            }
        }
        assert_eq!(core_module(&NoCatalog), LEGACY_CORE_MODULE);
        assert_eq!(
            crate::Catalog::core_module(&crate::subc_catalog::SubcCatalog::new(
                std::sync::Arc::new(NoCatalog)
            )),
            LEGACY_CORE_MODULE
        );
    }

    #[test]
    fn catalog_selection_prefers_new_name_and_preserves_missing_core() {
        for (listed, expected) in [
            (vec![LEGACY_CORE_MODULE], Some(LEGACY_CORE_MODULE)),
            (vec![CORE_MODULE], Some(CORE_MODULE)),
            (vec![LEGACY_CORE_MODULE, CORE_MODULE], Some(CORE_MODULE)),
            (vec![], None),
        ] {
            assert_eq!(resolve(|id| listed.contains(&id)), expected);
            let catalog = crate::catalog_snapshot::CatalogSnapshot::from_value(serde_json::json!({"modules":listed.iter().map(|id| serde_json::json!({"module_id":id})).collect::<Vec<_>>() })).unwrap();
            for requested in [CORE_MODULE, LEGACY_CORE_MODULE] {
                assert_eq!(
                    catalog_module(&catalog, requested).is_some(),
                    expected.is_some()
                );
                for op in crate::core_host::CORE_CARRIER_OPS {
                    assert!(crate::core_host::is_core_carrier_op(requested, op));
                }
            }
        }
    }

    #[test]
    fn approved_versions_uses_catalogued_core_and_propagates_catalog_failure() {
        struct Provider(Option<&'static str>);
        impl Transport for Provider {
            fn catalog(&self) -> Result<Value, WireError> {
                let module = self
                    .0
                    .ok_or_else(|| WireError::NeverSent("offline".into()))?;
                Ok(
                    json!({"modules":[{"module_id":module,"capabilities":{"provides":[crate::transport::PACKAGE_APPROVED_VERSIONS_CAPABILITY]}}]}),
                )
            }
            fn management(
                &self,
                module: &str,
                op: &str,
                params: Value,
            ) -> Result<Value, WireError> {
                assert_eq!(Some(module), self.0);
                assert_eq!(op, "package.approved_versions");
                assert_eq!(params, json!({"package":"package"}));
                Ok(json!({"package":"package","newest_approved":7}))
            }
            fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
                unreachable!()
            }
        }
        for module in [LEGACY_CORE_MODULE, CORE_MODULE] {
            assert_eq!(
                Provider(Some(module))
                    .newest_approved_template_version("package")
                    .unwrap(),
                Some(7)
            );
        }
        assert!(matches!(
            Provider(None).newest_approved_template_version("package"),
            Err(WireError::NeverSent(_))
        ));
    }
}
