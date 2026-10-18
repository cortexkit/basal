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
      "reason": "Restoring this database onto another Mac while this basal instance still runs would run its flows twice. Restore only onto a machine where this basal instance no longer runs.",
      "mechanism": "whole-db",
      "path": "store.db"
    },
    {
      "entry_id": "basal-writer-lease",
      "class": "portable",
      "reason": "Restore this writer lease only with the matching database and only onto a machine where this basal instance no longer runs; leaving basal running on both Macs would run its flows twice.",
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
    if let Err(error) = write_if_changed(&destination, catalog_bytes()) {
        tracing::warn!(
            target: "engram",
            path = %destination.display(),
            %error,
            "could not write basal engram catalog"
        );
    }
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
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
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
        assert_eq!(entries[0]["mechanism"], "whole-db");
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

    #[cfg(target_os = "linux")]
    #[test]
    fn python_catalog_guard_matches_golden() {
        use std::process::Command;

        let root = basal_core::test_support::timestamp_scratch("engram-catalog", "python");
        let data_dir = root.join("resolved-data-dir");
        write_for_store(&data_dir.join("store.db"));

        let module_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let checker = module_dir.join("../../script/tests/engram_catalog.py");
        let golden = module_dir.join("testdata/engram-catalog.json");
        let written = data_dir.join("engram-catalog.json");
        let result = Command::new("python3")
            .arg(checker)
            .arg(written)
            .arg(golden)
            .output()
            .expect("run the catalog's Python golden-file check");
        assert!(
            result.status.success(),
            "Python catalog check failed: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );

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
