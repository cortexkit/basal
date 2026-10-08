use basal_core::{Durability, Store};

#[test]
fn production_store_disables_unused_global_allocation_accounting() {
    let directory = std::env::temp_dir().join(format!("basal-sqlite-build-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Store::open(directory.join("basal.db"), Durability::default()).unwrap();
    let enabled: i64 = store
        .read(|connection| {
            Ok(connection.query_row(
                "SELECT sqlite_compileoption_used('DEFAULT_MEMSTATUS=0')",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    drop(store);
    std::fs::remove_dir_all(directory).unwrap();
    assert_eq!(
        enabled, 1,
        "SQLite must not serialize allocations to collect unused statistics"
    );
}
