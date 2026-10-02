# Deploying basal

basal never places its own binaries. In this fleet SUBC places every module binary with `subconscious/scripts/fleet/place-module.sh`, from a card the module stages; the fleet procedure is the knowhow skill `stage-module-card`. basal ships two binaries, `ck-basal` (the module) and `ck-basal-worker` (the confined QuickJS worker it runs flows in), and each gets its own card. [`script/stage.sh`](../script/stage.sh) builds, signs, smoke-tests and stages both and prints the card text. Posting it, and everything after, is a person's or SUBC's step.

## Staging

```sh
script/stage.sh                                   # a real stage, from a pushed commit
script/stage.sh --local-only --staging-root "$(mktemp -d)"   # rig testing only
script/stage.sh --verify <stage dir>              # re-check an existing stage
```

In order, `stage.sh`:

1. **Refuses a dirty tree**, untracked files included, and refuses unless `HEAD` is on `origin/main` (it fetches `origin main` first). `--local-only` skips only the `origin/main` check, for staging an unpushed commit onto the rig; the card, and a `pushed=no` line in each `.current` file, then say it was not pushed. basal has no remote yet, so until it has one only `--local-only` stages, and such a stage is for testing, never for handing over.
2. **Builds** `ck-basal` and `ck-basal-worker` in release mode, `--locked`, into `target/stage/`, with `CK_BUILD_GIT_SHA=<HEAD>` and `CK_BUILD_GIT_DIRTY` set as prefrontal's deploy script sets them. Each crate's `build.rs` embeds the commit: `ck-basal` declares it as the manifest's build provenance (`build_git_sha`), and both binaries print it from `--version`. It refuses if the build changed the tree, or if `ck-basal` contains the rig's kill switch (the string `BASAL_RIG_KILL_FILE`, present only in a `rig-kill-hook` build).
3. **Signs** a copy of each, by the policy in [`script/signing.sh`](../script/signing.sh): `codesign --force --sign - --options runtime --identifier ck-basal` (and `ck-basal-worker`). Ad hoc, hardened runtime, a plain identifier, no entitlements: the worker's QuickJS is an interpreter and needs no JIT, and its sandbox is a Seatbelt profile it applies to itself. Never `get-task-allow`. It then refuses unless the signature verifies strictly, the identifier is exact, the `flags=` line carries `runtime`, and there are no entitlements.
4. **Smoke-tests the signed files** directly, never through cargo, which relinks a binary and silently reverts it to a linker signature:
   - `ck-basal --manifest` must name module `basal` and declare the commit as `build_git_sha`; `--version` on both must name the commit, clean;
   - the worker's sandbox self-check, `script/sign-worker.sh verify` with `BASAL_WORKER_IDENTIFIER=ck-basal-worker`, as `flows-rig.sh place` runs it: the checked-in Seatbelt profile is embedded, and a file read, a socket connect and a program launch are all denied;
   - debugger hardening: an unhardened ad-hoc copy of `/bin/sleep` is held running and `lldb -p` must attach to it (the control, without which a refusal proves nothing), then each signed binary is held running (`--version`, with its stdout a full pipe so it blocks) and `lldb -p` must be refused. Each held process is checked to be running that file's image, by inode, before the attach.
   Each file's inode and codesign flags are recorded before and after; if either changed, it fails.
5. **Checks the marker and the control.** The marker is the full commit sha, which both binaries must embed (`strings | grep -c` at least 1). The controls, strings every build carries, are `flow.install` in `ck-basal` and `--confinement-probe` in the worker. Both are counted against the running binary in `~/.local/share/cortexkit/bin` when there is one.
6. **Stages** both under `<staging root>/basal-<short sha>/`, with a `.sha256` sidecar each that passes `shasum -c`, a `revision` file and the card as `card.md`. The default staging root is `~/.local/share/cortexkit/staging`. A stage directory is never rewritten: staging the same commit twice refuses.
7. **Declares it current**, writing `<staging root>/basal.current` and `<staging root>/basal-worker.current` through a temp file and a rename. Each holds `stage=` (the stage directory), `revision=` (the full 40-character sha) and `declared_at=` (UTC). place-module.sh derives the file name from the destination binary's name without `ck-`, so the worker's card reads `basal-worker.current`.
8. **Prints the card**: card, signing, marker and control, store and formats, order, and the post-placement check. It is not posted.

## What SUBC's gate checks, and what a first placement changes

place-module.sh runs one binary per call (`--dest` names the worker's destination), read-only unless given `--place`. Against this card:

| Arm | Result |
|---|---|
| sidecar (`<binary>.sha256` beside the staged file, `shasum -c`) | passes |
| kind (Mach-O, and the same kind as the running file) | the staged files are Mach-O; there is no running file to compare |
| currency (`<staging>/basal.current` and `basal-worker.current`, `stage=` equal to the staged file's directory) | passes |
| signing posture compared with the running binary; hardened runtime may be added, never removed | no running binary to compare. Both carry runtime. |
| `get-task-allow` refused | passes: no entitlements at all |
| designated requirement satisfied by the staged file | no running binary; ad-hoc requirements are cdhashes in any case |
| marker (staged ≥ 1, live 0) and control (both ≥ 1) | staged counts pass; no live binary to read |
| format floors (`check-format-floors.sh basal`) | no floors: none recorded, which passes |

The script resolves `--dest` (default `~/.local/share/cortexkit/bin/ck-<module>`) and refuses before any arm when it does not exist: "destination does not exist, so this is an install rather than a placement". So on the first placement it cannot run at all, not even read-only. With `--no-restart` it would skip its supervisor check (`ck module status basal`, which would also refuse while the daemon has no basal module), but the destination check still stops it. The first placement is therefore an install that SUBC does by hand from the card's sha256, and the gate applies from the second card on.

basal's store is new, so there is no migration and no format on disk: no `format-floor.json` is written, and none is needed for the gate to pass.

## The rig run before a handover

Run the staged bytes on the ckdev-flows rig ([rig.md](rig.md#running-a-stage)):

```sh
STAGING=$(mktemp -d)
script/stage.sh --local-only --staging-root "$STAGING"
script/flows-rig.sh build --prefrontal-rev <rev> ...      # basal at the same HEAD
script/flows-rig.sh place --from-stage "$STAGING/basal-<short sha>"
script/flows-rig.sh config
script/flows-rig.sh test        # every case but the crash case, which is reported as not run
script/flows-rig.sh place       # the rig's own build again, with the kill switch
script/flows-rig.sh test        # the crash case too
rm -rf "$STAGING"
```

The staged `ck-basal` has no kill switch, so the contract suite cannot run its crash case on those bytes. The evidence for a commit is both runs at that commit.

## Rollback

- **First placement:** disable basal in the daemon's config (`"enabled": false` in its `subc.jsonc` entry, or remove the entry) and remove `ck-basal` and `ck-basal-worker` from `~/.local/share/cortexkit/bin`. basal's store, `<data_home>/cortexkit/basal/store.db`, holds only what basal wrote since; keep it for diagnosis or delete it with the binaries.
- **Later placements:** with `--place`, place-module.sh keeps the previous binary as `~/.local/share/cortexkit/staging/<binary>.rollback-<timestamp>` with its sidecar, and SUBC places it back with `--older`. Roll back both binaries together: the module and the worker speak a protocol to each other and are built and tested as a pair.
- **Store:** no card so far changes basal's store schema. A card that does must say so, and SUBC places it with `--migrates <store>`, which snapshots the store first; rolling that card back means restoring the binaries and the snapshot together.
- **Stage:** an unused stage is removed with its directory. If it was declared current, point the `.current` files back at the previous stage (or remove them) so the gate never takes a superseded build.
