#!/bin/sh
# stage.sh: build, sign, smoke-test and stage ck-basal and ck-basal-worker
# for SUBC, who places every module binary with the fleet's
# subconscious/scripts/fleet/place-module.sh. Nothing here places, restarts
# or posts anything: it writes the stage and prints the card to hand over.
# docs/deploy.md describes the whole procedure and the rollback.
#
# Usage:
#   script/stage.sh [--local-only] [--staging-root <dir>]
#                   [--basal-marker <string> --basal-control <string>]
#                   [--worker-marker <string> --worker-control <string>]
#   script/stage.sh --verify <stage dir>
#
# --local-only    skip only the check that HEAD is on origin/main, for rig
#                 testing; the card and the .current files say "not pushed".
#                 A clean tree is still required.
# --staging-root  where the stage directory and the .current files go;
#                 default ~/.local/share/cortexkit/staging. Point it at a
#                 temporary directory for anything but a real stage.
# --basal-marker / --basal-control / --worker-marker / --worker-control
#                 required when ck-basal already exists at the live destination:
#                 each marker must read staged >= 1 / live 0, and each control
#                 must read >= 1 in both files, as the placement gate requires.
# --verify        check an existing stage directory (both binaries' sidecars
#                 and signatures) and print its revision; changes nothing.
#                 script/flows-rig.sh place --from-stage uses it.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
# shellcheck source=script/signing.sh
. "$ROOT/script/signing.sh"

CK_SHARE="$HOME/.local/share/cortexkit"
LIVE_BIN="$CK_SHARE/bin"
# The stage's own cargo target directory: whatever else was built in target/
# (the rig's features, a debug build) never lands in a stage.
TARGET="$ROOT/target/stage"
# The two binaries, each with its production identifier and a control: a
# string that every build of that binary carries, so the next card can show
# the gate's reader actually read the running file.
BINARIES="ck-basal ck-basal-worker"
control_of() {
  case "$1" in
    ck-basal) printf '%s\n' 'flow.install' ;;
    # Not a flag name: a short literal the binary only compares against can
    # be compiled into integer comparisons and never appear in strings.
    ck-basal-worker) printf '%s\n' 'refusing to run unconfined' ;;
  esac
}
# The rig's kill switch reads this variable; only a rig-kill-hook build
# contains it, and a production ck-basal must not.
KILL_HOOK_NEEDLE=BASAL_RIG_KILL_FILE

say() { printf '%s\n' "$*"; }
die() { printf 'stage: %s\n' "$*" >&2; exit 1; }
usage() {
  awk '/^# Usage:/ { on = 1 } /^set -eu/ { exit } on' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

LOCAL_ONLY=0
STAGING="$CK_SHARE/staging"
VERIFY=""
BASAL_MARKER=""
BASAL_CONTROL=""
WORKER_MARKER=""
WORKER_CONTROL=""
parse_options() {
while [ $# -gt 0 ]; do
  case "$1" in
    --local-only) LOCAL_ONLY=1; shift ;;
    --staging-root) [ $# -ge 2 ] || usage; STAGING=$2; shift 2 ;;
    --verify) [ $# -ge 2 ] || usage; VERIFY=$2; shift 2 ;;
    --basal-marker) [ $# -ge 2 ] || usage; BASAL_MARKER=$2; shift 2 ;;
    --basal-control) [ $# -ge 2 ] || usage; BASAL_CONTROL=$2; shift 2 ;;
    --worker-marker) [ $# -ge 2 ] || usage; WORKER_MARKER=$2; shift 2 ;;
    --worker-control) [ $# -ge 2 ] || usage; WORKER_CONTROL=$2; shift 2 ;;
    *) usage ;;
  esac
done
}
parse_options "$@"

# ---------------------------------------------------------------- verify

# verify_stage <dir>: every binary's sidecar passes shasum -c and its
# signature passes the shared policy under its production identifier.
# Prints "revision <sha>" from the stage's own record.
verify_stage() {
  dir=$1
  [ -d "$dir" ] || die "no stage directory at $dir"
  for bin in $BINARIES; do
    [ -f "$dir/$bin" ] || die "$dir has no $bin"
    (cd "$dir" && shasum -c "$bin.sha256" >/dev/null) \
      || die "$dir/$bin.sha256 does not verify its binary"
    verify_hardened "$dir/$bin" "$bin" || die "$dir/$bin fails the signing policy"
  done
  [ -f "$dir/revision" ] || die "$dir has no revision record"
  say "revision $(cat "$dir/revision")"
}

if [ -n "$VERIFY" ]; then
  verify_stage "$VERIFY"
  exit 0
fi

# ---------------------------------------------------------------- tree

for tool in git cargo codesign lldb shasum strings nm python3 perl lsof file; do
  command -v "$tool" >/dev/null || die "$tool is not on PATH"
done

# Never from a dirty tree: untracked files count too, since a stray source
# file or a changed build input would be built without being committed.
status=$(git -C "$ROOT" status --porcelain --untracked-files=all)
if [ -n "$status" ]; then
  printf '%s\n' "$status" >&2
  die "refusing: $ROOT has uncommitted changes; commit or remove them first"
fi
SHA=$(git -C "$ROOT" rev-parse --verify HEAD)
SHORT=$(git -C "$ROOT" rev-parse --short=12 HEAD)
# The fleet's rule, as prefrontal's deploy script computes it: dirty means
# tracked changes. The refusal above makes it false here; it is computed
# anyway so the build is told what was checked, not what was assumed.
if [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
  DIRTY=true
else
  DIRTY=false
fi

if [ "$LOCAL_ONLY" = 1 ]; then
  PUSHED="NOT PUSHED: staged with --local-only, which skips the origin/main check; for rig testing only"
else
  git -C "$ROOT" remote get-url origin >/dev/null 2>&1 \
    || die "refusing: basal has no remote 'origin', so HEAD cannot be on origin/main (--local-only stages for rig testing only)"
  git -C "$ROOT" fetch --quiet origin main || die "cannot fetch origin main"
  git -C "$ROOT" merge-base --is-ancestor HEAD origin/main \
    || die "refusing: HEAD $SHA is not on origin/main; push it first"
  PUSHED="on origin/main ($(git -C "$ROOT" rev-parse origin/main))"
fi
say "commit $SHA ($PUSHED)"

# ---------------------------------------------------------------- staging

mkdir -p "$STAGING"
# Logical form (pwd, not pwd -P): place-module.sh compares the stage named in
# a .current file with `cd "$(dirname <staged>)" && pwd` of the path SUBC
# passes, which is logical too.
STAGING=$(cd "$STAGING" && pwd)
STAGE_DIR="$STAGING/basal-$SHORT"
[ ! -e "$STAGE_DIR" ] || die "refusing: $STAGE_DIR already exists; a stage is never rewritten in place (remove it if it was never handed over)"

SCRATCH=$(mktemp -d "$STAGING/.basal-$SHORT.XXXXXX")
CONTROL_DIR=$(mktemp -d "${TMPDIR:-/tmp}/basal-stage-control.XXXXXX")
HELD=""
cleanup() {
  for pid in $HELD; do
    kill -KILL "$pid" 2>/dev/null || true
  done
  rm -rf "$SCRATCH" "$CONTROL_DIR"
}
trap cleanup EXIT
trap 'exit 1' INT TERM

# ---------------------------------------------------------------- build

say ""
say "=== build (release, CK_BUILD_GIT_SHA=$SHA CK_BUILD_GIT_DIRTY=$DIRTY)"
(cd "$ROOT" && env CARGO_TARGET_DIR="$TARGET" CK_BUILD_GIT_SHA="$SHA" CK_BUILD_GIT_DIRTY="$DIRTY" \
  cargo build --release --locked -p basal-module --bin ck-basal -p basal-worker --bin ck-basal-worker)
[ -z "$(git -C "$ROOT" status --porcelain --untracked-files=all)" ] \
  || die "the build changed the tree; nothing staged"
hook_table=$(strings "$TARGET/release/ck-basal") || die "cannot inspect the built ck-basal; nothing staged"
hook=$(printf '%s\n' "$hook_table" | grep -cF -- "$KILL_HOOK_NEEDLE" || true)
[ "$hook" = 0 ] || die "the built ck-basal contains the rig kill switch ($KILL_HOOK_NEEDLE); nothing staged"
say "ck-basal carries no rig kill switch ($KILL_HOOK_NEEDLE: 0)"
# The placement gate refuses a binary that still carries its debug map (the
# OSO entries pointing at object files on the build machine). Cargo strips
# release builds, but a stripping failure is only a warning, so check.
for bin in $BINARIES; do
  symbols=$(nm -a "$TARGET/release/$bin") || die "cannot inspect $bin's symbols; nothing staged"
  oso=$(printf '%s\n' "$symbols" | grep -c ' OSO ' || true)
  [ "$oso" = 0 ] || die "$bin still carries $oso debug-map entries: stripping failed; nothing staged"
done
say "debug map: 0 entries in each binary"

# ---------------------------------------------------------------- sign

say ""
say "=== sign (ad hoc, hardened runtime, explicit identifier, no entitlements)"
for bin in $BINARIES; do
  cp "$TARGET/release/$bin" "$SCRATCH/$bin"
  chmod 0755 "$SCRATCH/$bin"
  sign_hardened "$SCRATCH/$bin" "$bin" 2>/dev/null
  verify_hardened "$SCRATCH/$bin" "$bin" || die "$bin fails the signing policy; nothing staged"
  say "$bin: Identifier=$(codesign_identifier "$SCRATCH/$bin") flags=$(codesign_flags "$SCRATCH/$bin") entitlements: none"
done

# ---------------------------------------------------------------- dev names

# Fleet rule: only binaries placed in ~/.local/share/cortexkit/bin may run
# under a ck- name, so the smoke checks below run ckdev- names instead. Each is
# a hard link to the signed file (the same bytes and inode; a copy where links
# aren't supported), so the checks still test the exact bytes being staged.
# The staged files themselves keep their production names: SUBC places them.
for bin in $BINARIES; do
  dev="ckdev-${bin#ck-}"
  ln "$SCRATCH/$bin" "$SCRATCH/$dev" 2>/dev/null || cp "$SCRATCH/$bin" "$SCRATCH/$dev"
done

# ---------------------------------------------------------------- smoke

# A file's identity for the smoke run: its inode and its codesign flags.
# Every smoke test runs the signed file itself, never through cargo (which
# relinks the binary and silently reverts it to a linker signature), and the
# run counts only if neither changed.
identity() {
  printf '%s %s\n' "$(stat -f %i "$1")" "$(codesign_flags "$1")"
}

# hold <program> [args]: start the program so that it stays alive as that
# image, and print its pid and then the pid of a reader process. Its stdout is
# a pipe already full to the last byte, so its first write blocks for good.
# That keeps a short-lived command (--version) running long enough to attach
# to, without arguments that make it do anything else. A separate `sleep`
# holds the pipe's read end open: ck-basal-worker closes every inherited
# descriptor above 2 at startup, so a read end handed to the program itself
# would be closed, and the write would fail with EPIPE instead of blocking.
hold() {
  python3 - "$@" <<'PY'
import fcntl, os, subprocess, sys
r, w = os.pipe()
fcntl.fcntl(w, fcntl.F_SETFL, os.O_NONBLOCK)
try:
    while True:
        os.write(w, b"x")
except BlockingIOError:
    pass
fcntl.fcntl(w, fcntl.F_SETFL, 0)
reader = subprocess.Popen(["sleep", "600"], stdin=subprocess.DEVNULL,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                          pass_fds=(r,), start_new_session=True)
child = subprocess.Popen(sys.argv[1:], stdin=subprocess.DEVNULL, stdout=w,
                         stderr=subprocess.DEVNULL, start_new_session=True)
print(child.pid, reader.pid)
PY
}

# The inode of the image a running pid executes, from its txt mapping.
running_inode() {
  lsof -nP -a -p "$1" -d txt -Fin 2>/dev/null | awk -v prog="$2" '
    /^i/ { inode = substr($0, 2) }
    /^n/ { if (substr($0, 2) == prog) { print inode; exit } }'
}

# attach <pid>: lldb's whole output for an attach and detach, killed after
# 120 seconds if it hangs.
attach() {
  perl -e 'alarm 120; exec @ARGV' lldb --batch -p "$1" -o 'process detach' 2>&1 || true
}

# debugger_probe <file> <args...>: hold the file running, attach to it, and
# set VERDICT to "refused" or "attached". Fails if the held process is not
# running that file's image, so a verdict is always about the file itself.
# It runs in this shell, not a subshell, so the cleanup trap sees HELD.
debugger_probe() {
  file=$1
  shift
  pids=$(hold "$file" "$@")
  HELD="$HELD $pids"
  pid=${pids%% *}
  sleep 1
  kill -0 "$pid" 2>/dev/null || die "$file exited before the debugger probe"
  # The physical path: lsof names the image by its resolved path.
  physical_file="$(cd -P "$(dirname "$file")" && pwd -P)/$(basename "$file")"
  [ "$(running_inode "$pid" "$physical_file")" = "$(stat -f %i "$file")" ] \
    || die "pid $pid is not running $file"
  out=$(attach "$pid")
  # shellcheck disable=SC2086 # two pids, split on purpose
  kill -KILL $pids 2>/dev/null || true
  if printf '%s\n' "$out" | grep -q "Process $pid detached"; then
    VERDICT=attached
  elif printf '%s\n' "$out" | grep -q 'attach failed.*Not allowed to attach'; then
    VERDICT=refused
  else
    printf '%s\n' "$out" >&2
    die "lldb neither attached to nor was refused by pid $pid"
  fi
}

say ""
say "=== smoke tests (on the signed files)"
for bin in $BINARIES; do
  cmp -s "$SCRATCH/$bin" "$SCRATCH/ckdev-${bin#ck-}" || die "$bin smoke copy differs from the signed file"
  identity "$SCRATCH/ckdev-${bin#ck-}" > "$CONTROL_DIR/$bin.before"
done

manifest=$("$SCRATCH/ckdev-basal" --manifest) || die "ck-basal --manifest failed"
printf '%s' "$manifest" | python3 -c '
import json, sys
m = json.load(sys.stdin)
sha = sys.argv[1]
assert m["module_id"] == "basal", m["module_id"]
got = (m.get("provenance") or {}).get("build_git_sha")
assert got == sha, "provenance build_git_sha is %r, not %s" % (got, sha)
print("ck-basal --manifest: module_id basal, version %s, provenance build_git_sha %s"
      % (m["module_version"], got))
' "$SHA" || die "ck-basal --manifest does not declare this build"
for bin in $BINARIES; do
  version=$("$SCRATCH/ckdev-${bin#ck-}" --version) || die "$bin --version failed"
  # "<name> <crate version> (<sha>)": a dirty build would add ", dirty".
  case "$version" in
    "$bin "*" ($SHA)") ;;
    *) die "$bin --version says '$version', not a clean build of $SHA" ;;
  esac
  say "$bin --version: $version"
done

say "worker sandbox self-check (script/sign-worker.sh verify, as flows-rig.sh place runs it):"
BASAL_WORKER_IDENTIFIER=ck-basal-worker sh "$ROOT/script/sign-worker.sh" verify "$SCRATCH/ckdev-basal-worker" \
  || die "the worker failed its confinement gate; nothing staged"

# The control first: an unhardened ad-hoc copy of /bin/sleep must attach,
# or a refusal below would prove nothing about the hardening.
cp /bin/sleep "$CONTROL_DIR/sleep"
codesign --force --sign - "$CONTROL_DIR/sleep" 2>/dev/null
control_flags=$(codesign_flags "$CONTROL_DIR/sleep")
case "$control_flags" in
  *runtime*) die "the control copy of /bin/sleep is hardened ($control_flags)" ;;
esac
debugger_probe "$CONTROL_DIR/sleep" 600
[ "$VERDICT" = attached ] \
  || die "control: lldb could not attach to an unhardened ad-hoc /bin/sleep, so a refusal would prove nothing"
say "debugger control: lldb -p attached to an unhardened ad-hoc copy of /bin/sleep (flags=$control_flags)"
for bin in $BINARIES; do
  debugger_probe "$SCRATCH/ckdev-${bin#ck-}" --version
  [ "$VERDICT" = refused ] || die "$bin: lldb -p ATTACHED to the signed binary; nothing staged"
  say "debugger: lldb -p refused on $bin (Not allowed to attach to process)"
done

for bin in $BINARIES; do
  before=$(cat "$CONTROL_DIR/$bin.before")
  after=$(identity "$SCRATCH/ckdev-${bin#ck-}")
  cmp -s "$SCRATCH/$bin" "$SCRATCH/ckdev-${bin#ck-}" || die "$bin smoke copy differs from the staged bytes"
  [ "$before" = "$after" ] || die "$bin changed during the smoke run (inode flags: $before -> $after); the run does not count"
  say "$bin: inode and flags unchanged across the smoke run ($after)"
done

# ---------------------------------------------------------------- marker

# count <file> <needle>: as place-module.sh counts in its strings table.
count() {
  # Check the reader separately: a failed strings invocation is not a zero count.
  table=$(strings "$1") || die "cannot read the strings table of $1"
  printf '%s\n' "$table" | grep -cF -- "$2" || true
}
# counts <bin> <needle>: "staged N / live M", where live is the running
# binary in ~/.local/share/cortexkit/bin, which a first placement has none of.
counts() {
  if [ -f "$LIVE_BIN/$1" ]; then
    live=$(count "$LIVE_BIN/$1" "$2")
  else
    live="none (no running $1)"
  fi
  printf 'staged %s / live %s\n' "$(count "$CARD_BIN_DIR/$1" "$2")" "$live"
}
marker_of() {
  case "$1" in
    (ck-basal) printf '%s\n' "$BASAL_MARKER" ;;
    (ck-basal-worker) printf '%s\n' "$WORKER_MARKER" ;;
  esac
}
update_control_of() {
  case "$1" in
    (ck-basal) printf '%s\n' "$BASAL_CONTROL" ;;
    (ck-basal-worker) printf '%s\n' "$WORKER_CONTROL" ;;
  esac
}

# Read the committed build input, not a possibly changed working file. SQLite's
# read-only URI sees committed WAL data without opening the running store for writes.
store_versions() {
  schema=$(git -C "$ROOT" show HEAD:crates/basal-core/src/schema.rs) \
    || die "cannot read the schema at HEAD"
  versions=$(printf '%s\n' "$schema" | python3 -c '
import pathlib, re, sqlite3, sys
from contextlib import closing
versions = [int(v) for v in re.findall(r"^\s*version:\s*(\d+)\s*,", sys.stdin.read(), re.M)]
if not versions:
    sys.exit("no migration versions in the schema at HEAD")
with closing(sqlite3.connect(pathlib.Path(sys.argv[1]).resolve().as_uri() + "?mode=ro", uri=True)) as db:
    live = db.execute("SELECT max(version) FROM cortexkit_schema_version").fetchone()[0]
if not isinstance(live, int) or live < 0:
    sys.exit("the live store has no valid schema version")
print(max(versions), live)
' "$LIVE_STORE") || die "cannot compare the build and live store schema versions"
  BUILD_SCHEMA=${versions%% *}
  LIVE_SCHEMA=${versions#* }
  MIGRATES_ARG=""
  if [ "$BUILD_SCHEMA" -gt "$LIVE_SCHEMA" ]; then
    MIGRATES_ARG=" --migrates ~/.local/share/cortexkit/basal/store.db"
  fi
}

# Validate before publishing a stage or opening card.md, so a failed gate cannot
# leave a declaration that looks ready for placement.
prepare_card() {
CARD_BIN_DIR=$1
LIVE_STORE="$CK_SHARE/basal/store.db"
UPDATE=0
if [ -e "$LIVE_BIN/ck-basal" ]; then
  UPDATE=1
  [ -n "$BASAL_MARKER" ] && [ -n "$BASAL_CONTROL" ] \
    && [ -n "$WORKER_MARKER" ] && [ -n "$WORKER_CONTROL" ] \
    || die "an update requires --basal-marker, --basal-control, --worker-marker and --worker-control"
fi
for bin in $BINARIES; do
  [ "$(count "$CARD_BIN_DIR/$bin" "$SHA")" -ge 1 ] || die "$bin does not embed $SHA; nothing staged"
  marker=$SHA
  control=$(control_of "$bin")
  if [ "$UPDATE" = 1 ]; then
    marker=$(marker_of "$bin")
    control=$(update_control_of "$bin")
    [ -f "$LIVE_BIN/$bin" ] || die "an update needs the live destination $LIVE_BIN/$bin"
    staged_marker=$(count "$CARD_BIN_DIR/$bin" "$marker") || exit 1
    live_marker=$(count "$LIVE_BIN/$bin" "$marker") || exit 1
    [ "$staged_marker" -ge 1 ] && [ "$live_marker" -eq 0 ] \
      || die "$bin marker '$marker': staged $staged_marker / live $live_marker; need staged >= 1 / live 0; no card written"
    staged_control=$(count "$CARD_BIN_DIR/$bin" "$control") || exit 1
    live_control=$(count "$LIVE_BIN/$bin" "$control") || exit 1
    [ "$staged_control" -ge 1 ] && [ "$live_control" -ge 1 ] \
      || die "$bin control '$control': staged $staged_control / live $live_control; need both >= 1; no card written"
  fi
  [ "$(count "$CARD_BIN_DIR/$bin" "$control")" -ge 1 ] \
    || die "$bin does not contain its control '$control'; nothing staged"
  counts "$bin" "$marker" > "$CONTROL_DIR/$bin.marker"
  counts "$bin" "$control" > "$CONTROL_DIR/$bin.control"
  say "$bin marker \"$marker\": $(cat "$CONTROL_DIR/$bin.marker")"
  say "$bin control \"$control\": $(cat "$CONTROL_DIR/$bin.control")"
done
if [ "$UPDATE" = 1 ]; then
  store_versions
fi
}

# ---------------------------------------------------------------- prepare card
say ""
say "=== marker and control"
prepare_card "$SCRATCH"

# ---------------------------------------------------------------- stage

say ""
say "=== stage"
for bin in $BINARIES; do
  (cd "$SCRATCH" && shasum -a 256 "$bin" > "$bin.sha256" && shasum -c "$bin.sha256")
done
printf '%s\n' "$SHA" > "$SCRATCH/revision"
chmod 0755 "$SCRATCH"
# A rename in the same directory: the signed files keep their inodes.
mv "$SCRATCH" "$STAGE_DIR"
verify_stage "$STAGE_DIR" >/dev/null
DECLARED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# write_current <name>: <staging>/<name>.current through a temp file and a
# rename, so a reader never sees half a declaration. The name is the binary's
# without its ck- prefix, which is how place-module.sh finds it.
write_current() {
  tmp=$(mktemp "$STAGING/.$1.current.XXXXXX")
  {
    printf 'stage=%s\n' "$STAGE_DIR"
    printf 'revision=%s\n' "$SHA"
    printf 'declared_at=%s\n' "$DECLARED_AT"
    if [ "$LOCAL_ONLY" = 1 ]; then
      printf 'pushed=no (--local-only)\n'
    fi
  } > "$tmp"
  chmod 0644 "$tmp"
  mv -f "$tmp" "$STAGING/$1.current"
  say "wrote $STAGING/$1.current:"
  sed 's/^/  /' "$STAGING/$1.current"
}

# ---------------------------------------------------------------- card

sum_of() { awk '{ print $1 }' "$STAGE_DIR/$1.sha256"; }
# The CI result for the staged commit, as GitHub reports it, so the card
# never claims a run it did not look up.
ci_of() {
  command -v gh >/dev/null 2>&1 || { printf 'not checked (gh is not installed)'; return; }
  gh run list --repo cortexkit/basal --workflow CI --commit "$SHA" --event push --limit 1 \
      --json status,conclusion,url \
      --jq '.[0] | if . == null then "no run for this commit" else "\(.status) \(.conclusion // ""), \(.url)" end' \
      2>/dev/null || printf 'not checked (gh failed)'
}
write_first_install_card() {
cat > "$CARD" <<EOF
@SUBC basal: a FIRST placement, two binaries, one card each (ck-basal, ck-basal-worker)

**Card**
- stage: $STAGE_DIR (both binaries)
- ck-basal: $STAGE_DIR/ck-basal, sha256 $(sum_of ck-basal); basal.current revision $SHA
- ck-basal-worker: $STAGE_DIR/ck-basal-worker, sha256 $(sum_of ck-basal-worker); basal-worker.current revision $SHA
- build: release, clean tree, CK_BUILD_GIT_SHA=$SHA CK_BUILD_GIT_DIRTY=$DIRTY, commit $PUSHED.
- CI for that commit: $(ci_of)
- daemon floor: 0.20.53, the oldest daemon a subc-protocol 0.30 module is known to run against.

**Signing** (both): ad hoc, hardened runtime, explicit identifier, no entitlements, no get-task-allow, 0 debug-map entries
- ck-basal: Identifier=ck-basal flags=$(codesign_flags "$STAGE_DIR/ck-basal")
- ck-basal-worker: Identifier=ck-basal-worker flags=$(codesign_flags "$STAGE_DIR/ck-basal-worker")
- no entitlements: the worker's QuickJS is an interpreter and needs no JIT; its sandbox is a Seatbelt profile it applies to itself.
- smoke-tested on the signed files: ck-basal --manifest (module_id basal, provenance build_git_sha $SHA); --version on both; the worker's sandbox self-check (script/sign-worker.sh verify: embedded profile, and a file read, a socket connect and a program launch all denied); lldb -p refused on both, with an unhardened ad-hoc copy of /bin/sleep as the control that did attach. Inode and flags unchanged across the run.

**Marker and control** (strings table)
- marker: $SHA (the full commit, embedded by the build)
  - ck-basal: $(cat "$CONTROL_DIR/ck-basal.marker")
  - ck-basal-worker: $(cat "$CONTROL_DIR/ck-basal-worker.marker")
- control: a string every build of each binary carries
  - ck-basal "$(control_of ck-basal)": $(cat "$CONTROL_DIR/ck-basal.control")
  - ck-basal-worker "$(control_of ck-basal-worker)": $(cat "$CONTROL_DIR/ck-basal-worker.control")
- This is a first install: there is no running basal, so place each binary with place-module.sh's \`--install\` mode and the marker above (\`--module basal --staged $STAGE_DIR/ck-basal --marker $SHA --install\`, then the same for ck-basal-worker with \`--dest ~/.local/share/cortexkit/bin/ck-basal-worker\`). It checks that the destination does not exist, that the binary is hardened with no get-task-allow, that its identifier matches the destination name, and that the marker is in the staged binary. No restart: basal starts on \`ck module rescan\` once its config entry exists.

**Store and formats**
- new module: no migration, no existing format, no format-floor.json. basal's store, <data_home>/cortexkit/basal/store.db, is created on first start. check-format-floors.sh finds no floors for basal and passes.

**Order**
- both binaries in ~/.local/share/cortexkit/bin before basal first starts: ck-basal runs ck-basal-worker from its own directory under that exact name.
- the daemon has no basal module yet: its subc.jsonc entry is the operator's approval step and comes after both files are in place. The entry: no environment (basal finds its store from its home), \`reserved: true\` (core accepts basal's install and decision cards only from reserved:basal), and exclusive overlap (the journal is a single-writer database).

**Post-placement check** (mine)
- \`ck --json provenance basal\`: build_git_sha $SHA, and the observed pid's running image is the placed ck-basal (same inode).
- a worker's running image is the placed ck-basal-worker (same inode) once a flow runs.
- BASAL_WORKER_IDENTIFIER=ck-basal-worker sh script/sign-worker.sh verify ~/.local/share/cortexkit/bin/ck-basal-worker passes at the placed path.
- basal's flow.list answers (no flows yet).
EOF
}

write_update_card() {
cat > "$CARD" <<EOF
@SUBC basal: an UPDATE, two binaries, worker first and ck-basal restarted last

**Card**
- stage: $STAGE_DIR (both binaries)
- ck-basal: $STAGE_DIR/ck-basal, sha256 $(sum_of ck-basal); basal.current revision $SHA
- ck-basal-worker: $STAGE_DIR/ck-basal-worker, sha256 $(sum_of ck-basal-worker); basal-worker.current revision $SHA
- build: release, clean tree, CK_BUILD_GIT_SHA=$SHA CK_BUILD_GIT_DIRTY=$DIRTY, commit $PUSHED.
- CI for that commit (push event): $(ci_of)
- daemon floor: 0.20.53, the oldest daemon a subc-protocol 0.30 module is known to run against.

**Signing** (both): ad hoc, hardened runtime, explicit identifier, no entitlements, no get-task-allow, 0 debug-map entries
- ck-basal: Identifier=ck-basal flags=$(codesign_flags "$STAGE_DIR/ck-basal")
- ck-basal-worker: Identifier=ck-basal-worker flags=$(codesign_flags "$STAGE_DIR/ck-basal-worker")
- no entitlements: the worker's QuickJS is an interpreter and needs no JIT; its sandbox is a Seatbelt profile it applies to itself.
- smoke-tested on the signed files: ck-basal --manifest (module_id basal, provenance build_git_sha $SHA); --version on both; the worker's sandbox self-check (script/sign-worker.sh verify: embedded profile, and a file read, a socket connect and a program launch all denied); lldb -p refused on both, with an unhardened ad-hoc copy of /bin/sleep as the control that did attach. Inode and flags unchanged across the run.

**Marker and control** (strings table, checked against the live destinations)
- ck-basal marker "$BASAL_MARKER": $(cat "$CONTROL_DIR/ck-basal.marker")
- ck-basal control "$BASAL_CONTROL": $(cat "$CONTROL_DIR/ck-basal.control")
- ck-basal-worker marker "$WORKER_MARKER": $(cat "$CONTROL_DIR/ck-basal-worker.marker")
- ck-basal-worker control "$WORKER_CONTROL": $(cat "$CONTROL_DIR/ck-basal-worker.control")

**Store and rollback**
- $LIVE_STORE: build schema $BUILD_SCHEMA / live schema $LIVE_SCHEMA (SELECT max(version) FROM cortexkit_schema_version, mode=ro).
EOF
if [ -n "$MIGRATES_ARG" ]; then
  cat >> "$CARD" <<EOF
- This update migrates the store. The ck-basal command below includes \`--migrates ~/.local/share/cortexkit/basal/store.db\` so place-module.sh snapshots it before placement.
- Rollback is the old binary AND the store backup together, with both binaries restored as a pair. A binary-only rollback refuses to open the newer store: basal_core::Store::open returns "the store's schema is at version N but this build knows only up to M". Stop basal before restoring the store backup; do not let it reopen the store until both old binaries and the backup are restored.
EOF
else
  say "- No schema rise: no store migration or store backup is requested. Roll back both binaries together with place-module.sh's --older mode." >> "$CARD"
fi
cat >> "$CARD" <<EOF

**Order and restart**
1. Worker first, without restarting the parent:
   \`place-module.sh --module basal --staged $(shell_quote "$STAGE_DIR/ck-basal-worker") --marker $(shell_quote "$WORKER_MARKER") --control $(shell_quote "$WORKER_CONTROL") --dest ~/.local/share/cortexkit/bin/ck-basal-worker --no-restart\`
2. Then ck-basal, with the default update restart:
   \`place-module.sh --module basal --staged $(shell_quote "$STAGE_DIR/ck-basal") --marker $(shell_quote "$BASAL_MARKER") --control $(shell_quote "$BASAL_CONTROL")$MIGRATES_ARG\`
- These commands are read-only gate runs; SUBC adds \`--place\` for placement. Warm workers are spawned by ck-basal, so restarting ck-basal last makes every worker come from the new file. Between the two placements the old parent may spawn a new worker, so this order requires that the worker protocol change is additive: the old parent must decode every message the new worker sends. Check that before placing; a change that is not additive needs basal stopped while both binaries are replaced.

**Post-placement check** (mine)
- \`ck --json provenance basal\`: build_git_sha $SHA, and the observed pid's running image is the placed ck-basal (same inode).
- a worker's running image is the placed ck-basal-worker (same inode) once a flow runs.
- BASAL_WORKER_IDENTIFIER=ck-basal-worker sh script/sign-worker.sh verify ~/.local/share/cortexkit/bin/ck-basal-worker passes at the placed path.
- the store reads schema version $BUILD_SCHEMA with its tables intact.
- basal's flow.list answers.
EOF
}

shell_quote() {
  # Needles are literal strings, not shell fragments. Preserve quotes and spaces
  # when SUBC copies the gate command from the card.
  printf "'"
  printf '%s' "$1" | sed "s/'/'\\\\''/g"
  printf "'"
}

write_card() {
  # Run validation here too: callers that only generate a card get the same
  # refusal as staging, before card.md can be created or truncated.
   if [ "${1:-}" != --prepared ]; then
     prepare_card "$STAGE_DIR"
   fi
  CARD="$STAGE_DIR/card.md"
  if [ "$UPDATE" = 1 ]; then
    write_update_card
  else
    write_first_install_card
  fi
}

# ---------------------------------------------------------------- output
write_card --prepared
write_current basal
write_current basal-worker
say ""
say "=== card (saved to $CARD; not posted)"
cat "$CARD"
say ""
say "staged $STAGE_DIR"
