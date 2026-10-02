# shellcheck shell=sh
# signing.sh: basal's one signing policy, sourced (not run) by
# script/stage.sh, script/flows-rig.sh and script/sign-worker.sh.
#
# Every basal binary, and every binary the ckdev-flows rig places, is signed
# ad hoc with the hardened runtime, an explicit identifier and no
# entitlements. The hardened runtime is what refuses a same-user debugger,
# which could otherwise read the module's launch secret and whatever
# credentials it holds. QuickJS is an interpreter, so the worker needs no JIT
# entitlement; nothing else needs one either. get-task-allow is never
# allowed: it reopens debugger attach whatever the runtime flag says.
#
# Functions return non-zero with a message on stderr instead of exiting,
# because they run inside the caller's shell. For the same reason every
# variable they set starts with sig_: sh has no local variables, and a
# plain name would overwrite one of the caller's.

# sign_hardened FILE IDENTIFIER
# Always pass the identifier: codesign otherwise derives one from the file
# name, and a renamed or temporary copy would change the identity that macOS
# privacy grants (and, later, a team signature) are bound to.
sign_hardened() {
  codesign --force --sign - --options runtime --identifier "$2" "$1"
}

# codesign_flags FILE: the CodeDirectory flags, e.g. 0x10002(adhoc,runtime).
codesign_flags() {
  codesign -dv "$1" 2>&1 | sed -n 's/^CodeDirectory .*flags=\([^ ]*\).*/\1/p'
}

# codesign_identifier FILE
codesign_identifier() {
  codesign -dv "$1" 2>&1 | sed -n 's/^Identifier=//p'
}

# verify_hardened FILE IDENTIFIER
# The signature verifies strictly, the identifier is exactly IDENTIFIER, the
# flags carry runtime, and the binary has no entitlements at all.
verify_hardened() {
  codesign --verify --strict "$1" 2>/dev/null || {
    printf 'REFUSED: %s: the signature does not verify\n' "$1" >&2
    return 1
  }
  sig_identifier=$(codesign_identifier "$1")
  [ "$sig_identifier" = "$2" ] || {
    printf "REFUSED: %s: identifier is '%s', not %s\n" "$1" "$sig_identifier" "$2" >&2
    return 1
  }
  sig_flags=$(codesign_flags "$1")
  case "$sig_flags" in
    *\(*runtime*\)) ;;
    *)
      printf 'REFUSED: %s: flags=%s lacks runtime (the hardened runtime)\n' "$1" "${sig_flags:-<none>}" >&2
      return 1 ;;
  esac
  sig_entitlements=$(codesign -d --entitlements - --xml "$1" 2>/dev/null || true)
  [ -z "$sig_entitlements" ] || {
    printf 'REFUSED: %s: carries entitlements; basal signs with none\n' "$1" >&2
    return 1
  }
}
