use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use basal_host::catalog_snapshot::CatalogSnapshot;

use super::*;

struct CatalogTransport {
    snapshot: Arc<CatalogSnapshot>,
    catalog_reads: AtomicUsize,
}

impl Transport for CatalogTransport {
    fn catalog(&self) -> Result<Value, WireError> {
        self.catalog_reads.fetch_add(1, Ordering::Relaxed);
        Ok(broca_catalog())
    }

    fn catalog_snapshot(&self) -> Result<Arc<CatalogSnapshot>, WireError> {
        Ok(self.snapshot.clone())
    }

    fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
        unreachable!()
    }

    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        unreachable!()
    }
}

fn broca_catalog() -> Value {
    json!({"modules":[{"module_id":"broca",
        "capabilities":{"provides":["agent-run-scopes/v1"]}}]})
}

#[test]
fn model_provider_check_reuses_the_catalog_snapshot() {
    let transport = CatalogTransport {
        snapshot: CatalogSnapshot::from_value(broca_catalog()).unwrap(),
        catalog_reads: AtomicUsize::new(0),
    };
    for _ in 0..2 {
        provider_ready(&transport).unwrap();
    }
    assert_eq!(transport.catalog_reads.load(Ordering::Relaxed), 0);
}
