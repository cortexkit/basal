use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const CATALOG_JSON: &str = r#"{
  "schema_version": 1,
  "module_id": "basal",
  "entries": [
    {
      "entry_id": "basal-store",
      "class": "portable",
      "reason": "Restoring this database onto another machine while this basal instance still runs would run its flows twice. Restore only onto a machine where this basal instance no longer runs.",
      "mechanism": "page-db",
      "path": "store.db",
      "writer_interaction": "read-transaction-live"
    },
    {
      "entry_id": "basal-writer-lease",
      "class": "portable",
      "reason": "Restore this writer lease only with the matching database and only onto a machine where this basal instance no longer runs; leaving basal running on both machines would run its flows twice.",
      "mechanism": "filetree",
      "path": "a820ffc03d506451.lease"
    },
    {
      "entry_id": "basal-logs",
      "class": "excluded",
      "reason": "Operational logs are not durable module state and may contain sensitive operational details.",
      "path": "logs"
    },
    {
      "entry_id": "basal-dry-run",
      "class": "excluded",
      "reason": "Dry-run output is temporary scratch that can be regenerated and must not be restored.",
      "path": "dry-run"
    }
  ]
}
"#;

fn catalog_bytes() -> &'static [u8] {
    CATALOG_JSON.as_bytes()
}

/// Writes the static backup descriptor beside the store path resolved by the daemon.
/// A catalog is useful only when it cannot prevent the module from starting.
pub(crate) fn write_for_store(store_path: &Path) {
    let Some(data_dir) = store_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        tracing::warn!(
            target: "engram",
            store_path = %store_path.display(),
            "cannot determine basal data directory for the engram catalog"
        );
        return;
    };

    let destination = data_dir.join("engram-catalog.json");
    if let Err(error) =
        write_if_changed(&destination, catalog_bytes()).and_then(|_| owner_only(&destination))
    {
        tracing::warn!(
            target: "engram",
            path = %destination.display(),
            %error,
            "could not write basal engram catalog"
        );
    }
}

/// Keeps the catalog readable only by its owner, like the rest of basal's data
/// folder. This also repairs a file an earlier build wrote with the default
/// mode, which the unchanged-bytes check would otherwise leave alone. On
/// Windows the data folder's owner-only DACL already covers the file.
#[cfg(unix)]
fn owner_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if fs::metadata(path)?.permissions().mode() & 0o777 != 0o600 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn owner_only(_: &Path) -> io::Result<()> {
    Ok(())
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    write_if_changed_with(path, bytes, atomic_write)
}

fn write_if_changed_with(
    path: &Path,
    bytes: &[u8],
    writer: impl FnOnce(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<bool> {
    if fs::read(path).is_ok_and(|existing| existing == bytes) {
        return Ok(false);
    }
    writer(path, bytes)?;
    Ok(true)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "catalog path has no file name")
    })?;

    for _ in 0..128 {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            id
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = match options.open(&temp_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };

        let result = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary catalog file",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn catalog_matches_golden_and_is_written_beside_resolved_store() {
        let golden = include_bytes!("../testdata/engram-catalog.json");
        assert_eq!(catalog_bytes(), golden);

        let descriptor: serde_json::Value =
            serde_json::from_slice(catalog_bytes()).expect("catalog constant is valid JSON");
        assert_eq!(descriptor["schema_version"], 1);
        assert_eq!(descriptor["module_id"], "basal");
        let entries = descriptor["entries"].as_array().expect("catalog entries");
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0]["entry_id"], "basal-store");
        assert_eq!(entries[0]["class"], "portable");
        assert_eq!(entries[0]["mechanism"], "page-db");
        assert_eq!(entries[0]["writer_interaction"], "read-transaction-live");
        assert_eq!(entries[0]["path"], "store.db");
        assert_eq!(entries[1]["entry_id"], "basal-writer-lease");
        assert_eq!(entries[1]["class"], "portable");
        assert_eq!(entries[1]["mechanism"], "filetree");
        assert_eq!(entries[1]["path"], "a820ffc03d506451.lease");
        assert_eq!(entries[2]["entry_id"], "basal-logs");
        assert_eq!(entries[2]["class"], "excluded");
        assert_eq!(entries[3]["entry_id"], "basal-dry-run");
        assert_eq!(entries[3]["class"], "excluded");
        for entry in entries {
            assert!(
                !entry["reason"].as_str().unwrap().trim().is_empty(),
                "every catalog entry explains its restore classification"
            );
        }

        let root = basal_core::test_support::timestamp_scratch("engram-catalog", "golden");
        let data_dir = root.join("resolved-data-dir");
        let store_path = data_dir.join("store.db");
        write_for_store(&store_path);
        assert_eq!(
            fs::read(data_dir.join("engram-catalog.json")).expect("catalog output"),
            golden
        );
        fs::remove_dir_all(root).expect("remove catalog fixture");
    }

    #[cfg(unix)]
    #[test]
    fn catalog_is_owner_only_when_written_and_when_already_current() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let root = basal_core::test_support::timestamp_scratch("engram-catalog", "mode");
        let data_dir = root.join("resolved-data-dir");
        let destination = data_dir.join("engram-catalog.json");

        write_for_store(&data_dir.join("store.db"));
        assert_eq!(mode(&destination), 0o600, "a fresh catalog");

        // A current file an earlier build left world-readable is repaired too,
        // although its bytes match and the writer is skipped.
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o644)).unwrap();
        write_for_store(&data_dir.join("store.db"));
        assert_eq!(mode(&destination), 0o600, "an existing 0644 catalog");
        fs::remove_dir_all(root).expect("remove catalog fixture");
    }

    #[test]
    fn matching_catalog_bytes_skip_the_writer() {
        let root = basal_core::test_support::timestamp_scratch("engram-catalog", "unchanged");
        let destination = root.join("engram-catalog.json");
        fs::write(&destination, catalog_bytes()).expect("seed matching catalog");
        let calls = Cell::new(0);

        let changed = write_if_changed_with(&destination, catalog_bytes(), |_, _| {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .expect("matching catalog needs no write");

        assert!(!changed);
        assert_eq!(calls.get(), 0, "the file writer must not be called");
        fs::remove_dir_all(root).expect("remove catalog fixture");
    }

    #[test]
    fn catalog_write_failure_is_nonfatal() {
        let root = basal_core::test_support::timestamp_scratch("engram-catalog", "failure");
        let data_dir_blocker = root.join("data-is-a-file");
        fs::write(&data_dir_blocker, b"not a directory").expect("create blocking file");

        write_for_store(&data_dir_blocker.join("store.db"));

        assert_eq!(fs::read(&data_dir_blocker).unwrap(), b"not a directory");
        fs::remove_dir_all(root).expect("remove catalog fixture");
    }
}
