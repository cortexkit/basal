mod common;

use std::sync::Arc;

#[test]
fn fixture_releases_its_store_before_removing_the_directory() {
    let mut store = None;
    let fixture =
        common::fixture_with_store("fixture-teardown", common::Options::default(), |opened| {
            store = Some(Arc::downgrade(&opened));
            Ok(())
        });
    let store = store.unwrap();
    let directory = fixture.dir.clone();
    assert!(store.upgrade().is_some());
    drop(fixture);
    assert!(store.upgrade().is_none());
    assert!(!directory.exists());
}
