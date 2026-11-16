//! Astrocyte's replayable, privacy-bounded ownership feed.
use super::*;

impl Module {
    pub(super) fn op_attribution_read(&self, caller: &Caller, params: Value) -> OpResult {
        // Even an attested operator is refused: this feed is reserved to
        // Astrocyte, not exposed as an operator surface.
        if !matches!(caller, Caller::Module { module_id } if module_id == "astrocyte") {
            return Err(OpError::new(
                "not_permitted",
                "attribution.read is reserved to astrocyte",
            ));
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            after: Option<i64>,
            limit: Option<usize>,
        }
        let p: Params = serde_json::from_value(if params.is_null() { json!({}) } else { params })
            .map_err(invalid_params)?;
        let after = p.after.unwrap_or(0);
        let limit = p.limit.unwrap_or(100);
        if after < 0 || !(1..=500).contains(&limit) {
            return Err(invalid_params(
                "after must be nonnegative and limit must be 1..500",
            ));
        }
        self.rt
            .store()
            .read(|c| basal_core::attribution::read(c, after, limit))
            .map_err(|e| self.core_error(e))
    }
}
