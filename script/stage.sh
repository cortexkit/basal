#!/bin/sh
# stage.sh: build, sign, smoke-test and stage ck-basal and ck-basal-worker
# for SUBC, who places every module binary with the fleet's
# subconscious/scripts/fleet/place-module.sh. Nothing here places, restarts
# or posts anything: it writes the stage and prints the card to hand over.
# docs/deploy.md describes the whole procedure and the rollback.
#
# Usage:
#   script/stage.sh [--local-only] [--staging-root <dir>]
#   script/stage.sh --verify <stage dir>
#
# --local-only    skip only the check that HEAD is on origin/main, for rig
#                 testing; the card and the .current files say "not pushed".
#                 A clean tree is still required.
# --staging-root  where the stage directory and the .current files go;
#                 default ~/.local/share/cortexkit/staging. Point it at a
#                 temporary directory for anything but a real stage.
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
    ck-basal-worker) printf '%s\n' '--confinement-probe' ;;
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
while [ $# -gt 0 ]; do
  case "$1" in
    --local-only) LOCAL_ONLY=1; shift ;;
    --staging-root) [ $# -ge 2 ] || usage; STAGING=$2; shift 2 ;;
    --verify) [ $# -ge 2 ] || usage; VERIFY=$2; shift 2 ;;
    *) usage ;;
  esac
done

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

for tool in git cargo codesign lldb shasum strings python3 perl lsof file; do
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
hook=$(strings "$TARGET/release/ck-basal" | grep -cF -- "$KILL_HOOK_NEEDLE" || true)
[ "$hook" = 0 ] || die "the built ck-basal contains the rig kill switch ($KILL_HOOK_NEEDLE); nothing staged"
say "ck-basal carries no rig kill switch ($KILL_HOOK_NEEDLE: 0)"

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

# ---------------------------------------------------------------- smoke

# A file's identity for the smoke run: its inode and its codesign flags.
# Every smoke test runs the signed file itself, never through cargo (which
# relinks the binary and silently reverts it to a linker signature), and the
# run counts only if neither changed.
identity() {
  printf '%s %s\n' "$(stat -f %i "$1")" "$(codesign_flags "$1")"
}

# hold <program> [args]: start the program so that it stays alive as that
# image, and print its pid. Its stdout is a pipe already full to the last
# byte, with the read end held open by the child itself, so its first write
# blocks for good. That keeps a short-lived command (--version) running long
# enough to attach to, without arguments that make it do anything else.
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
child = subprocess.Popen(sys.argv[1:], stdin=subprocess.DEVNULL, stdout=w,
                         stderr=subprocess.DEVNULL, pass_fds=(r,),
                         start_new_session=True)
print(child.pid)
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
  pid=$(hold "$file" "$@")
  HELD="$HELD $pid"
  sleep 1
  kill -0 "$pid" 2>/dev/null || die "$file exited before the debugger probe"
  # The physical path: lsof names the image by its resolved path.
  physical_file="$(cd -P "$(dirname "$file")" && pwd -P)/$(basename "$file")"
  [ "$(running_inode "$pid" "$physical_file")" = "$(stat -f %i "$file")" ] \
    || die "pid $pid is not running $file"
  out=$(attach "$pid")
  kill -KILL "$pid" 2>/dev/null || true
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
  identity "$SCRATCH/$bin" > "$CONTROL_DIR/$bin.before"
done

manifest=$("$SCRATCH/ck-basal" --manifest) || die "ck-basal --manifest failed"
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
  version=$("$SCRATCH/$bin" --version) || die "$bin --version failed"
  # "<name> <crate version> (<sha>)": a dirty build would add ", dirty".
  case "$version" in
    "$bin "*" ($SHA)") ;;
    *) die "$bin --version says '$version', not a clean build of $SHA" ;;
  esac
  say "$bin --version: $version"
done

say "worker sandbox self-check (script/sign-worker.sh verify, as flows-rig.sh place runs it):"
BASAL_WORKER_IDENTIFIER=ck-basal-worker sh "$ROOT/script/sign-worker.sh" verify "$SCRATCH/ck-basal-worker" \
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
  debugger_probe "$SCRATCH/$bin" --version
  [ "$VERDICT" = refused ] || die "$bin: lldb -p ATTACHED to the signed binary; nothing staged"
  say "debugger: lldb -p refused on $bin (Not allowed to attach to process)"
done

for bin in $BINARIES; do
  before=$(cat "$CONTROL_DIR/$bin.before")
  after=$(identity "$SCRATCH/$bin")
  [ "$before" = "$after" ] || die "$bin changed during the smoke run (inode flags: $before -> $after); the run does not count"
  say "$bin: inode and flags unchanged across the smoke run ($after)"
done

# ---------------------------------------------------------------- marker

say ""
say "=== marker and control"
# count <file> <needle>: as place-module.sh counts in its strings table.
count() {
  strings "$1" | grep -cF -- "$2" || true
}
# counts <bin> <needle>: "staged N / live M", where live is the running
# binary in ~/.local/share/cortexkit/bin, which a first placement has none of.
counts() {
  if [ -f "$LIVE_BIN/$1" ]; then
    live=$(count "$LIVE_BIN/$1" "$2")
  else
    live="none (no running $1)"
  fi
  printf 'staged %s / live %s\n' "$(count "$SCRATCH/$1" "$2")" "$live"
}
for bin in $BINARIES; do
  control=$(control_of "$bin")
  [ "$(count "$SCRATCH/$bin" "$SHA")" -ge 1 ] || die "$bin does not embed $SHA; nothing staged"
  [ "$(count "$SCRATCH/$bin" "$control")" -ge 1 ] \
    || die "$bin does not contain its control '$control'; nothing staged"
  counts "$bin" "$SHA" > "$CONTROL_DIR/$bin.marker"
  counts "$bin" "$control" > "$CONTROL_DIR/$bin.control"
  say "$bin marker \"$SHA\": $(cat "$CONTROL_DIR/$bin.marker")"
  say "$bin control \"$control\": $(cat "$CONTROL_DIR/$bin.control")"
done

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
write_current basal
write_current basal-worker

# ---------------------------------------------------------------- card

sum_of() { awk '{ print $1 }' "$STAGE_DIR/$1.sha256"; }
CARD="$STAGE_DIR/card.md"
cat > "$CARD" <<EOF
@SUBC basal: a FIRST placement, two binaries, one card each (ck-basal, ck-basal-worker)

**Card**
- stage: $STAGE_DIR (both binaries)
- ck-basal: $STAGE_DIR/ck-basal, sha256 $(sum_of ck-basal); basal.current revision $SHA
- ck-basal-worker: $STAGE_DIR/ck-basal-worker, sha256 $(sum_of ck-basal-worker); basal-worker.current revision $SHA
- build: release, clean tree, CK_BUILD_GIT_SHA=$SHA CK_BUILD_GIT_DIRTY=$DIRTY, commit $PUSHED. basal has no CI yet.

**Signing** (both): ad hoc, hardened runtime, explicit identifier, no entitlements, no get-task-allow
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
- This is a first placement: there is no running basal, so the gate's marker and control arms have no live binary to compare. place-module.sh refuses a missing destination before any arm ("destination does not exist, so this is an install rather than a placement"), so this card needs an install, not a gated placement. The two binaries' sha256 above, and the build revision in \`ck --json provenance basal\` afterwards, are the identity checks that remain.

**Store and formats**
- new module: no migration, no existing format, no format-floor.json. basal's store, <data_home>/cortexkit/basal/store.db, is created on first start. check-format-floors.sh finds no floors for basal and passes.

**Order**
- both binaries in ~/.local/share/cortexkit/bin before basal first starts: ck-basal runs ck-basal-worker from its own directory under that exact name.
- the daemon has no basal module yet: its subc.jsonc entry (reserved, so its routes carry reserved:basal) is the operator's approval step and comes after both files are in place.

**Post-placement check** (mine)
- \`ck --json provenance basal\`: build_git_sha $SHA, and the observed pid's running image is the placed ck-basal (same inode).
- a worker's running image is the placed ck-basal-worker (same inode) once a flow runs.
- BASAL_WORKER_IDENTIFIER=ck-basal-worker sh script/sign-worker.sh verify ~/.local/share/cortexkit/bin/ck-basal-worker passes at the placed path.
- basal's flow.list answers (no flows yet).
EOF
say ""
say "=== card (saved to $CARD; not posted)"
cat "$CARD"
say ""
say "staged $STAGE_DIR"
