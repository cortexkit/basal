//! The daemon-attested prefrontal module and principal.

pub const CORE_MODULE: &str = "prefrontal";
pub const CORE_PRINCIPAL: &str = "reserved:prefrontal";

pub fn is_core_module(id: &str) -> bool {
    id == CORE_MODULE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{Transport, WireError};
    use serde_json::{Value, json};

    #[test]
    fn approved_versions_requires_the_prefrontal_declaration() {
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
                assert_eq!(module, CORE_MODULE);
                assert_eq!(op, "package.approved_versions");
                assert_eq!(params, json!({"package":"package"}));
                Ok(json!({"package":"package","newest_approved":7}))
            }
            fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
                unreachable!()
            }
        }
        assert_eq!(
            Provider(Some(CORE_MODULE))
                .newest_approved_template_version("package")
                .unwrap(),
            Some(7)
        );
        assert!(
            matches!(Provider(Some("prefrontal-core")).newest_approved_template_version("package"), Err(WireError::Refused { code, .. }) if code == "version_required")
        );
        assert!(matches!(
            Provider(None).newest_approved_template_version("package"),
            Err(WireError::NeverSent(_))
        ));
    }
}
