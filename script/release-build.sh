#!/usr/bin/env bash
# Build basal's binaries with their provenance stamped in.
# Both executables must identify the same clean source revision. A package
# version alone cannot distinguish builds made from different commits.
set -euo pipefail

cd "$(dirname "$0")/.."

say() { printf '%s\n' "$*"; }
die() { printf 'release-build.sh: %s\n' "$*" >&2; exit 1; }
# shellcheck source=script/check-release-binaries.sh
. ./script/check-release-binaries.sh

REV="$(git rev-parse HEAD)"
[[ "$REV" == "${RELEASE_REVISION:-$REV}" ]] || die "expected $RELEASE_REVISION, checked out $REV"

# Refuse a dirty tree rather than claim that its bytes were built from HEAD.
dirty=$(git status --porcelain --untracked-files=all)
if [ -n "$dirty" ]; then
  echo "refusing to stamp a dirty tree: the built bytes would not match $REV" >&2
  echo "commit or stash first, or use a plain cargo build for a dev binary" >&2
  exit 1
fi

echo "building at $REV"
CK_BUILD_GIT_SHA="$REV" CK_BUILD_GIT_DIRTY=false CARGO_TARGET_DIR=target cargo build --locked --release \
  -p basal-module --bin ck-basal -p basal-worker --bin ck-basal-worker

after_rev=$(git rev-parse HEAD)
dirty=$(git status --porcelain --untracked-files=all)
if [[ "$after_rev" != "$REV" || -n "$dirty" ]]; then
  echo "refusing to report provenance: the checkout changed during the build (expected clean $REV; HEAD is $after_rev)" >&2
  exit 1
fi

check_release_binaries target/release

echo
# Reserve ck- process names for installed programs. Probing under a ckdev-
# alias distinguishes the probe while running exactly the bytes just built.
probe_dir="$(mktemp -d)"
# macOS /bin/bash 3.2 exits 0 when `set -eu` stops the script on an unbound
# variable while an EXIT trap is set, so an aborted build would look like a
# finished one. The trap keeps any failure status, and turns a 0 into 1 unless
# the script reached its last line and set `finished=1`.
finished=0
on_exit() {
  local rc=$?
  rm -rf "$probe_dir" || true
  if [ "$rc" -eq 0 ] && [ "$finished" -ne 1 ]; then
    echo "release-build.sh: stopped before its last step; treating this run as failed" >&2
    rc=1
  fi
  exit "$rc"
}
trap on_exit EXIT
revision_pattern='\(([0-9a-f]{40})\)'
suffix=""
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) suffix=.exe ;;
esac
paths=()
for bin in ck-basal ck-basal-worker; do
  built="target/release/$bin$suffix"
  paths+=("$built")
  link="$probe_dir/ckdev-${bin#ck-}$suffix"
  ln "$built" "$link" 2>/dev/null || cp "$built" "$link"
  if ! version=$("$link" --version); then
    echo "release-build.sh: $bin version probe failed" >&2
    exit 1
  fi
  if [[ ! "$version" =~ $revision_pattern || "${BASH_REMATCH[1]}" != "$REV" ]]; then
    echo "release-build.sh: $bin does not report revision $REV: ${version:-<no version output>}" >&2
    exit 1
  fi
  printf '%-12s %s\n' "$bin" "$version"
done

echo
echo "sha256:"
# `shasum` is available on macOS; the other runners also have `sha256sum`.
if command -v shasum >/dev/null 2>&1; then
  shasum -a 256 "${paths[@]}" \
    | sed 's|target/release/||'
elif command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${paths[@]}" \
    | sed 's|target/release/||'
else
  # Refuse rather than print nothing: a provenance block that silently omits
  # the hashes is worse than a failure, because the omission is invisible in
  # the output someone copies.
  echo "no sha256 tool found (looked for shasum, sha256sum)" >&2
  exit 1
fi
finished=1
