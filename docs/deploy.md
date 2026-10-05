# Deploying basal

basal never places its own binaries. In this fleet the operator places every module binary with `subconscious/scripts/fleet/place-module.sh`, from a card the module stages. basal ships two binaries, `ck-basal` (the module) and `ck-basal-worker` (the confined QuickJS worker it runs flows in). [`script/stage.sh`](../script/stage.sh) builds, signs, smoke-tests and stages both and prints one card covering the two gate invocations. Posting it, and everything after, is the operator's step.

## Staging

```sh
script/stage.sh                                   # first install, from a pushed commit
# An update needs a marker/control pair for each binary. These example needles
# distinguish the flow-scope/host-rejection build from its predecessor:
script/stage.sh --basal-marker flow_scope_required --basal-control operator_attestation_required \
  --worker-marker hostRejection --worker-control rquickjs
script/stage.sh --local-only --staging-root "$(mktemp -d)"   # rig testing only
script/stage.sh --verify <stage dir>              # re-check an existing stage
```

In order, `stage.sh`:

1. **Refuses a dirty tree**, untracked files included, and refuses unless basal has a remote `origin` and `HEAD` is on `origin/main` (it fetches `origin main` first). `--local-only` skips only the `origin/main` check, for staging an unpushed commit onto the rig; the card, and a `pushed=no` line in each `.current` file, then say it was not pushed. Such a stage is for testing, never for handing over.
2. **Builds** `ck-basal` and `ck-basal-worker` in release mode, `--locked`, into `target/stage/`, with `CK_BUILD_GIT_SHA=<HEAD>` and `CK_BUILD_GIT_DIRTY` set as prefrontal's deploy script sets them. Each crate's `build.rs` embeds the commit: `ck-basal` declares it as the manifest's build provenance (`build_git_sha`), and both binaries print it from `--version`. It refuses if the build changed the tree, or if `ck-basal` contains the rig's kill switch (the string `BASAL_RIG_KILL_FILE`, present only in a `rig-kill-hook` build).
3. **Signs** a copy of each, by the policy in [`script/signing.sh`](../script/signing.sh): `codesign --force --sign - --options runtime --identifier ck-basal` (and `ck-basal-worker`). Ad hoc, hardened runtime, a plain identifier, no entitlements: the worker's QuickJS is an interpreter and needs no JIT, and its sandbox is a Seatbelt profile it applies to itself. Never `get-task-allow`. It then refuses unless the signature verifies strictly, the identifier is exact, the `flags=` line carries `runtime`, and there are no entitlements.
4. **Smoke-tests the signed files** directly, never through cargo, which relinks a binary and silently reverts it to a linker signature:
   - `ck-basal --manifest` must name module `basal` and declare the commit as `build_git_sha`; `--version` on both must name the commit, clean;
   - the worker's sandbox self-check, `script/sign-worker.sh verify` with `BASAL_WORKER_IDENTIFIER=ck-basal-worker`, as `flows-rig.sh place` runs it: the checked-in Seatbelt profile is embedded, and a file read, a socket connect and a program launch are all denied;
   - debugger hardening: an unhardened ad-hoc copy of `/bin/sleep` is held running and `lldb -p` must attach to it (the control, without which a refusal proves nothing), then each signed binary is held running (`--version`, with its stdout a full pipe so it blocks) and `lldb -p` must be refused. Each held process is checked to be running that file's image, by inode, before the attach.
   Each file's inode and codesign flags are recorded before and after; if either changed, it fails.
5. **Chooses first install or update** from whether `~/.local/share/cortexkit/bin/ck-basal` exists, and **checks the marker and control** before publishing a stage or writing a card. Both binaries must still embed the full commit sha. For a first install, the card is unchanged: the full sha is the marker, with controls `flow.install` and `refusing to run unconfined`. For an update, `--basal-marker`, `--basal-control`, `--worker-marker` and `--worker-control` are required, and both live destinations must be readable. Each marker must read at least 1 staged and 0 live; each control must read at least 1 in both. The card prints the actual `strings` counts, not expected counts. Choose needles against the outgoing build; the example above is not a permanent default.
   For updates it also compares the highest migration `version:` in `crates/basal-core/src/schema.rs` at `HEAD` with `SELECT max(version) FROM cortexkit_schema_version` in the live store. The store is opened with a SQLite `mode=ro` URI, never read-write; a missing or unreadable store refuses the card. A schema rise adds `--migrates ~/.local/share/cortexkit/basal/store.db` to the ck-basal command, not the worker command.
6. **Stages** both under `<staging root>/basal-<short sha>/`, with a `.sha256` sidecar each that passes `shasum -c`, and a `revision` file. The default staging root is `~/.local/share/cortexkit/staging`. A stage directory is never rewritten: staging the same commit twice refuses.
7. **Declares it current**, writing `<staging root>/basal.current` and `<staging root>/basal-worker.current` through a temp file and a rename. Each holds `stage=` (the stage directory), `revision=` (the full 40-character sha) and `declared_at=` (UTC), plus `pushed=no (--local-only)` for a local-only stage. place-module.sh derives the file name from the destination binary's name without `ck-`, so the worker's card reads `basal-worker.current`.
8. **Prints the card** and saves it in the stage directory as `card.md`: card, signing, marker and control, store/rollback, order, and the post-placement check. Its CI line looks up the push-triggered CI run for the exact commit and names its conclusion and URL, not a newer scheduled breadth audit. It is not posted.

## What the placement gate checks, and what a first placement changes

place-module.sh runs one binary per call (`--dest` names the worker's destination), read-only unless given `--place`. Against this card:

| Arm | Result |
|---|---|
| sidecar (`<binary>.sha256` beside the staged file, `shasum -c`) | passes |
| kind (Mach-O, and the same kind as the running file) | the staged files are Mach-O; there is no running file to compare |
| currency (`<staging>/basal.current` and `basal-worker.current`, `stage=` equal to the staged file's directory) | passes |
| signing posture compared with the running binary; hardened runtime may be added, and is removed only with `--allow-unhardened` | no running binary to compare. Both carry runtime. |
| `get-task-allow` refused | passes: no entitlements at all |
| designated requirement satisfied by the staged file | no running binary; ad-hoc requirements are cdhashes in any case |
| marker (staged ≥ 1, live 0) and control (both ≥ 1) | staged counts pass; no live binary to read |
| format floors (`check-format-floors.sh basal`) | no floors: none recorded, which passes |

The first-install card uses `--install` for each binary, with `--dest ~/.local/share/cortexkit/bin/ck-basal-worker` for the worker. Install mode requires that the destination not exist and checks the staged marker, hardened signature, identifier and sidecar without comparing to a live image. It never restarts: both files must be in place before the operator approves basal's reserved, exclusive config entry and runs `ck module rescan`. Without `--install`, a missing destination refuses as an install rather than an update. The gate is read-only by default; the operator adds `--place` to perform either kind of placement.

basal's store is new, so there is no migration and no format on disk: no `format-floor.json` is written, and none is needed for the gate to pass.

## The update card

An update names a discriminator and shared control for each staged/live pair, their measured counts, the live and build schema versions, and the rollback. Run the two commands in the printed order:

1. **ck-basal-worker first**, `--module basal --dest ~/.local/share/cortexkit/bin/ck-basal-worker --no-restart`, with the worker's staged path, marker and control.
2. **ck-basal last**, `--module basal`, with its staged path, marker and control, and `--migrates ~/.local/share/cortexkit/basal/store.db` only when the schema version rises. Leave the default update restart enabled.

Warm workers are spawned by ck-basal. Restarting ck-basal last ensures every worker comes from the newly placed worker file, not a warm outgoing image. Between the two placements, the old parent spawning a new worker is safe because the worker protocol change is additive. The commands on the card are gate-only; the operator adds `--place` to each after checking them.

After placement, basal checks that `ck --json provenance basal` names the commit, the parent's and a worker's running images match their placed files by inode, the sandbox verifies at the placed worker path, the store reads the build's new schema version with its tables intact, and `flow.list` answers.

## The rig run before a handover

Before staging, run the formatting, both clippy configurations, workspace tests and shell checks listed in the [README](../README.md), then validate and replay the safety catalogue with the pinned `ck-mutate` runner:

```sh
cargo install --locked --git https://github.com/cortexkit/commons --rev 0b1004452fb2b673fdb2bc010a850b430e3e945f cortexkit-mutate
mkdir -p target/mutations
ck-mutate check
ck-mutate run --all --report target/mutations/handover.json
```

Every row must be `CAUGHT`; a build failure or a red test other than the named guard is not proof of that guard. Keep the JSON report with the handover evidence rather than committing machine output. New safety guards belong in `mutations.toml`: resolve the full libtest path, use `ck-mutate prove` to append only a caught edit, and commit the guard with its proof as described in the README. These source-level proofs complement, rather than replace, the signed-binary and live rig checks below.

Run the staged bytes on the ckdev-flows rig, basal's isolated test stack ([`script/flows-rig.sh`](../script/flows-rig.sh) documents it in its header):

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
- **Later placements:** with `--place`, place-module.sh keeps the previous binary as `~/.local/share/cortexkit/staging/<binary>.rollback-<timestamp>` with its sidecar (the newest three per binary are kept), and the operator places it back with `--older`. `--older` only waives the currency arm: the rollback still passes every other arm, so its marker and control are chosen against the binary being rolled back. Roll back both binaries together: the module and the worker speak a protocol to each other and are built and tested as a pair.
- **Migrating update:** the ck-basal invocation includes `--migrates <store>`, which makes place-module.sh snapshot the live store before changing the binary. Rollback is the **old binary AND the store backup together**, with both binaries restored as a pair. A binary-only rollback fails closed: `basal_core::Store::open` returns "the store's schema is at version N but this build knows only up to M". Stop basal before restoring the store backup, restore both old binaries and the matching store snapshot, then restart. Never restore a store underneath the running module. If the schema did not rise, the card requests no migration snapshot.
- **Stage:** an unused stage is removed with its directory. If it was declared current, point the `.current` files back at the previous stage (or remove them) so the gate never takes a superseded build.
