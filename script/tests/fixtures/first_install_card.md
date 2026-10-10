@SUBC basal: a FIRST placement, two binaries, one card each (ck-basal, ck-basal-worker)

**Card**
- stage: @STAGE@ (both binaries)
- ck-basal: @STAGE@/ck-basal, sha256 basal-digest; basal.current revision @SHA@
- ck-basal-worker: @STAGE@/ck-basal-worker, sha256 worker-digest; basal-worker.current revision @SHA@
- build: release, clean tree, CK_BUILD_GIT_SHA=@SHA@ CK_BUILD_GIT_DIRTY=false, commit on origin/main (fixture).
- CI for that commit: completed success, https://example.test/push
- daemon floor: 0.20.53, the oldest daemon a subc-protocol 0.30 module is known to run against.

**Signing** (both): ad hoc, hardened runtime, explicit identifier, no entitlements, no get-task-allow, 0 debug-map entries
- ck-basal: Identifier=ck-basal flags=0x10000(runtime)
- ck-basal-worker: Identifier=ck-basal-worker flags=0x10000(runtime)
- no entitlements: the worker's QuickJS is an interpreter and needs no JIT; its sandbox is a Seatbelt profile it applies to itself.
- smoke-tested on the signed files: ck-basal --manifest (module_id basal, provenance build_git_sha @SHA@); --version on both; the worker's sandbox self-check (script/sign-worker.sh verify: embedded profile, and a file read, a socket connect and a program launch all denied); lldb -p refused on both, with an unhardened ad-hoc copy of /bin/sleep as the control that did attach. Inode and flags unchanged across the run.

**Marker and control** (strings table)
- marker: @SHA@ (the full commit, embedded by the build)
  - ck-basal: staged 1 / live none (no running ck-basal)
  - ck-basal-worker: staged 1 / live none (no running ck-basal-worker)
- control: a string every build of each binary carries
  - ck-basal "flow.install": staged 1 / live none (no running ck-basal)
  - ck-basal-worker "refusing to run unconfined": staged 1 / live none (no running ck-basal-worker)
- This is a first install: there is no running basal, so place each binary with place-module.sh's `--install` mode and the marker above (`--module basal --staged @STAGE@/ck-basal --marker @SHA@ --install`, then the same for ck-basal-worker with `--dest ~/.local/share/cortexkit/bin/ck-basal-worker`). It checks that the destination does not exist, that the binary is hardened with no get-task-allow, that its identifier matches the destination name, and that the marker is in the staged binary. No restart: basal starts on `ck module rescan` once its config entry exists.

**Store and formats**
- new module: no migration, no existing format, no format-floor.json. basal's store, <data_home>/cortexkit/basal/store.db, is created on first start. check-format-floors.sh finds no floors for basal and passes.

**Order**
- both binaries in ~/.local/share/cortexkit/bin before basal first starts: ck-basal runs ck-basal-worker from its own directory under that exact name.
- the daemon has no basal module yet: its subc.jsonc entry is the operator's approval step and comes after both files are in place. The entry: no environment (basal finds its store from its home), `reserved: true` (core accepts basal's install and decision cards only from reserved:basal), and exclusive overlap (the journal is a single-writer database).

**Post-placement check** (mine)
- `ck --json provenance basal`: build_git_sha @SHA@, and the observed pid's running image is the placed ck-basal (same inode).
- a worker's running image is the placed ck-basal-worker (same inode) once a flow runs.
- BASAL_WORKER_IDENTIFIER=ck-basal-worker sh script/sign-worker.sh verify ~/.local/share/cortexkit/bin/ck-basal-worker passes at the placed path.
- basal's flow.list answers (no flows yet).
