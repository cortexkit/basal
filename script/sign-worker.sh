#!/bin/sh
# Signing policy for ck-basal-worker on macOS.
#
# The worker is ad-hoc signed with the hardened runtime, an explicit
# identifier and no entitlements at all. QuickJS is an interpreter, so the
# worker needs neither allow-jit nor allow-unsigned-executable-memory, and
# its OS sandbox is the Seatbelt profile it applies to itself at startup, not
# an App Sandbox entitlement.
#
# Usage:
#   script/sign-worker.sh sign   BINARY
#   script/sign-worker.sh verify BINARY
#   script/sign-worker.sh place  BINARY DESTINATION
#
# `place` copies the binary to DESTINATION, signs it there and verifies it
# there, restoring any previous file if verification fails. It never
# restarts anything.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="$ROOT/crates/basal-worker/sandbox/worker.sb"
# The signing itself, and the signature half of the gate, are the policy every
# basal binary shares (script/signing.sh); this script adds the worker's own
# checks on top: the embedded profile and the live confinement probe.
# shellcheck source=script/signing.sh
. "$ROOT/script/signing.sh"
# BASAL_WORKER_IDENTIFIER exists for isolated rigs (script/flows-rig.sh), which
# sign their copies under their own ckdev- identifiers so a rig binary can never
# be mistaken for a production one; every other check is unchanged.
IDENTIFIER="${BASAL_WORKER_IDENTIFIER:-ck-basal-worker}"

usage() {
  echo "usage: $0 sign|verify BINARY | place BINARY DESTINATION" >&2
  exit 2
}

sign() {
  sign_hardened "$1" "$IDENTIFIER"
}

refuse() {
  echo "REFUSED: $1" >&2
  exit 1
}

verify() {
  binary=$1
  # Strict verification, the exact identifier, runtime in the flags and no
  # entitlements; it prints its own refusal.
  verify_hardened "$binary" "$IDENTIFIER" || exit 1
  # The sandbox profile compiled into the binary must be the checked-in one.
  python3 - "$binary" "$PROFILE" <<'PY' || refuse "embedded sandbox profile differs from $PROFILE"
import sys
binary = open(sys.argv[1], "rb").read()
profile = open(sys.argv[2], "rb").read()
sys.exit(0 if profile in binary else 1)
PY
  # And the signed binary really confines itself: a file read, a socket
  # connect and a program launch all fail.
  probe=$("$binary" --confinement-probe --read "$PROFILE" --connect 127.0.0.1:9 --exec /bin/echo) \
    || refuse "confinement probe failed to run"
  for attempt in read connect exec; do
    printf '%s' "$probe" | grep -q "\"$attempt\":{\"ok\":false" \
      || refuse "confinement probe: $attempt was not denied: $probe"
  done
  printf '%s' "$probe" | grep -q '"confinement":"seatbelt"' \
    || refuse "confinement probe did not report seatbelt: $probe"
  echo "signature gate passed: $binary ($IDENTIFIER)"
}

operation=${1:-}
case "$operation" in
  sign)
    [ $# -eq 2 ] || usage
    sign "$2"
    ;;
  verify)
    [ $# -eq 2 ] || usage
    verify "$2"
    ;;
  place)
    [ $# -eq 3 ] || usage
    binary=$2
    dest=$3
    mkdir -p "$(dirname "$dest")"
    scratch=$(mktemp -d "$(dirname "$dest")/.worker-placement.XXXXXX")
    installed=0
    accepted=0
    existed=0
    rollback() {
      if [ "$installed" = 1 ] && [ "$accepted" = 0 ]; then
        if [ "$existed" = 1 ]; then mv -f "$scratch/old" "$dest"; else rm -f "$dest"; fi
        echo "REFUSED: placed worker failed the gate; previous file restored" >&2
      fi
      rm -rf "$scratch"
    }
    trap rollback EXIT
    trap 'exit 1' INT TERM
    cp "$binary" "$scratch/new"
    if [ -e "$dest" ]; then cp -p "$dest" "$scratch/old"; existed=1; fi
    mv -f "$scratch/new" "$dest"
    installed=1
    # Sign at the final path and inspect that path, not a staging copy.
    sign "$dest"
    verify "$dest"
    accepted=1
    ;;
  *)
    usage
    ;;
esac
