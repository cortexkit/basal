# fs.write temporary file cleanup: mutation proofs

`fs.write` records each temporary file in the store's `fs_temps` table
(schema 14) before it creates the file, and clears the record once the file
has been renamed over its target and the rename is durable. Every
maintenance pass (`Runtime::enforce_deadlines`, through `retention_due`)
removes any recorded file that no write in this process still holds, then
drops its record. Removal deletes only a regular file with exactly the
recorded name, in exactly the recorded directory, inside the write's roots.
A temporary file is named after the call's idempotency key
(`.basal-call-<key>.tmp`), so a resent call finds and replaces what its
earlier send left. A one-time pass per store removes regular files left by
earlier versions, named `.basal-<pid>-<seq>.tmp`, from the directories
journaled writes went to.

The guards are tested in `crates/basal-testkit/tests/fs_write_temps.rs`.
Every assertion checks state on disk and in the store at a point the test
chose; no assertion depends on elapsed time.

Every row was replayed with `ckdev-mutate` 0.9.8 using `run --only <id>`.
Linux rows ran on the Linux build server and macOS rows ran locally. Every
row reported `CAUGHT`. The expected test went red each time; other tests in
`--test fs_write_temps` that went red are listed. No other test target was
replayed (`--broad` was not run), so no row is marked as a hub.

| Row | Platform | Red |
|---|---|---|
| `fs-write-creates-its-temporary-file-before-recording-it` | linux | crash, cancel, root-retarget, success |
| `fs-write-keeps-its-record-after-the-rename` | macos | success, crash, resend |
| `fs-write-fails-on-the-file-its-earlier-send-left` | linux | resend |
| `fs-temp-removal-removes-what-is-not-a-regular-file` | linux | crash, legacy, remove_temp unit |
| `fs-temp-removal-accepts-a-directory-that-resolves-elsewhere` | macos | crash |
| `fs-temp-removal-trusts-the-recorded-directory-over-the-roots` | linux | root-retarget |
| `fs-legacy-temp-name-matches-more-than-the-old-shape` | macos | legacy, cancel, crash, remove_temp unit |
| `fs-temp-sweep-removes-a-file-a-live-write-holds` | linux | cancel |
| `fs-temp-sweep-is-not-run-by-maintenance` | macos | crash, cancel, root-retarget |
| `fs-temp-sweep-skips-startup` | macos | crash, root-retarget |
| `fs-temp-sweep-ignores-a-write-that-ended-uncleared` | linux | cancel |

Short names used in the table:

- success: `a_successful_write_leaves_no_record_and_no_temporary_file`
- cancel: `a_cancelled_runs_write_that_dies_mid_flight_is_cleaned_up`
- crash: `a_crash_mid_write_is_cleaned_up_by_recovery_without_following_a_swapped_path`
- root-retarget: `a_crash_record_whose_directory_left_its_root_is_not_acted_on`
- resend: `a_resent_write_replaces_the_file_its_earlier_send_left`
- legacy: `legacy_temporary_files_are_removed_once_from_written_directories`
- remove_temp unit: `remove_temp_removes_only_a_recorded_regular_file`

## Limits

- One window remains between the `lstat` that finds a regular file and the
  `unlinkat` that removes it. A file swapped for a symlink inside that window
  loses the symlink entry itself; `unlinkat` never follows it, so the target
  is untouched.
- A record whose directory no longer resolves inside its roots is dropped
  without touching anything. The same happens when a root that was a
  directory is gone, for example on an unmounted volume. A file that record
  named is not removed later.
- The legacy pass runs once per store and finds only directories still
  named by journal rows that were not yet pruned.
