#!/bin/sh
# flows-rig.sh: build, place and run basal's isolated test rig, ckdev-flows.
#
# The rig is a private subc daemon with its own port, its own XDG homes under
# ~/.local/share/cortexkit/ckdev-flows/ and its own ckdev-* binaries, every
# one built here from a named revision. It runs the credentials vault,
# Fusiform, Broca, entorhinal, prefrontal-core, prefrontal-routing, ck-basal and a
# rig-only callosum stub that answers consent cards as the operator. It never
# reads, writes or starts anything belonging to the production daemon or to
# another rig.
#
# Usage:
#   script/flows-rig.sh build --prefrontal-rev <rev> [--subc-rev <rev>]
#                             [--broca-rev <rev>] [--credentials-rev <rev>]
#                             [--commons-rev <rev>] [--entorhinal-rev <rev>]
#                             [--fusiform-rev <rev>]
#                             [--sibling-lock <repo>]... [--dry-run]
#   script/flows-rig.sh place    [--from-stage <dir>] [--dry-run]
#   script/flows-rig.sh config   [--dry-run]
#   script/flows-rig.sh start    [--dry-run]
#   script/flows-rig.sh status   [--dry-run]
#   script/flows-rig.sh stop     [--dry-run]
#   script/flows-rig.sh manifest [--dry-run]
#   script/flows-rig.sh credential --key-file <path> [--dry-run]
#   script/flows-rig.sh test     [--models] [--dry-run]
#
# --dry-run prints every command the subcommand would run and every file it
# would write, with the file's content, and changes nothing.
#
# Subcommands:
#   build     clone each repository at the named revision and build it.
#             --prefrontal-rev is required; the others default to their
#             checkout's HEAD. basal is always built from this checkout's
#             HEAD, without uncommitted changes. The source checkouts are
#             only read. A build that changes a tracked file fails, except
#             that --sibling-lock <repo> lets that repository's Cargo.lock
#             change the versions of path dependencies on a sibling clone
#             (claustrum builds against ../subconscious by path); the
#             manifest then records its cargo_lock as sibling-refreshed.
#   place     sign every binary by script/signing.sh under a ckdev-*
#             identifier and place it in bin/. The rig's ck-basal is built
#             with the rig-kill-hook feature, which the contract suite's
#             crash case needs.
#   config    write subc.jsonc, bootstrap the key-file vault and grant only
#             Broca's read of apikey:openai and routing's llm-provider list.
#             Temporarily start the rig to read its served Fusiform catalog,
#             then pin alfonso-routing.jsonc to openai/gpt-6-luna. Refuse a
#             catalog without that model. No production config is copied.
#   credential ingest the isolated API key with provider_ids: ["openai"],
#             delete the input file after deposit and confirm exactly one
#             active credential. The operator saves it at
#             ~/.local/share/cortexkit/ckdev-flows/home/.secrets/openai.key.
#   start, status, stop
#             run, inspect (pids, identifiers, open stores) and stop the
#             daemon. status fails if any open store lies outside the rig.
#   manifest  write results/<timestamp>/stack.json: every repository's
#             commit and each binary's sha256 and signing identifier.
#   test      start the rig if needed, write a manifest and run the live
#             contract suite (basal-rig-contract) against the real
#             prefrontal-core, with contract.json and contract.log beside
#             the manifest. It fails if any check failed.
#             Without --models, report the model cases as not run by name.
#             --models requires that credential and agent-owned flows through
#             core's relay (prefrontal 73c66ff1f or later): minimal call first,
#             routing/journal, result/usage settlement, classify, token-cap
#             refusal, crash recovery and no tools. Three tiny calls at most
#             in normal execution; each manifest caps all its scheduled runs
#             in a day, for an aggregate allowance of 3,088 tokens per suite.
#             Any first-call refusal is reported verbatim and stops the model
#             cases; it is never retried with another provider or model.
#
# place --from-stage <dir> places ck-basal and ck-basal-worker from a stage
# directory script/stage.sh wrote, byte for byte and under their production
# identifiers, instead of the rig's own build of them; every other binary is
# the rig's build. The stage must be of the commit the rig built basal at.
# A staged ck-basal has no kill switch, so test then reports the crash case
# as not run; a plain place puts the rig's build back.
#
# What the rig isolates:
#   - Everything lives under ~/.local/share/cortexkit/ckdev-flows/: src/ (one
#     clone per repository at the commit built), build/ (cargo output and the
#     build record), bin/ (the placed, signed binaries), config/, data/ and
#     runtime/ (the rig's three XDG homes), home/ (HOME and working directory
#     of every rig process), logs/ and results/<timestamp>/.
#   - The daemon listens on port 8791; production's listens on 8757. config
#     and start refuse if 8791 is taken.
#   - Every rig process starts from an empty environment: the login name,
#     HOME set to home/, the rig's XDG homes, SUBC_CONNECTION_FILE naming the
#     rig's connection file, and a PATH with every CortexKit directory
#     removed, so no production variable or file under the real home
#     reaches a rig module.
#   - The script refuses any path that resolves, before or after following
#     symlinks, under ~/.local/share/cortexkit/ but outside ckdev-flows/.
#   - The vault starts empty with its own key file, so the macOS keychain is
#     never touched. Every auth command names the rig data, key and connection
#     paths explicitly. Fusiform's store and HTTPS catalog polling are isolated
#     too; no production auth-methods.json is ever copied into the rig.
#
# Model cases need core's agent-owned flow scope registration at approval:
# prefrontal 73c66ff1f or later, plus a basal build that opens scoped Broca
# routes. Without the latter, the first call reports scope_owner_mismatch;
# it must not be bypassed. The other repositories default to their HEAD:
#   script/flows-rig.sh build --prefrontal-rev 73c66ff1f --sibling-lock claustrum
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
# The signing policy stage.sh signs production binaries with.
# shellcheck source=script/signing.sh
. "$ROOT/script/signing.sh"
WORKSPACE=${CORTEXKIT_WORKSPACE:-$HOME/Work/Projects/CortexKit}
CK_SHARE="$HOME/.local/share/cortexkit"
CK_CONFIG="$HOME/.config/cortexkit"
RIG="$CK_SHARE/ckdev-flows"

BIN="$RIG/bin"
SRC="$RIG/src"
TARGETS="$RIG/build/target"
STACK="$RIG/build/stack.tsv"
# Where the placed ck-basal and ck-basal-worker came from: the rig's build
# (with the kill switch) or a stage. `place` writes it; `test`, `status` and
# the manifest read it.
BASAL_SOURCE="$RIG/build/basal-source.tsv"
LOGS="$RIG/logs"
RESULTS="$RIG/results"
CONFIG_HOME="$RIG/config"
DATA_HOME="$RIG/data"
RUNTIME_DIR="$RIG/runtime"
# HOME for every rig process. A module that falls back to $HOME for any path
# (OpenCode's auth.json, a ~/.config file, a dot-directory) then lands inside
# the rig instead of reaching the user's real files.
RIG_HOME="$RIG/home"
CONN="$RUNTIME_DIR/subc-connection.json"
PIDFILE="$RUNTIME_DIR/ckdev-subc.pid"
SUBC_CONFIG="$CONFIG_HOME/cortexkit/subc.jsonc"
ROUTING_CONFIG="$CONFIG_HOME/cortexkit/alfonso-routing.jsonc"
BROCA_INDEX="$DATA_HOME/cortexkit/broca/run-index.db"
# The vault's data directory is where the daemon's storage convention puts
# module `claustrum`. Its master key must live outside that directory (the
# vault refuses a key beside its own store), so it sits in the config home.
VAULT_DIR="$DATA_HOME/cortexkit/claustrum"
VAULT_KEY="$CONFIG_HOME/claustrum/master.key"
# The stores the contract suite reads (read-only), and the file whose content
# names the runtime boundary at which the rig build of ck-basal kills itself
# once (crates/basal-module/src/rig_kill.rs); the suite writes it.
CORE_STORE="$DATA_HOME/cortexkit/prefrontal-core/store.db"
BASAL_STORE="$DATA_HOME/cortexkit/basal/store.db"
MACHINE_ID="$DATA_HOME/cortexkit/machine-id"
KILL_FILE="$RUNTIME_DIR/basal-kill-at"
# The contract suite: a client of the rig, not a module, so it is run from
# the build output rather than placed in bin/.
CONTRACT="$TARGETS/basal/release/basal-rig-contract"
# Where the contract suite's projects live: one git repository per run, which
# the rig creates under its own home and registers in its entorhinal (see
# ensure_project).
PROJECTS="$RIG_HOME/projects"
# Clear of production's daemon (8757) and of the older isolation rig under
# ckdev-rig/ (8799, plus 8377 and 8378 for one of its modules).
PORT=8791

DRY=0

say() { printf '%s\n' "$*"; }
die() { printf 'flows-rig: %s\n' "$*" >&2; exit 1; }

usage() {
  awk '/^# Usage:/ { on = 1 } /^set -eu/ { exit } on' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

# ---------------------------------------------------------------- guards

# The physical form of a path, following symlinks in every existing parent,
# so a symlink inside the rig cannot point a write at production.
physical() {
  p=$1
  rest=""
  while [ ! -e "$p" ] && [ "$p" != "/" ]; do
    rest="/$(basename "$p")$rest"
    p=$(dirname "$p")
  done
  if [ -d "$p" ]; then
    printf '%s%s\n' "$(cd -P "$p" && pwd -P)" "$rest"
  else
    printf '%s/%s%s\n' "$(cd -P "$(dirname "$p")" && pwd -P)" "$(basename "$p")" "$rest"
  fi
}

# Refuse a path under ~/.local/share/cortexkit/ that is not inside the rig,
# both as written and after resolving symlinks.
guard_path() {
  for form in "$1" "$(physical "$1")"; do
    case "$form" in
      "$RIG" | "$RIG"/* | "$RIG_PHYSICAL" | "$RIG_PHYSICAL"/*) ;;
      "$CK_SHARE" | "$CK_SHARE"/* | "$CK_SHARE_PHYSICAL" | "$CK_SHARE_PHYSICAL"/*)
        die "refusing: $1 resolves to $form, under $CK_SHARE but outside $RIG" ;;
    esac
  done
}

guard_all_paths() {
  CK_SHARE_PHYSICAL=$(physical "$CK_SHARE")
  RIG_PHYSICAL=$(physical "$RIG")
  case "$RIG_PHYSICAL" in
    "$CK_SHARE_PHYSICAL"/ckdev-flows) ;;
    *) die "refusing: the rig root $RIG resolves to $RIG_PHYSICAL" ;;
  esac
  for path in "$BIN" "$SRC" "$TARGETS" "$STACK" "$BASAL_SOURCE" "$LOGS" "$RESULTS" \
      "$CONFIG_HOME" "$DATA_HOME" "$RUNTIME_DIR" "$CONN" "$PIDFILE" \
      "$SUBC_CONFIG" "$ROUTING_CONFIG" "$BROCA_INDEX" "$VAULT_DIR" "$VAULT_KEY" "$RIG_HOME" "$CORE_STORE" \
      "$BASAL_STORE" "$MACHINE_ID" "$KILL_FILE" "$CONTRACT" "$PROJECTS"; do
    guard_path "$path"
  done
}

# The pid listening on the rig port, if any.
port_listener() {
  lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | head -n 1 || true
}

# For subcommands that would start a daemon: the port must be free.
guard_port_free() {
  listener=$(port_listener)
  [ -z "$listener" ] || die "refusing: port $PORT is already taken by pid $listener"
}

# For subcommands that talk to a running rig: the port is free or held by the
# rig's own daemon, never by anything else.
guard_port_ours() {
  listener=$(port_listener)
  [ -z "$listener" ] || [ "$listener" = "${1:-}" ] \
    || die "refusing: port $PORT is held by pid $listener, which is not the rig daemon"
}

# ---------------------------------------------------------------- helpers

run() {
  if [ "$DRY" = 1 ]; then
    say "+ $*"
  else
    "$@"
  fi
}

# Run a command in a directory (cargo picks its toolchain file from there).
run_in() {
  dir=$1
  shift
  if [ "$DRY" = 1 ]; then
    say "+ (cd $dir) $*"
  else
    (cd "$dir" && "$@")
  fi
}

# Write stdin to a file through a temp name and a rename.
write_file() {
  dest=$1
  guard_path "$dest"
  if [ "$DRY" = 1 ]; then
    say "+ write $dest:"
    sed 's/^/|   /'
    return
  fi
  mkdir -p "$(dirname "$dest")"
  tmp=$(mktemp "$(dirname "$dest")/.write.XXXXXX")
  cat > "$tmp"
  mv -f "$tmp" "$dest"
}

# PATH with every entry under the CortexKit data or config tree removed, so a
# rig process that runs a program by name can never reach a production binary.
rig_path() {
  # The trailing newline matters: read drops a final line that lacks one.
  printf '%s\n' "$PATH" | tr ':' '\n' | while IFS= read -r entry; do
    case "$entry" in
      "$CK_SHARE" | "$CK_SHARE"/* | "$CK_CONFIG" | "$CK_CONFIG"/* | "") ;;
      *) printf '%s\n' "$entry" ;;
    esac
  done | paste -s -d: -
}

# Run a command in the rig's environment: an empty environment plus the rig's
# own HOME (also the working directory), its three XDG homes, its connection
# file, a temp dir inside its runtime dir, and the user's name. Nothing
# inherited from the caller can redirect a module at production state
# (CK_MASTER_KEY_PATH, BROCA_STATE_ROOT, SUBC_PORT, the real HOME, ...).
# rig_env_exec replaces the calling (sub)shell, so `(rig_env_exec cmd) &`
# leaves $! naming cmd itself.
rig_env() {
  (rig_env_exec "$@")
}

rig_env_exec() {
  cd "$RIG_HOME" || die "no rig home at $RIG_HOME; run start first"
  # Close every descriptor above stderr first. Whatever launched this script
  # (an agent's shell tool, a terminal multiplexer) may hold files open, and a
  # daemon inheriting them would keep files outside the rig open for its whole
  # life and pass them on to every module it spawns.
  exec python3 -c 'import os, sys
os.closerange(3, min(os.sysconf("SC_OPEN_MAX"), 1 << 16))
os.execvp(sys.argv[1], sys.argv[1:])' \
    env -i \
    HOME="$RIG_HOME" USER="${USER:-}" LOGNAME="${LOGNAME:-}" LANG="${LANG:-en_US.UTF-8}" \
    PATH="$(rig_path)" \
    TMPDIR="$RUNTIME_DIR/tmp" \
    XDG_CONFIG_HOME="$CONFIG_HOME" \
    XDG_DATA_HOME="$DATA_HOME" \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    SUBC_CONNECTION_FILE="$CONN" \
    "$@"
}

# `ck` against the rig. SUBC_CONNECTION_FILE is exclusive in ck's discovery
# (it never falls back to another daemon), and any `daemon: <path>` line ck
# prints must name the rig's connection file. ck prints that line only after
# a module mutation; for read verbs, check_daemon_identity compares pids.
rig_ck() {
  out=$(rig_env "$BIN/ckdev-ck" "$@" 2>&1) || {
    printf '%s\n' "$out"
    die "ck $* failed"
  }
  printf '%s\n' "$out"
  named=$(printf '%s\n' "$out" | sed -n 's/^daemon: \(\/.*\)$/\1/p')
  if [ -n "$named" ] && [ "$named" != "$CONN" ]; then
    die "ck $* answered from $named, not the rig's $CONN"
  fi
}

# ck's auth face is linked only inside the rig. All three paths are explicit
# even for offline bootstrap, so neither CLI discovery nor a keychain fallback
# can ever select the operator's real vault.
rig_auth() {
  # Follow final symlinks too: a key-file or connection-file symlink must not
  # make explicit rig flags a disguised reference to production.
  python3 - "$RIG_PHYSICAL" "$VAULT_DIR" "$VAULT_KEY" "$CONN" <<'PY' || die "auth paths escape the rig"
import os, sys
root = sys.argv[1] + os.sep
for path in sys.argv[2:]:
    if not os.path.realpath(path).startswith(root):
        sys.exit("flows-rig: auth path resolves outside the rig: " + path)
PY
  run rig_env PATH="$BIN:$(rig_path)" "$BIN/ckdev-ck" auth "$@" \
    --data-dir "$VAULT_DIR" --key-path "$VAULT_KEY" --subc "$CONN"
}

conn_pid() {
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pid"])' "$CONN" 2>/dev/null || true
}

identifier_of() {
  codesign_identifier "$1"
}

# The placement mode `place` recorded: "rig-build" or "staged".
basal_mode() {
  if [ -f "$BASAL_SOURCE" ]; then
    cut -f1 "$BASAL_SOURCE"
  else
    printf 'rig-build\n'
  fi
}

# The identifier a placed basal binary must carry: its production one when
# it came from a stage, the rig's ckdev- one otherwise.
basal_identifier() {
  if [ "$(basal_mode)" = staged ]; then
    printf '%s\n' "$1"
  else
    printf 'ckdev-%s\n' "${1#ck-}"
  fi
}

# The repositories the rig builds from: name, source checkout, Cargo-built.
# The clones under $SRC keep these names because the repositories reach each
# other by relative path (../subconscious, ../commons, ../claustrum).
repos() {
  cat <<EOF
subconscious	$WORKSPACE/subconscious
commons	$WORKSPACE/commons
broca	$WORKSPACE/broca
claustrum	$WORKSPACE/claustrum
entorhinal	$WORKSPACE/entorhinal
fusiform	$WORKSPACE/fusiform
prefrontal	$WORKSPACE/prefrontal
basal	$ROOT
EOF
}

# Every placed binary: rig name, repository, cargo binary, placed file name.
# ck-basal finds its worker beside itself under the fixed name ck-basal-worker
# (crates/basal-module/src/pool.rs), so that one file keeps its cargo name.
binaries() {
  cat <<'EOF'
subc	subconscious	ck-subc	ckdev-subc
ck	subconscious	ck	ckdev-ck
broca	broca	ck-broca	ckdev-broca
claustrum	claustrum	ck-claustrum	ckdev-claustrum
auth	claustrum	ck-auth	ckdev-auth
entorhinal	entorhinal	ck-entorhinal	ckdev-entorhinal
fusiform	fusiform	ck-fusiform	ckdev-fusiform
models	fusiform	ck-models	ckdev-models
prefrontal-core	prefrontal	ck-prefrontal-core	ckdev-prefrontal-core
prefrontal-routing	prefrontal	ck-prefrontal-routing	ckdev-prefrontal-routing
basal	basal	ck-basal	ckdev-basal
basal-worker	basal	ck-basal-worker	ck-basal-worker
callosum	basal	ck-callosum-stub	ckdev-callosum
EOF
}

# The supervised modules: subc module id and placed file name.
modules() {
  cat <<'EOF'
claustrum	ckdev-claustrum
broca	ckdev-broca
fusiform	ckdev-fusiform
entorhinal	ckdev-entorhinal
prefrontal-core	ckdev-prefrontal-core
prefrontal-routing	ckdev-prefrontal-routing
basal	ckdev-basal
callosum	ckdev-callosum
EOF
}

all_placed() {
  for file in $(binaries | cut -f4); do
    [ -f "$BIN/$file" ] || return 1
  done
}

daemon_pid() {
  [ -f "$PIDFILE" ] || return 0
  pid=$(cat "$PIDFILE")
  if kill -0 "$pid" 2>/dev/null; then
    printf '%s\n' "$pid"
  fi
}

# ---------------------------------------------------------------- build

cmd_build() {
  prefrontal_rev=""
  subc_rev=HEAD
  broca_rev=HEAD
  credentials_rev=HEAD
  commons_rev=HEAD
  entorhinal_rev=HEAD
  fusiform_rev=HEAD
  sibling_lock=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --sibling-lock) [ $# -ge 2 ] || usage; sibling_lock="$sibling_lock $2"; shift 2 ;;
      --prefrontal-rev) [ $# -ge 2 ] || usage; prefrontal_rev=$2; shift 2 ;;
      --subc-rev) [ $# -ge 2 ] || usage; subc_rev=$2; shift 2 ;;
      --broca-rev) [ $# -ge 2 ] || usage; broca_rev=$2; shift 2 ;;
      --credentials-rev) [ $# -ge 2 ] || usage; credentials_rev=$2; shift 2 ;;
      --commons-rev) [ $# -ge 2 ] || usage; commons_rev=$2; shift 2 ;;
      --entorhinal-rev) [ $# -ge 2 ] || usage; entorhinal_rev=$2; shift 2 ;;
      --fusiform-rev) [ $# -ge 2 ] || usage; fusiform_rev=$2; shift 2 ;;
      *) usage ;;
    esac
  done
  # prefrontal has no default: every rig result must name the core and
  # routing it tested, and a silent HEAD would name whatever happened to be
  # checked out.
  [ -n "$prefrontal_rev" ] || die "build needs --prefrontal-rev <rev>; it has no default"
  if [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
    say "note: $ROOT has uncommitted changes; the rig builds its HEAD commit only"
  fi
  # Forget the previous build before starting this one. The record is written
  # again only when every crate has built, so after a failed build `place`
  # refuses instead of placing a mix of new and old binaries.
  run rm -f "$STACK"

  stack=""
  for line in $(repos | tr '\t' '|'); do
    name=${line%%|*}
    source=${line#*|}
    case "$name" in
      subconscious) rev=$subc_rev ;;
      commons) rev=$commons_rev ;;
      broca) rev=$broca_rev ;;
      claustrum) rev=$credentials_rev ;;
      entorhinal) rev=$entorhinal_rev ;;
      fusiform) rev=$fusiform_rev ;;
      prefrontal) rev=$prefrontal_rev ;;
      basal) rev=HEAD ;;
    esac
    [ -d "$source" ] || die "no checkout of $name at $source"
    sha=$(git -C "$source" rev-parse --verify --quiet "$rev^{commit}") \
      || die "$name: cannot resolve $rev in $source"
    say "$name: $rev -> $sha ($source)"
    clone_at "$name" "$source" "$sha"
    stack="$stack$name	$source	$rev	$sha
"
  done

  cargo_build subconscious "" -p subc-core --bin ck-subc --bin ck
  cargo_build broca "" -p broca-module-serve --bin ck-broca
  cargo_build claustrum "" -p credentials-module --bin ck-claustrum --bin ck-auth
  cargo_build entorhinal "" -p entorhinal-module --bin ck-entorhinal
  cargo_build fusiform "" -p fusiform-module --bin ck-fusiform
  cargo_build fusiform "" -p fusiform-cli --bin ck-models
  prefrontal_sha=$(printf '%s' "$stack" | awk -F'\t' '$1=="prefrontal"{print $4}')
  # prefrontal's build scripts embed the revision they were told; the clone
  # is at that exact commit with no local changes, so it is not dirty.
  cargo_build prefrontal "CK_BUILD_GIT_SHA=$prefrontal_sha CK_BUILD_GIT_DIRTY=false" \
    -p prefrontal-core-module --bin ck-prefrontal-core
  cargo_build prefrontal "CK_BUILD_GIT_SHA=$prefrontal_sha CK_BUILD_GIT_DIRTY=false" \
    -p prefrontal-routing-module --bin ck-prefrontal-routing
  # The rig's ck-basal carries the one-shot kill switch the contract suite's
  # crash case arms; a production build never enables this feature.
  # basal's binaries embed their revision too, as script/stage.sh builds them.
  basal_sha=$(printf '%s' "$stack" | awk -F'\t' '$1=="basal"{print $4}')
  basal_env="CK_BUILD_GIT_SHA=$basal_sha CK_BUILD_GIT_DIRTY=false"
  cargo_build basal "$basal_env" -p basal-module --features rig-kill-hook --bin ck-basal
  cargo_build basal "$basal_env" -p basal-worker --bin ck-basal-worker
  cargo_build basal "" -p basal-rig --bin ck-callosum-stub --bin basal-rig-contract

  # A build that rewrote a tracked file (a Cargo.lock refreshed against a
  # sibling at another revision) did not build exactly the named commit.
  # The one exception is a repository named with --sibling-lock: its
  # Cargo.lock may change, but only the versions of path dependencies on a
  # sibling clone. That happens when the repository was built in production
  # against a sibling checkout at an earlier revision than the rig's. The
  # change is saved beside the clone, recorded in the stack, and undone.
  locks=""
  for line in $(repos | tr '\t' '|'); do
    name=${line%%|*}
    [ "$DRY" = 0 ] || continue
    changed=$(git -C "$SRC/$name" status --porcelain --untracked-files=no)
    [ -n "$changed" ] || continue
    case " $sibling_lock " in
      *" $name "*)
        if [ "$changed" = " M Cargo.lock" ] && sibling_versions_only "$SRC/$name"; then
          git -C "$SRC/$name" diff Cargo.lock > "$SRC/$name.sibling-lock.diff"
          git -C "$SRC/$name" checkout --quiet -- Cargo.lock
          say "$name: Cargo.lock refreshed against sibling path dependencies; diff in $SRC/$name.sibling-lock.diff"
          locks="$locks $name"
          continue
        fi
        ;;
    esac
    git -C "$SRC/$name" status --short --untracked-files=no >&2
    die "$name: the build changed tracked files in $SRC/$name"
  done
  stack=$(printf '%s' "$stack" | while IFS='	' read -r n s r c; do
    # Leading-paren patterns: /bin/sh's bash 3.2 misparses a bare pattern's
    # closing paren inside a command substitution.
    case " $locks " in (*" $n "*) l=sibling-refreshed ;; (*) l=committed ;; esac
    printf '%s\t%s\t%s\t%s\t%s\n' "$n" "$s" "$r" "$c" "$l"
  done)
  printf 'repo\tsource\trequested\tcommit\tcargo_lock\n%s\n' "$stack" | write_file "$STACK"
  say "built; next: $0 place"
}

# sibling_versions_only <clone>: succeed if the clone's Cargo.lock differs from
# its commit only in the version of packages that have no `source`, which are
# path dependencies, and adds or removes no package.
sibling_versions_only() {
  git -C "$1" show HEAD:Cargo.lock > "$1.committed-lock"
  status=0
  python3 - "$1.committed-lock" "$1/Cargo.lock" <<'PY' || status=$?
import sys
def blocks(path):
    out = {}
    for chunk in open(path).read().split("[[package]]")[1:]:
        fields = dict(
            line.split(" = ", 1) for line in chunk.strip().splitlines()
            if " = " in line and not line.startswith(" ")
        )
        out[(fields["name"], fields.get("source"))] = (fields, chunk)
    return out
old = blocks(sys.argv[1])
new = blocks(sys.argv[2])
if old.keys() != new.keys():
    sys.exit(1)
for key, (fields, chunk) in new.items():
    if chunk == old[key][1]:
        continue
    if key[1] is not None:
        sys.exit(1)
    if chunk.replace(fields["version"], old[key][0]["version"], 1) != old[key][1]:
        sys.exit(1)
PY
  rm -f "$1.committed-lock"
  return "$status"
}
# Bring $SRC/<name> to exactly <sha> without touching the source checkout:
# a separate clone, detached at the commit, with no local changes.
clone_at() {
  name=$1
  source=$2
  sha=$3
  dest="$SRC/$name"
  guard_path "$dest"
  if [ -d "$dest/.git" ]; then
    # The clone is the rig's own; origin only says where to fetch from, and
    # basal's source moves between worktrees.
    run git -C "$dest" remote set-url origin "$source"
    run git -C "$dest" fetch --quiet origin
  else
    run mkdir -p "$SRC"
    run git clone --quiet --no-checkout "$source" "$dest"
  fi
  if [ "$DRY" = 0 ] && ! git -C "$dest" cat-file -e "$sha^{commit}" 2>/dev/null; then
    git -C "$dest" fetch --quiet origin "$sha"
  fi
  run git -C "$dest" checkout --quiet --force --detach "$sha"
  if [ "$DRY" = 0 ] && [ -n "$(git -C "$dest" status --porcelain --untracked-files=no)" ]; then
    die "$dest has local changes after checkout"
  fi
}

# cargo_build <repo> "<VAR=value ...>" <cargo args>: a release build in the
# clone, into a target directory of the rig's own.
cargo_build() {
  repo=$1
  extra_env=$2
  shift 2
  # shellcheck disable=SC2086 # extra_env is a list of VAR=value words
  run_in "$SRC/$repo" env CARGO_TARGET_DIR="$TARGETS/$repo" $extra_env \
    cargo build --release "$@"
}

# ---------------------------------------------------------------- place

cmd_place() {
  stage=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --from-stage) [ $# -ge 2 ] || usage; stage=$2; shift 2 ;;
      *) usage ;;
    esac
  done
  [ -z "$(daemon_pid)" ] || die "refusing: the rig daemon is running; stop it first"
  [ "$DRY" = 1 ] || [ -f "$STACK" ] || die "nothing built yet; run build first"
  revision=""
  if [ -n "$stage" ]; then
    stage=$(cd "$stage" 2>/dev/null && pwd -P) || die "no stage directory at $stage"
    # stage.sh's own check of a stage: both sidecars and both signatures,
    # under the production identifiers the fleet's placement would use.
    verified=$(sh "$ROOT/script/stage.sh" --verify "$stage") \
      || die "the stage at $stage fails stage.sh --verify"
    revision=$(printf '%s\n' "$verified" | sed -n 's/^revision //p')
    if [ "$DRY" = 0 ]; then
      built=$(awk -F'\t' '$1=="basal"{print $4}' "$STACK")
      # The callosum stub, the contract suite and the worker's gate script
      # come from the rig's build, so it must be of the staged commit.
      [ "$revision" = "$built" ] \
        || die "the stage is of $revision but the rig built basal at $built; build the rig at the staged commit first"
    fi
  fi
  run mkdir -p "$BIN"
  for line in $(binaries | tr '\t' '|'); do
    name=$(printf '%s' "$line" | cut -d'|' -f1)
    repo=$(printf '%s' "$line" | cut -d'|' -f2)
    cargo_bin=$(printf '%s' "$line" | cut -d'|' -f3)
    file=$(printf '%s' "$line" | cut -d'|' -f4)
    case "$name" in
      basal | basal-worker)
        if [ -n "$stage" ]; then
          place_staged "$name" "$stage/$cargo_bin" "$BIN/$file" "$cargo_bin"
          continue
        fi ;;
    esac
    place_one "$name" "$TARGETS/$repo/release/$cargo_bin" "$BIN/$file"
  done
  run ln -sf ckdev-auth "$BIN/ck-auth"
  if [ -n "$stage" ]; then
    printf 'staged\t%s\t%s\n' "$stage" "$revision" | write_file "$BASAL_SOURCE"
    worker_identifier=ck-basal-worker
  else
    printf 'rig-build\n' | write_file "$BASAL_SOURCE"
    worker_identifier=ckdev-basal-worker
  fi
  # The worker runs flow code, so it must also pass basal's own gate at its
  # final path: hardened runtime, no entitlements, the checked-in Seatbelt
  # profile embedded, and a live probe that confinement denies a file read,
  # a socket connect and a program launch. The gate script comes from the
  # basal clone, so it checks against the profile of the commit that was built.
  worker="$BIN/ck-basal-worker"
  if [ "$DRY" = 1 ]; then
    say "+ BASAL_WORKER_IDENTIFIER=$worker_identifier sh $SRC/basal/script/sign-worker.sh verify $worker"
  elif ! BASAL_WORKER_IDENTIFIER=$worker_identifier sh "$SRC/basal/script/sign-worker.sh" verify "$worker"; then
    rm -f "$worker"
    die "the placed worker failed basal's confinement gate and was removed"
  fi
  say "placed; next: $0 config"
}

# Sign a copy under a temp name with an explicit identifier (codesign would
# otherwise derive one from the temp name), check it, rename it into place,
# and check the final file again.
place_one() {
  name=$1
  built=$2
  dest=$3
  identifier="ckdev-$name"
  guard_path "$dest"
  if [ "$DRY" = 1 ]; then
    say "+ cp $built $BIN/.place.XXXXXX; chmod 0755 $BIN/.place.XXXXXX"
    say "+ sign_hardened $BIN/.place.XXXXXX $identifier   (script/signing.sh)"
    say "+ verify_hardened $BIN/.place.XXXXXX $identifier"
    say "+ mv -f $BIN/.place.XXXXXX $dest"
    say "+ verify_hardened $dest $identifier"
    return
  fi
  [ -f "$built" ] || die "$name: no built binary at $built"
  tmp=$(mktemp "$BIN/.place.XXXXXX")
  cp "$built" "$tmp"
  # mktemp creates the file 0600 and cp keeps that mode on an existing file.
  chmod 0755 "$tmp"
  # The production signing policy: production modules run with the hardened
  # runtime, and basal's worker gate refuses a worker without it, so the rig
  # matches both.
  sign_hardened "$tmp" "$identifier" 2>/dev/null
  if ! verify_hardened "$tmp" "$identifier"; then
    rm -f "$tmp"
    die "$name: the signed copy fails the signing policy"
  fi
  mv -f "$tmp" "$dest"
  verify_hardened "$dest" "$identifier" || die "$name: $dest fails the signing policy"
  say "placed $dest ($identifier)"
}

# place_staged <name> <staged file> <dest> <identifier>: copy a staged binary
# into the rig unchanged (never re-signed: the rig runs the bytes production
# placement would install), and check at the final path that the bytes match the stage's
# sidecar and that the signature passes under its production identifier.
place_staged() {
  name=$1
  staged=$2
  dest=$3
  identifier=$4
  guard_path "$dest"
  if [ "$DRY" = 1 ]; then
    say "+ cp $staged $BIN/.place.XXXXXX; chmod 0755 $BIN/.place.XXXXXX; mv -f $BIN/.place.XXXXXX $dest"
    say "+ sha256 of $dest must equal $staged.sha256; verify_hardened $dest $identifier"
    return
  fi
  tmp=$(mktemp "$BIN/.place.XXXXXX")
  cp "$staged" "$tmp"
  chmod 0755 "$tmp"
  mv -f "$tmp" "$dest"
  want=$(awk '{ print $1 }' "$staged.sha256")
  got=$(shasum -a 256 "$dest" | awk '{ print $1 }')
  [ "$got" = "$want" ] || die "$name: $dest is $got, but the stage's sidecar says $want"
  verify_hardened "$dest" "$identifier" || die "$name: $dest fails the signing policy"
  say "placed $dest from the stage ($identifier, sha256 $got)"
}

# ---------------------------------------------------------------- config

cmd_config() {
  [ $# -eq 0 ] || usage
  guard_port_free
  [ -z "$(daemon_pid)" ] || die "refusing: the rig daemon is running; stop it first"
  # From scratch: the whole file is generated here, never merged with a
  # previous one. ck-bus and nats-server are left out (the bus needs its own
  # NATS server, operator JWT and vault signing); prefrontal-core then
  # reports "bus: connecting" in its health, which is expected on this rig.
  write_file "$SUBC_CONFIG" <<EOF
{
  // ckdev-flows: basal's isolated rig, written by script/flows-rig.sh config.
  // Regenerate it with that command rather than editing it by hand.
  "version": 1,
  "port": $PORT,
  "storage": {
    "backend": "sqlite",
    // Every module's store is <data_home>/cortexkit/<module id>/store.db.
    "data_home": "$DATA_HOME"
  },
  "modules": {
    // The credentials vault, keyed by a master key file of the rig's
    // own (never the macOS keychain, never a production key).
    "claustrum": {
      "program": "$BIN/ckdev-claustrum",
      "args": [],
      "env": { "CK_MASTER_KEY_PATH": "$VAULT_KEY" },
      "enabled": true,
      "reserved": true,
      "launch_nonce_env": false
    },
    "broca": {
      "program": "$BIN/ckdev-broca",
      "args": [],
      "env": { "BROCA_STATE_ROOT": "$DATA_HOME/cortexkit/broca" },
      "enabled": true,
      "reserved": true,
      "launch_nonce_env": false
    },
    // The daemon supplies data/cortexkit/fusiform/store.db to this module.
    "fusiform": {
      "program": "$BIN/ckdev-fusiform",
      "args": [],
      "env": {},
      "enabled": true
    },
    // The project-identity/v1 provider. prefrontal-core declares that
    // capability required, and the daemon refuses every route to core until
    // a provider has registered. Its store is data/cortexkit/entorhinal/.
    "entorhinal": {
      "program": "$BIN/ckdev-entorhinal",
      "args": [],
      "env": {},
      "enabled": true
    },
    // Core runs its projects registry consumer against the rig's entorhinal,
    // as in production.
    // Never set PREFRONTAL_CORE_DIAGNOSTICS here: it opens core's test seams,
    // which exist for prefrontal's end-to-end harness only.
    "prefrontal-core": {
      "program": "$BIN/ckdev-prefrontal-core",
      "args": [],
      "env": {},
      "enabled": true,
      "reserved": true,
      "launch_nonce_env": false
    },
    "prefrontal-routing": {
      "program": "$BIN/ckdev-prefrontal-routing",
      "args": [],
      "env": {},
      "enabled": true,
      "reserved": true,
      "launch_nonce_env": false
    },
    // Reserved, so its routes carry the principal reserved:basal. The rig's
    // build reads BASAL_RIG_KILL_FILE for the contract suite's crash case; a
    // production ck-basal has no such switch and ignores the variable.
    "basal": {
      "program": "$BIN/ckdev-basal",
      "args": [],
      "env": { "BASAL_RIG_KILL_FILE": "$KILL_FILE" },
      "enabled": true,
      "reserved": true
    },
    // Rig only, never in a production config: a stub under the reserved id
    // callosum, which core and basal both treat as the operator. It answers
    // the contract suite's consent cards (crates/basal-rig).
    "callosum": {
      "program": "$BIN/ckdev-callosum",
      "args": [],
      "env": {},
      "enabled": true,
      "reserved": true
    }
  }
}
EOF
  run mkdir -p -m 700 "$RIG_HOME" "$RUNTIME_DIR/tmp" "$(dirname "$VAULT_KEY")"
  rig_auth bootstrap
  if [ "$DRY" = 1 ]; then
    say "+ start the temporary rig for authenticated vault grants and its served catalog"
  else
    cmd_start
  fi
  # --subc requires a live connection file for grants. Bootstrap is the one
  # offline command; grant writes go through the running vault's admin route.
  config_status=0
  configure_live || config_status=$?
  if [ "$DRY" = 1 ]; then
    say "+ stop the temporary rig"
  else
    # Stop even when an authenticated grant or catalog read was refused.
    cmd_stop
  fi
  [ "$config_status" = 0 ] || die "cannot configure the rig's vault grants and served Luna pin"
  say "configured; next: $0 start"
}

configure_live() {
  # Grants are vault records, not subc JSON fields. Re-running grant is
  # idempotent; these are the only authorities this configuration installs.
  rig_auth grant --principal reserved:broca --selector-kind exact \
    --selector apikey:openai --operation read || return 1
  rig_auth grant --principal reserved:prefrontal-routing --selector-kind category \
    --selector llm-provider --operation list || return 1
  if [ "$DRY" = 1 ]; then
    say "+ read the served catalog: ckdev-models get --subc $CONN --json"
    say "+ write $ROUTING_CONFIG: model_routing.exclude = [every served provider, -openai/gpt-6-luna]"
    say "+ fail if openai/gpt-6-luna is absent"
  else
    pin_routing
  fi
}

pin_routing() {
  catalog=$(rig_env "$BIN/ckdev-models" get --subc "$CONN" --json) || return 1
  policy=$(printf '%s\n' "$catalog" | python3 -c 'import json, sys
catalog = json.load(sys.stdin)
models = catalog["models"]
if "openai/gpt-6-luna" not in models:
    sys.exit("flows-rig: served Fusiform catalog lacks openai/gpt-6-luna")
providers = sorted({key.split("/", 1)[0] for key in models})
print(json.dumps({"model_routing": {"exclude": providers + ["-openai/gpt-6-luna"]}}, indent=2))') || return 1
  printf '%s\n' "$policy" | write_file "$ROUTING_CONFIG"
}

# ---------------------------------------------------------------- credential

cmd_credential() {
  [ $# -eq 2 ] && [ "$1" = --key-file ] || usage
  key_file=$2
  # realpath also follows the final component: never ingest or delete a
  # production file through a symlink in the rig's secrets directory.
  key_file=$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$key_file")
  case "$key_file" in
    "$RIG_PHYSICAL"/*) ;;
    *) die "the payload file must be inside $RIG" ;;
  esac
  if [ "$DRY" = 1 ]; then
    rig_auth put --id apikey:openai --provider-id openai --payload-file "$key_file"
    run rm -f "$key_file"
    rig_auth status
    say "+ verify exactly one credential: active apikey:openai, provider_ids [openai]"
    return
  fi
  [ -f "$key_file" ] || die "no input key file at $key_file"
  pid=$(daemon_pid)
  [ -n "$pid" ] || die "start the rig before depositing its credential"
  check_daemon_identity "$pid"
  rig_auth put --id apikey:openai --provider-id openai --payload-file "$key_file"
  rm -f "$key_file"
  rig_auth status
  require_model_credential
}

require_model_credential() {
  listing=$(rig_auth list) || { say "cannot read the rig credential inventory" >&2; return 1; }
  printf '%s\n' "$listing" | python3 -c 'import sys
lines = sys.stdin.read().splitlines()
start = next((i for i, line in enumerate(lines) if line.startswith("STATE ")), None)
if start is None:
    sys.exit("flows-rig: missing credential inventory header")
rows = []
for line in lines[start+1:]:
    if not line.strip(): break
    rows.append(line.split())
if len(rows) != 1 or rows[0][0] != "active" or rows[0][2:] != ["apikey:openai", "llm-provider", "openai"]:
    sys.exit("flows-rig: expected exactly one active apikey:openai credential with provider_ids [openai]")' \
    || return 1
}

# ---------------------------------------------------------------- start

cmd_start() {
  [ $# -eq 0 ] || usage
  [ -z "$(daemon_pid)" ] || die "the rig daemon is already running (pid $(daemon_pid))"
  guard_port_free
  if [ "$DRY" = 0 ]; then
    [ -f "$SUBC_CONFIG" ] || die "no rig config; run config first"
    for file in $(binaries | cut -f4); do
      [ -x "$BIN/$file" ] || die "$BIN/$file is not placed; run place first"
    done
  fi
  check_rig_tools
  run mkdir -p -m 700 "$RUNTIME_DIR" "$RUNTIME_DIR/tmp" "$LOGS" "$DATA_HOME" "$(dirname "$VAULT_KEY")" "$RIG_HOME"

  # An empty vault of the rig's own. Bootstrap mints a fresh master key into
  # the rig's key file and is idempotent; nothing is copied from anywhere.
  if [ "$DRY" = 1 ] || [ ! -f "$VAULT_DIR/store.db" ]; then
    rig_auth bootstrap
  fi

  log="$LOGS/daemon-$(date -u +%Y%m%dT%H%M%SZ).out"
  if [ "$DRY" = 1 ]; then
    say "+ rig_env nohup $BIN/ckdev-subc >> $log 2>&1 &   (pid into $PIDFILE)"
    say "+ wait for $CONN to name that pid; ckdev-ck daemon; ckdev-ck module list"
    return
  fi
  (rig_env_exec nohup "$BIN/ckdev-subc") >> "$log" 2>&1 &
  pid=$!
  printf '%s\n' "$pid" > "$PIDFILE"
  waited=0
  while [ "$(conn_pid)" != "$pid" ]; do
    kill -0 "$pid" 2>/dev/null || { tail -n 20 "$log" >&2; die "the daemon exited at startup (log: $log)"; }
    [ "$waited" -lt 30 ] || die "no connection file naming pid $pid after 30s (log: $log)"
    sleep 1
    waited=$((waited + 1))
  done
  check_daemon_identity "$pid"
  # Give the supervisor a moment to spawn and register every module.
  sleep 10
  rig_ck module list
  say "started (pid $pid, daemon output: $log); check it with: $0 status"
}

# prefrontal-core runs git constantly, and the rig's PATH is the caller's with
# CortexKit directories removed, so refuse to start unless /usr/bin is still
# on it and git resolves through it. Read-only, so a dry run performs it too.
check_rig_tools() {
  path=$(rig_path)
  case ":$path:" in
    *:/usr/bin:*) ;;
    *) die "refusing: /usr/bin is not on the rig's PATH ($path)" ;;
  esac
  git=$(PATH=$path; command -v git) \
    || die "refusing: git is not reachable through the rig's PATH ($path)"
  version=$(PATH=$path; git --version) || die "refusing: $git does not run"
  say "rig PATH reaches /usr/bin and $git ($version)"
}

# The daemon answering on the rig's connection file is the one this script
# started: ck's reported pid, the connection file's pid and the pidfile agree.
check_daemon_identity() {
  expected=$1
  guard_port_ours "$expected"
  [ "$(conn_pid)" = "$expected" ] || die "$CONN names pid $(conn_pid), not the rig daemon $expected"
  out=$(rig_ck daemon)
  printf '%s\n' "$out"
  answered=$(printf '%s\n' "$out" | sed -n 's/^daemon .* · pid \([0-9][0-9]*\) · .*/\1/p')
  [ "$answered" = "$expected" ] || die "ck reached daemon pid '$answered', not the rig daemon $expected"
}

# ---------------------------------------------------------------- status

cmd_status() {
  [ $# -eq 0 ] || usage
  if [ "$DRY" = 1 ]; then
    say "+ read $PIDFILE; check $CONN and port $PORT name that pid"
    say "+ rig_env $BIN/ckdev-ck daemon; rig_env $BIN/ckdev-ck module list"
    say "+ per module: ps (pid by program path), codesign -dv (identifier),"
    say "  lsof -d txt (running inode vs placed file), lsof -Ftn (open regular files),"
    say "  failing on any store outside $CONFIG_HOME, $DATA_HOME or $RUNTIME_DIR,"
    say "  and on any other open file under $HOME that is outside $RIG"
    say "+ status includes Fusiform's pid, ckdev-fusiform signature and rig-only store"
    rig_auth list
    return
  fi
  pid=$(daemon_pid)
  if [ -z "$pid" ]; then
    say "daemon: not running"
    guard_port_ours ""
    exit 3
  fi
  check_daemon_identity "$pid"
  rig_ck module list
  failed=0
  processes=$(ps -axo pid=,ppid=,command=)
  say ""
  # report_process runs in a subshell: sh has no local variables, and its own
  # assignments (pid among them) would otherwise overwrite this function's.
  (report_process subc "$BIN/ckdev-subc" "$pid" ckdev-subc) || failed=1
  for line in $(modules | tr '\t' '|'); do
    id=${line%%|*}
    file=${line#*|}
    child=$(printf '%s\n' "$processes" | awk -v parent="$pid" -v prog="$BIN/$file" \
      '$2 == parent && $3 == prog { print $1; exit }')
    want="ckdev-${file#ckdev-}"
    [ "$id" != basal ] || want=$(basal_identifier ck-basal)
    (report_process "$id" "$BIN/$file" "$child" "$want") || failed=1
  done
  basal=$(printf '%s\n' "$processes" | awk -v parent="$pid" -v prog="$BIN/ckdev-basal" \
    '$2 == parent && $3 == prog { print $1; exit }')
  workers=""
  if [ -n "$basal" ]; then
    workers=$(printf '%s\n' "$processes" | awk -v parent="$basal" -v prog="$BIN/ck-basal-worker" \
      '$2 == parent && $3 == prog { print $1 }')
  fi
  if [ -z "$workers" ]; then
    say "basal-worker: none running"
  fi
  for worker in $workers; do
    (report_process basal-worker "$BIN/ck-basal-worker" "$worker" "$(basal_identifier ck-basal-worker)") || failed=1
  done
  report_credentials
  [ "$failed" = 0 ] || die "status found a module outside the rig (see above)"
}

# One module's row: running, pid, identifier, whether the running image is
# the placed file, every store the process has open, and any open regular
# file under the user's real home that is not inside the rig root. Returns
# non-zero for a store outside the rig's three XDG homes (config/, data/ and
# runtime/ under the rig root) or for any such file outside the rig root.
report_process() {
  name=$1
  placed=$2
  pid=$3
  want_identifier=$4
  identifier=$(identifier_of "$placed")
  if [ -z "$pid" ]; then
    say "$name: not running · identifier $identifier"
    return 0
  fi
  placed_inode=$(stat -f %i "$placed")
  running_inode=$(lsof -nP -a -p "$pid" -d txt -Fin 2>/dev/null | awk -v prog="$placed" '
    /^i/ { inode = substr($0, 2) }
    /^n/ { if (substr($0, 2) == prog) { print inode; exit } }')
  if [ "$running_inode" = "$placed_inode" ]; then
    image="running image is the placed file (inode $placed_inode)"
  else
    image="running image is NOT the placed file (running inode ${running_inode:-unknown}, placed $placed_inode)"
  fi
  [ "$identifier" = "$want_identifier" ] || image="$image; identifier should be $want_identifier"
  say "$name: running · pid $pid · identifier $identifier · $image"
  bad=0
  # Every open file as "<lsof type><tab><path>"; lsof prints a file's type
  # field before its name field.
  opened=$(lsof -nP -a -p "$pid" -Ftn 2>/dev/null | awk '
    /^t/ { type = substr($0, 2) }
    /^n\// { print type "\t" substr($0, 2) }' | sort -u)
  tab=$(printf '\t')
  stores=0
  while IFS="$tab" read -r type path; do
    [ "$type" = REG ] || continue
    case "$path" in
      *.db | *.sqlite)
        # A store must lie inside the rig's XDG homes.
        stores=$((stores + 1))
        verdict=$(store_verdict "$path")
        say "  store: $path ($verdict)"
        case "$verdict" in inside*) ;; *) bad=1 ;; esac
        continue ;;
    esac
    # Any other file under the user's real home must lie inside the rig root.
    # The placed binary is inside it too; it is named here so the rule does
    # not depend on where bin/ is.
    [ "$path" != "$placed" ] || continue
    case "$path" in
      "$RIG"/*) ;;
      "$HOME"/*)
        say "  open file: $path ($(store_verdict "$path"))"
        bad=1 ;;
    esac
  done <<EOF
$opened
EOF
  [ "$stores" -gt 0 ] || say "  store: none open"
  return "$bad"
}

store_verdict() {
  case "$1" in
    "$CK_SHARE"/prefrontal-core/*) say "PRODUCTION prefrontal-core store" ;;
    "$DATA_HOME"/* | "$CONFIG_HOME"/* | "$RUNTIME_DIR"/*) say "inside the rig's homes" ;;
    "$RIG"/*) say "OUTSIDE the rig's homes, inside the rig root" ;;
    "$CK_SHARE"/* | "$CK_CONFIG"/*) say "PRODUCTION CortexKit path" ;;
    "$HOME"/*) say "OUTSIDE the rig, in the user's real home" ;;
    *) say "OUTSIDE the rig" ;;
  esac
}

# The vault's credential inventory. `list` with an explicit data directory
# reads only the unencrypted metadata, so it needs neither the master key nor
# the running vault. Only the isolated OpenAI key may be deposited here.
report_credentials() {
  listing=$(rig_auth list 2>&1) || {
    say "credentials: cannot read the rig vault: $listing"
    return 0
  }
  count=$(printf '%s\n' "$listing" | awk '
    /^STATE +VER +CREDENTIAL/ { rows = 1; next }
    rows && /^$/ { exit }
    rows { n++ }
    END { print n + 0 }')
  if [ "$count" = 0 ]; then
    say "credentials: no provider credential present (vault $VAULT_DIR)"
  else
    say "credentials: $count present in the rig vault:"
    printf '%s\n' "$listing" | sed 's/^/  /'
  fi
}

# ---------------------------------------------------------------- stop

cmd_stop() {
  [ $# -eq 0 ] || usage
  pid=$(daemon_pid)
  if [ "$DRY" = 1 ]; then
    say "+ check $CONN and port $PORT name pid ${pid:-<from $PIDFILE>}; kill -TERM it; wait up to 60s"
    say "+ check no process runs a program from $BIN"
    say "+ rm -rf $RUNTIME_DIR/*   (connection file, start lock, pidfile, tmp)"
    return
  fi
  if [ -n "$pid" ]; then
    command=$(ps -o command= -p "$pid" | awk '{ print $1 }')
    [ "$command" = "$BIN/ckdev-subc" ] || die "pid $pid in $PIDFILE runs $command, not the rig daemon"
    guard_port_ours "$pid"
    if [ -f "$CONN" ]; then
      [ "$(conn_pid)" = "$pid" ] || die "$CONN names pid $(conn_pid), not $pid"
    fi
    kill -TERM "$pid"
    waited=0
    while kill -0 "$pid" 2>/dev/null; do
      [ "$waited" -lt 60 ] || die "the rig daemon (pid $pid) is still running 60s after SIGTERM"
      sleep 1
      waited=$((waited + 1))
    done
    say "stopped the rig daemon (pid $pid)"
  else
    say "the rig daemon is not running"
  fi
  left=$(ps -axo pid=,command= | awk -v bin="$BIN/" 'index($2, bin) == 1 { print }')
  [ -z "$left" ] || { printf '%s\n' "$left" >&2; die "rig processes are still running"; }
  guard_path "$RUNTIME_DIR"
  if [ -d "$RUNTIME_DIR" ]; then
    find "$RUNTIME_DIR" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
  fi
  say "runtime directory cleared"
}

# ---------------------------------------------------------------- manifest

cmd_manifest() {
  [ $# -eq 0 ] || usage
  write_manifest "$(date -u +%Y%m%dT%H%M%SZ)"
}

# write_manifest <stamp>: write results/<stamp>/stack.json, the record of
# which commit of each repository and which binaries a rig result tested.
write_manifest() {
  stamp=$1
  dest="$RESULTS/$stamp/stack.json"
  if [ "$DRY" = 1 ] && { [ ! -f "$STACK" ] || ! all_placed; }; then
    say "+ read the built commits from $STACK and check each clone in $SRC is still at its commit"
    say "+ shasum -a 256 and codesign -dv each binary in $BIN"
    say "+ write $dest (rig, root, port, repositories[name, source, requested, commit],"
    say "  binaries[name, repository, path, sha256, identifier], basal_features,"
    say "  basal_placement[mode, stage, revision], contract_suite[path, sha256])"
    return
  fi
  [ -f "$STACK" ] || die "nothing built yet; run build first"
  # Each repository's commit is read again from its clone, so the manifest
  # records what is checked out there now, and must agree with the build.
  repos_tsv=""
  for line in $(tail -n +2 "$STACK" | tr '\t' '|'); do
    name=$(printf '%s' "$line" | cut -d'|' -f1)
    commit=$(printf '%s' "$line" | cut -d'|' -f4)
    head=$(git -C "$SRC/$name" rev-parse HEAD)
    [ "$head" = "$commit" ] || die "$name: the clone is at $head, but $commit was built"
    lock=$(printf '%s' "$line" | cut -d'|' -f5)
    repos_tsv="$repos_tsv$(printf '%s' "$line" | cut -d'|' -f1-4 | tr '|' '\t')	${lock:-committed}
"
  done
  bins_tsv=""
  for line in $(binaries | tr '\t' '|'); do
    name=$(printf '%s' "$line" | cut -d'|' -f1)
    repo=$(printf '%s' "$line" | cut -d'|' -f2)
    file=$(printf '%s' "$line" | cut -d'|' -f4)
    path="$BIN/$file"
    [ -f "$path" ] || die "$path is not placed"
    sum=$(shasum -a 256 "$path" | awk '{ print $1 }')
    bins_tsv="$bins_tsv$name	$repo	$path	$sum	$(identifier_of "$path")
"
  done
  contract_sum=""
  if [ -f "$CONTRACT" ]; then
    contract_sum=$(shasum -a 256 "$CONTRACT" | awk '{ print $1 }')
  fi
  source_tsv="rig-build"
  [ ! -f "$BASAL_SOURCE" ] || source_tsv=$(cat "$BASAL_SOURCE")
  json=$(REPOS="$repos_tsv" BINS="$bins_tsv" python3 - "$stamp" "$RIG" "$PORT" \
      "$CONTRACT" "$contract_sum" "$source_tsv" <<'PY'
import json, os, sys
stamp, rig, port = sys.argv[1], sys.argv[2], int(sys.argv[3])
contract, contract_sum = sys.argv[4], sys.argv[5]
source = (sys.argv[6].split("\t") + ["", ""])[:3]
staged = source[0] == "staged"
rows = lambda name: [l.split("\t") for l in os.environ[name].splitlines() if l]
print(json.dumps({
    "rig": "ckdev-flows",
    "root": rig,
    "port": port,
    "written_at": stamp,
    "repositories": [
        # cargo_lock is "committed", or "sibling-refreshed" when the build
        # ran with path dependencies at the rig's sibling revision instead
        # of the versions the commit's Cargo.lock records.
        {"name": n, "source": s, "requested": r, "commit": c, "cargo_lock": l}
        for n, s, r, c, l in rows("REPOS")
    ],
    "binaries": [
        {"name": n, "repository": r, "path": p, "sha256": h, "identifier": i}
        for n, r, p, h, i in rows("BINS")
    ],
    # cmd_build compiles the rig's ck-basal with this feature (the crash
    # case's kill switch); production builds never do, so a staged ck-basal
    # has none.
    "basal_features": [] if staged else ["rig-kill-hook"],
    # Where ck-basal and ck-basal-worker came from: a stage from
    # script/stage.sh (the production bytes, so the crash case is not run),
    # or the rig's own build with the kill switch.
    "basal_placement": (
        {"mode": "staged", "stage": source[1], "revision": source[2]}
        if staged
        else {"mode": "rig-build-with-kill-hook"}
    ),
    "contract_suite": {"path": contract, "sha256": contract_sum or None},
}, indent=2))
PY
)
  printf '%s\n' "$json" | write_file "$dest"
  [ "$DRY" = 1 ] || say "wrote $dest"
}

# ---------------------------------------------------------------- test

# Run the live contract suite (crates/basal-rig) against the rig: start it if
# it is not running, write results/<stamp>/stack.json, run the suite with its
# output in contract.log and its results in contract.json beside it, and stop
# the rig again only if this command started it.
cmd_test() {
  models=0
  if [ $# -eq 1 ] && [ "$1" = --models ]; then models=1; else [ $# -eq 0 ] || usage; fi
  stamp=$(date -u +%Y%m%dT%H%M%SZ)
  dir="$RESULTS/$stamp"
  if [ "$DRY" = 1 ]; then
    say "+ if the rig daemon is not running: $0 start, and $0 stop after the suite"
    say "+ write $dir/stack.json, as manifest does"
    say "+ register the run's project in the rig's entorhinal (ensure_project)"
    say "+ rig_env $CONTRACT --core-store $CORE_STORE --basal-store $BASAL_STORE"
    say "    --machine-id $MACHINE_ID --kill-file $KILL_FILE --project-id <the project>"
    say "    --results $dir/contract.json"
    say "    --broca-index $BROCA_INDEX"
    if [ "$models" = 1 ]; then
      rig_auth list
      say "    --models (after requiring exactly one active OpenAI credential)"
    else
      say "    (all seven model cases reported as not run by name)"
    fi
    say "    [--no-kill-hook, only when place --from-stage placed a staged ck-basal]"
    say "  (output into $dir/contract.log)"
    return
  fi
  [ -x "$CONTRACT" ] || die "no contract suite at $CONTRACT; run build first"
  started=0
  pid=$(daemon_pid)
  if [ -z "$pid" ]; then
    cmd_start
    started=1
  else
    check_daemon_identity "$pid"
  fi
  project_id=$(ensure_project "$stamp")
  write_manifest "$stamp"
  guard_path "$dir/contract.log"
  guard_path "$dir/contract.json"
  # A staged ck-basal is a production build with no kill switch, so the
  # crash case cannot run; the suite then reports it as not run. This is the
  # only way the flag is ever passed: a rig build always runs the case.
  if [ "$(basal_mode)" = staged ]; then
    set -- --no-kill-hook
    say "ck-basal is a staged production build without the kill switch; the crash case will be reported as not run"
  else
    set --
  fi
  if [ "$models" = 1 ]; then
    # A failed prerequisite must not leave a daemon this command started.
    if ! require_model_credential; then
      [ "$started" = 0 ] || cmd_stop
      die "the rig credential prerequisite failed"
    fi
    set -- "$@" --models
  fi
  say "running the contract suite (output: $dir/contract.log)"
  set +e
  rig_env "$CONTRACT" --core-store "$CORE_STORE" --basal-store "$BASAL_STORE" \
    --machine-id "$MACHINE_ID" --kill-file "$KILL_FILE" --project-id "$project_id" \
    --results "$dir/contract.json" --broca-index "$BROCA_INDEX" "$@" > "$dir/contract.log" 2>&1
  status=$?
  set -e
  cat "$dir/contract.log"
  rm -f "$KILL_FILE"
  if [ "$started" = 1 ]; then
    cmd_stop
  fi
  [ "$status" = 0 ] || die "the contract suite failed (exit $status); results in $dir"
  say "the contract suite passed; results in $dir"
}

# ensure_project <stamp>: create the run's project in the rig's entorhinal,
# so a fresh rig needs no manual step. The suite registers a new head agent
# on every run, and core refuses a second head for a project
# (agent_project_taken), so every run gets its own project: a git repository
# under the rig's home, registered as
# basal-rig-<stamp> and assigned to the workspace basal-rig, which is what
# core needs to resolve a head's project to a workspace. Prints the project
# id. Every call goes through the rig's ck, against the rig's connection
# file; ck finds its `projects` and `workspaces` domains as ck-projects and
# ck-workspaces on PATH, which are entorhinal's binary under those names, so
# they are linked into a directory of the rig's own.
ensure_project() {
  project_name="basal-rig-$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  project_root="$PROJECTS/$project_name"
  faces="$RIG/build/faces"
  guard_path "$faces"
  guard_path "$project_root"
  mkdir -p "$faces" "$project_root"
  ln -sf "$BIN/ckdev-entorhinal" "$faces/ck-projects"
  ln -sf "$BIN/ckdev-entorhinal" "$faces/ck-workspaces"
  [ -d "$project_root/.git" ] || git -C "$project_root" init --quiet
  project_id=$(rig_projects projects resolve "$project_root" --json | project_field registered)
  if [ -z "$project_id" ]; then
    rig_projects projects register "$project_name" "$project_root" >&2
    project_id=$(rig_projects projects resolve "$project_root" --json | project_field registered)
    [ -n "$project_id" ] || die "entorhinal did not register $project_root"
  fi
  workspace=$(rig_projects projects resolve "$project_root" --json | project_field workspace)
  if [ "$workspace" != basal-rig ]; then
    rig_projects workspaces assign "$project_id" basal-rig >&2
  fi
  printf '%s\n' "$project_id"
}

# The rig's ck with entorhinal's operator faces on PATH.
rig_projects() {
  rig_env PATH="$RIG/build/faces:$(rig_path)" "$BIN/ckdev-ck" "$@"
}

# From `ck projects resolve --json` on stdin: `registered` prints the project
# id if the root is registered (not an implicit, derived one), `workspace`
# its workspace id.
project_field() {
  python3 -c 'import json, sys
r = json.load(sys.stdin)
if sys.argv[1] == "registered":
    print(r["projectId"] if r.get("projectName") and not r.get("gone") else "")
else:
    print(r.get("workspaceId") or "")' "$1"
}

# ---------------------------------------------------------------- main

[ $# -ge 1 ] || usage
command=$1
shift
# Rotate the arguments instead of flattening them: a payload path may contain
# spaces, and only the auth CLI is allowed to open that file.
remaining=$#
while [ "$remaining" -gt 0 ]; do
  arg=$1; shift
  if [ "$arg" = --dry-run ]; then DRY=1; else set -- "$@" "$arg"; fi
  remaining=$((remaining - 1))
done
guard_all_paths
case "$command" in
  build) cmd_build "$@" ;;
  place) cmd_place "$@" ;;
  config) cmd_config "$@" ;;
  start) cmd_start "$@" ;;
  status) cmd_status "$@" ;;
  stop) cmd_stop "$@" ;;
  manifest) cmd_manifest "$@" ;;
  credential) cmd_credential "$@" ;;
  test) cmd_test "$@" ;;
  *) usage ;;
esac
