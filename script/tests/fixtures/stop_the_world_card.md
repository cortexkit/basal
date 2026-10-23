@SUBC basal: a STOP-THE-WORLD UPDATE, stop ck-basal before replacing either binary

macOS developer placement.

**Card**
- stage: @STAGE@ (both binaries)
- ck-basal: @STAGE@/ck-basal, sha256 basal-digest; basal.current revision @SHA@
- ck-basal-worker: @STAGE@/ck-basal-worker, sha256 worker-digest; basal-worker.current revision @SHA@
- build: release, clean tree, CK_BUILD_GIT_SHA=@SHA@ CK_BUILD_GIT_DIRTY=false, commit on origin/main (fixture).
- CI for that commit (push event): completed success, https://example.test/push
- daemon floor: 0.20.53, the oldest daemon a subc-protocol 0.30 module is known to run against.

**macOS signing** (both): ad hoc, hardened runtime, explicit identifier, no entitlements, no get-task-allow, 0 debug-map entries
- ck-basal: Identifier=ck-basal flags=0x10000(runtime)
- ck-basal-worker: Identifier=ck-basal-worker flags=0x10000(runtime)
- no entitlements: the worker's QuickJS is an interpreter and needs no JIT; macOS sandbox: a Seatbelt profile it applies to itself.
- smoke-tested on the signed files: ck-basal --manifest (module_id basal, provenance build_git_sha @SHA@); --version on both; the worker's macOS sandbox self-check (script/sign-worker.sh verify: embedded profile, and a file read, a socket connect and a program launch all denied); macOS debugger hardening: lldb -p refused on both, with an unhardened ad-hoc copy of /bin/sleep as the control that did attach. Inode and flags unchanged across the run.

**Marker and control** (strings table, checked against the live destinations)
- ck-basal marker "new basal": staged 2 / live 0
- ck-basal control "basal control": staged 1 / live 1
- ck-basal-worker marker "new worker": staged 2 / live 0
- ck-basal-worker control "worker control": staged 1 / live 1

**Store and rollback**
- @HOME@/.local/share/cortexkit/basal/store.db: build schema 7 / live schema 6 (SELECT max(version) FROM cortexkit_schema_version, mode=ro).
- This update migrates the store. The ck-basal command below includes `--migrates ~/.local/share/cortexkit/basal/store.db` so place-module.sh snapshots it before placement.
- Rollback is the old binary AND the store backup together, with both binaries restored as a pair. A binary-only rollback refuses to open the newer store: basal_core::Store::open returns "the store's schema is at version N but this build knows only up to M". Stop basal before restoring the store backup; do not let it reopen the store until both old binaries and the backup are restored.

**Order and restart** (stop-the-world)
- Measured worker protocol: staged 11 / live 10. Different versions cannot share a running parent: stop basal before either placement, and keep it stopped until both files are replaced.
1. Back up the store read-only before stopping (retain the directory and sidecar for rollback):
   `BACKUP=$(mktemp -d ~/.local/share/cortexkit/staging/basal-store.XXXXXX) && sqlite3 "file:$HOME/.local/share/cortexkit/basal/store.db?mode=ro" ".backup '$BACKUP/store.db'" && (cd "$BACKUP" && shasum -a 256 store.db > store.db.sha256 && shasum -c store.db.sha256)`
2. Stop and disable the module (persists until start); confirm no basal parent or worker remains before placing:
   `ck module stop basal`
3. Place both binaries without restarting:
   `place-module.sh --module basal --staged '@STAGE@/ck-basal-worker' --marker 'new worker' --control 'worker control' --dest ~/.local/share/cortexkit/bin/ck-basal-worker --no-restart`
   `place-module.sh --module basal --staged '@STAGE@/ck-basal' --marker 'new basal' --control 'basal control' --migrates ~/.local/share/cortexkit/basal/store.db --no-restart`
4. Only after both placements succeed, enable and start the module:
   `ck module start basal`
- The place-module.sh commands are read-only gate runs; the SUBC operator adds `--place` to both for actual placement. Never start between placements. If either fails, leave basal stopped and recover the binary pair and matching store backup before starting.

**Post-placement checks**
- `ck --json provenance basal`: build_git_sha @SHA@, and after allowing up to five seconds for the ck-subc trampoline to exec, the observed pid's running image is the placed ck-basal (same inode); fail if it never becomes that file.
- a worker's running image is the placed ck-basal-worker (same inode) once a flow runs; if it briefly reports the ck-subc trampoline, retry for up to five seconds and fail if it never becomes the placed file.
- macOS sandbox: BASAL_WORKER_IDENTIFIER=ck-basal-worker sh script/sign-worker.sh verify ~/.local/share/cortexkit/bin/ck-basal-worker passes at the placed path.
- the store reads schema version 7 with its tables intact.
- basal's flow.list answers.
