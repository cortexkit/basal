# Windows mutation control

The source replacement below removes only the worker's final startup handle
allowlist enforcement. The launcher keeps the full token, job and mitigations,
but `Deviation::HandleNotAllowed` omits its inherited-handle list. The parent
plants an inheritable file and asks the worker to write through that handle.

- **file:** `crates/basal-worker/src/confinement/windows/startup.rs`
- **old:** `    allowlist::check(&table).map_err(|error| Refusal::new(reason::HANDLE_NOT_ALLOWED, error))?;`
- **new:** `    let _ = allowlist::check(&table); // NON-VACUITY BREAK`
- **test that must fail:** `planted_handle_startup_refuses_and_the_allowlist_mutant_writes_the_fixture`
- **command:** `cargo test -p basal-worker --locked --test windows_probes -- planted_handle_startup_refuses_and_the_allowlist_mutant_writes_the_fixture --exact --nocapture`
- **required witness:** the failing assertion reports worker exit 0 and
  `fixture bytes [101, 115, 99, 97, 112, 101, 100]` (`escaped`). A compilation
  failure, refusal from another layer or crash is not this witness.

`tests/windows_mutation_witness.py` stages the live file, requires an empty
working diff, applies the replacement, records a non-empty diff and the named
red test, restores with Git checkout and touches the file, records an empty
diff, then requires the restored test to pass. It never changes `mutations.toml`.
The output directory is supplied by the caller and must be outside Cargo's
regenerable build directories.

## Committed import allowlist

This control removes a DLL name that the real production image imports from the
fixed committed inventory; it changes neither the image nor the PE parser.

- **file:** `crates/basal-worker/tests/data/windows-imports.txt`
- **old:** `kernel32.dll\n`
- **new:** `# NON-VACUITY BREAK\n`
- **test that must fail:** `the_worker_is_a_gui_image_without_gui_com_or_c_runtime_imports`
- **command:** `cargo test -p basal-worker --locked --test windows_image -- the_worker_is_a_gui_image_without_gui_com_or_c_runtime_imports --exact --nocapture`
- **required witness:** `kernel32.dll is not in` the committed import inventory.

The witness script runs this control after the startup allowlist control. Only
its named test runs; other tests in the target are reported as filtered, not as
passing. Both source files are restored before a successful script exit.
