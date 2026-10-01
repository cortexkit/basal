#!/bin/sh
# flows-rig.sh: build, place and run basal's isolated test rig, ckdev-flows.
#
# The rig is a private subc daemon with its own port, its own XDG homes under
# ~/.local/share/cortexkit/ckdev-flows/ and its own ckdev-* binaries, every
# one built here from a named revision. It runs the credentials vault (empty),
# Broca, prefrontal-core, prefrontal-routing and ck-basal. It never reads,
# writes or starts anything belonging to the production daemon or to another
# rig. docs/rig.md describes it.
#
# Usage:
#   script/flows-rig.sh build --prefrontal-rev <rev> [--subc-rev <rev>]
#                             [--broca-rev <rev>] [--credentials-rev <rev>]
#                             [--commons-rev <rev>] [--dry-run]
#   script/flows-rig.sh place    [--dry-run]
#   script/flows-rig.sh config   [--dry-run]
#   script/flows-rig.sh start    [--dry-run]
#   script/flows-rig.sh status   [--dry-run]
#   script/flows-rig.sh stop     [--dry-run]
#   script/flows-rig.sh manifest [--dry-run]
#
# --dry-run prints every command the subcommand would run and every file it
# would write, with the file's content, and changes nothing.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
WORKSPACE=${CORTEXKIT_WORKSPACE:-$HOME/Work/Projects/CortexKit}
CK_SHARE="$HOME/.local/share/cortexkit"
CK_CONFIG="$HOME/.config/cortexkit"
RIG="$CK_SHARE/ckdev-flows"

BIN="$RIG/bin"
SRC="$RIG/src"
TARGETS="$RIG/build/target"
STACK="$RIG/build/stack.tsv"
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
# The vault's data directory is where the daemon's storage convention puts
# module `claustrum`. Its master key must live outside that directory (the
# vault refuses a key beside its own store), so it sits in the config home.
VAULT_DIR="$DATA_HOME/cortexkit/claustrum"
VAULT_KEY="$CONFIG_HOME/claustrum/master.key"
# Clear of production's daemon (8757) and of the older isolation rig under
# ckdev-rig/ (8799, plus 8377 and 8378 for one of its modules).
PORT=8791

DRY=0

say() { printf '%s\n' "$*"; }
die() { printf 'flows-rig: %s\n' "$*" >&2; exit 1; }

usage() {
  sed -n '12,23p' "$0" | sed 's/^# \{0,1\}//' >&2
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
  for path in "$BIN" "$SRC" "$TARGETS" "$STACK" "$LOGS" "$RESULTS" \
      "$CONFIG_HOME" "$DATA_HOME" "$RUNTIME_DIR" "$CONN" "$PIDFILE" \
      "$SUBC_CONFIG" "$VAULT_DIR" "$VAULT_KEY" "$RIG_HOME"; do
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

conn_pid() {
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pid"])' "$CONN" 2>/dev/null || true
}

identifier_of() {
  codesign -dv "$1" 2>&1 | sed -n 's/^Identifier=//p'
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
prefrontal-core	prefrontal	ck-prefrontal-core	ckdev-prefrontal-core
prefrontal-routing	prefrontal	ck-prefrontal-routing	ckdev-prefrontal-routing
basal	basal	ck-basal	ckdev-basal
basal-worker	basal	ck-basal-worker	ck-basal-worker
EOF
}

# The supervised modules: subc module id and placed file name.
modules() {
  cat <<'EOF'
claustrum	ckdev-claustrum
broca	ckdev-broca
prefrontal-core	ckdev-prefrontal-core
prefrontal-routing	ckdev-prefrontal-routing
basal	ckdev-basal
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
  while [ $# -gt 0 ]; do
    case "$1" in
      --prefrontal-rev) [ $# -ge 2 ] || usage; prefrontal_rev=$2; shift 2 ;;
      --subc-rev) [ $# -ge 2 ] || usage; subc_rev=$2; shift 2 ;;
      --broca-rev) [ $# -ge 2 ] || usage; broca_rev=$2; shift 2 ;;
      --credentials-rev) [ $# -ge 2 ] || usage; credentials_rev=$2; shift 2 ;;
      --commons-rev) [ $# -ge 2 ] || usage; commons_rev=$2; shift 2 ;;
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

  stack=""
  for line in $(repos | tr '\t' '|'); do
    name=${line%%|*}
    source=${line#*|}
    case "$name" in
      subconscious) rev=$subc_rev ;;
      commons) rev=$commons_rev ;;
      broca) rev=$broca_rev ;;
      claustrum) rev=$credentials_rev ;;
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
  prefrontal_sha=$(printf '%s' "$stack" | awk -F'\t' '$1=="prefrontal"{print $4}')
  # prefrontal's build scripts embed the revision they were told; the clone
  # is at that exact commit with no local changes, so it is not dirty.
  cargo_build prefrontal "CK_BUILD_GIT_SHA=$prefrontal_sha CK_BUILD_GIT_DIRTY=false" \
    -p prefrontal-core-module --bin ck-prefrontal-core
  cargo_build prefrontal "CK_BUILD_GIT_SHA=$prefrontal_sha CK_BUILD_GIT_DIRTY=false" \
    -p prefrontal-routing-module --bin ck-prefrontal-routing
  cargo_build basal "" -p basal-module --bin ck-basal
  cargo_build basal "" -p basal-worker --bin ck-basal-worker

  # A build that rewrote a tracked file (a Cargo.lock refreshed against a
  # sibling at another revision) did not build exactly the named commit.
  for line in $(repos | tr '\t' '|'); do
    name=${line%%|*}
    if [ "$DRY" = 0 ] && [ -n "$(git -C "$SRC/$name" status --porcelain --untracked-files=no)" ]; then
      git -C "$SRC/$name" status --short --untracked-files=no >&2
      die "$name: the build changed tracked files in $SRC/$name"
    fi
  done
  printf 'repo\tsource\trequested\tcommit\n%s' "$stack" | write_file "$STACK"
  say "built; next: $0 place"
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
  [ $# -eq 0 ] || usage
  [ -z "$(daemon_pid)" ] || die "refusing: the rig daemon is running; stop it first"
  [ "$DRY" = 1 ] || [ -f "$STACK" ] || die "nothing built yet; run build first"
  run mkdir -p "$BIN"
  for line in $(binaries | tr '\t' '|'); do
    name=$(printf '%s' "$line" | cut -d'|' -f1)
    repo=$(printf '%s' "$line" | cut -d'|' -f2)
    cargo_bin=$(printf '%s' "$line" | cut -d'|' -f3)
    file=$(printf '%s' "$line" | cut -d'|' -f4)
    place_one "$name" "$TARGETS/$repo/release/$cargo_bin" "$BIN/$file"
  done
  # The worker runs flow code, so it must also pass basal's own gate at its
  # final path: hardened runtime, no entitlements, the checked-in Seatbelt
  # profile embedded, and a live probe that confinement denies a file read,
  # a socket connect and a program launch. The gate script comes from the
  # basal clone, so it checks against the profile of the commit that was built.
  worker="$BIN/ck-basal-worker"
  if [ "$DRY" = 1 ]; then
    say "+ BASAL_WORKER_IDENTIFIER=ckdev-basal-worker sh $SRC/basal/script/sign-worker.sh verify $worker"
  elif ! BASAL_WORKER_IDENTIFIER=ckdev-basal-worker sh "$SRC/basal/script/sign-worker.sh" verify "$worker"; then
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
    say "+ codesign --force --sign - -o runtime --identifier $identifier $BIN/.place.XXXXXX"
    say "+ codesign -dv $BIN/.place.XXXXXX  (Identifier must be exactly $identifier)"
    say "+ mv -f $BIN/.place.XXXXXX $dest"
    say "+ codesign --verify --strict $dest; codesign -dv $dest  (Identifier=$identifier)"
    return
  fi
  [ -f "$built" ] || die "$name: no built binary at $built"
  tmp=$(mktemp "$BIN/.place.XXXXXX")
  cp "$built" "$tmp"
  # mktemp creates the file 0600 and cp keeps that mode on an existing file.
  chmod 0755 "$tmp"
  # -o runtime: production modules are signed with the hardened runtime, and
  # basal's worker gate refuses a worker without it, so the rig matches both.
  codesign --force --sign - -o runtime --identifier "$identifier" "$tmp"
  got=$(identifier_of "$tmp")
  if [ "$got" != "$identifier" ]; then
    rm -f "$tmp"
    die "$name: signed identifier is '$got', not $identifier"
  fi
  mv -f "$tmp" "$dest"
  codesign --verify --strict "$dest" || die "$name: $dest does not verify"
  got=$(identifier_of "$dest")
  [ "$got" = "$identifier" ] || die "$name: placed identifier is '$got', not $identifier"
  say "placed $dest ($identifier)"
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
    // The credentials vault, empty, keyed by a master key file of the rig's
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
      "enabled": true
    },
    // The projects registry is off: this rig runs no entorhinal, and core's
    // registry consumer would otherwise dial for it on every route bind.
    // Never set PREFRONTAL_CORE_DIAGNOSTICS here: it opens core's test seams,
    // which exist for prefrontal's end-to-end harness only.
    "prefrontal-core": {
      "program": "$BIN/ckdev-prefrontal-core",
      "args": [],
      "env": { "PREFRONTAL_CORE_PROJECTS_REGISTRY": "disabled" },
      "enabled": true,
      "reserved": true,
      "launch_nonce_env": false
    },
    "prefrontal-routing": {
      "program": "$BIN/ckdev-prefrontal-routing",
      "args": [],
      "env": {},
      "enabled": true,
      "launch_nonce_env": false
    },
    // Reserved, so its routes carry the principal reserved:basal.
    "basal": {
      "program": "$BIN/ckdev-basal",
      "args": [],
      "env": {},
      "enabled": true,
      "reserved": true
    }
  }
}
EOF
  say "configured; next: $0 start"
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
    run rig_env "$BIN/ckdev-auth" bootstrap --data-dir "$VAULT_DIR" --key-path "$VAULT_KEY"
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
    say "+ rig_env $BIN/ckdev-auth list --data-dir $VAULT_DIR --key-path $VAULT_KEY"
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
    (report_process "$id" "$BIN/$file" "$child" "ckdev-${file#ckdev-}") || failed=1
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
    (report_process basal-worker "$BIN/ck-basal-worker" "$worker" ckdev-basal-worker) || failed=1
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
# the running vault. The rig is meant to hold none.
report_credentials() {
  listing=$(rig_env "$BIN/ckdev-auth" list --data-dir "$VAULT_DIR" --key-path "$VAULT_KEY" 2>&1) || {
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
  stamp=$(date -u +%Y%m%dT%H%M%SZ)
  dest="$RESULTS/$stamp/stack.json"
  if [ "$DRY" = 1 ] && { [ ! -f "$STACK" ] || ! all_placed; }; then
    say "+ read the built commits from $STACK and check each clone in $SRC is still at its commit"
    say "+ shasum -a 256 and codesign -dv each binary in $BIN"
    say "+ write $dest (rig, root, port, repositories[name, source, requested, commit],"
    say "  binaries[name, repository, path, sha256, identifier])"
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
    repos_tsv="$repos_tsv$(printf '%s' "$line" | tr '|' '\t')
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
  json=$(REPOS="$repos_tsv" BINS="$bins_tsv" python3 - "$stamp" "$RIG" "$PORT" <<'PY'
import json, os, sys
stamp, rig, port = sys.argv[1], sys.argv[2], int(sys.argv[3])
rows = lambda name: [l.split("\t") for l in os.environ[name].splitlines() if l]
print(json.dumps({
    "rig": "ckdev-flows",
    "root": rig,
    "port": port,
    "written_at": stamp,
    "repositories": [
        {"name": n, "source": s, "requested": r, "commit": c} for n, s, r, c in rows("REPOS")
    ],
    "binaries": [
        {"name": n, "repository": r, "path": p, "sha256": h, "identifier": i}
        for n, r, p, h, i in rows("BINS")
    ],
}, indent=2))
PY
)
  printf '%s\n' "$json" | write_file "$dest"
  [ "$DRY" = 1 ] || say "wrote $dest"
}

# ---------------------------------------------------------------- main

[ $# -ge 1 ] || usage
command=$1
shift
args=""
for arg in "$@"; do
  if [ "$arg" = "--dry-run" ]; then DRY=1; else args="$args $arg"; fi
done
# shellcheck disable=SC2086 # no rig argument contains whitespace
set -- $args
guard_all_paths
case "$command" in
  build) cmd_build "$@" ;;
  place) cmd_place "$@" ;;
  config) cmd_config "$@" ;;
  start) cmd_start "$@" ;;
  status) cmd_status "$@" ;;
  stop) cmd_stop "$@" ;;
  manifest) cmd_manifest "$@" ;;
  *) usage ;;
esac
