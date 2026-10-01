#!/bin/sh
# Runs every mutation-control set and fails unless each one really ran.
#
# The runner refuses to start while the worktree has unstaged changes, and
# every run rewrites its own evidence file under docs/findings. Run in
# sequence without care, the second set therefore refuses and the evidence
# files keep their committed contents, which then look like a fresh result.
# This script restores the evidence files before each set, requires each
# run to exit 0 and to have rewritten its evidence file during this run, and
# counts outcomes only from those freshly written files. Any set that did
# not run, or any control that did not turn its test red, fails the script.
#
# Usage: script/verify-controls.sh [set ...]
# With no arguments it runs every set this build defines. A set is a runner
# flag without the dashes (journal, dispatch, ...); `worker` runs the runner
# with no flag. Only sets the runner's source names are accepted: the runner
# reads an unrecognised flag as a test-name filter for its default set, which
# would match nothing and look like a pass.
set -u
cd "$(dirname "$0")/.." || exit 2

if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "refusing: the worktree has uncommitted changes" >&2
    git status --short --untracked-files=no >&2
    exit 2
fi

runner=crates/basal-testkit/src/bin/mutation-controls.rs
defined="worker"
for candidate in journal dispatch schedule module broca hosts; do
    if grep -q "\"--$candidate\"" "$runner"; then
        defined="$defined $candidate"
    fi
done
if [ "$#" -eq 0 ]; then
    # shellcheck disable=SC2086
    set -- $defined
fi
for set_name in "$@"; do
    case " $defined " in
        *" $set_name "*) ;;
        *) echo "unknown control set '$set_name'; this build defines: $defined" >&2; exit 2 ;;
    esac
done

logs=$(mktemp -d "${TMPDIR:-/tmp}/basal-controls.XXXXXX")
failed=0
for set_name in "$@"; do
    if [ "$set_name" = worker ]; then flag=""; else flag="--$set_name"; fi
    git checkout -q -- docs/findings
    # $flag stays unquoted so the default set passes no argument at all.
    # shellcheck disable=SC2086
    if ! cargo run -q -p basal-testkit --bin mutation-controls -- $flag --check \
        > "$logs/$set_name.check.log" 2>&1; then
        echo "$set_name: FAILED its --check (log: $logs/$set_name.check.log)"
        failed=1
        continue
    fi
    # shellcheck disable=SC2086
    if ! cargo run -q -p basal-testkit --bin mutation-controls -- $flag \
        > "$logs/$set_name.log" 2>&1; then
        echo "$set_name: FAILED, runner exited non-zero (log: $logs/$set_name.log)"
        tail -3 "$logs/$set_name.log"
        failed=1
        continue
    fi
    evidence=$(git diff --name-only -- 'docs/findings/*mutations*.json')
    if [ -z "$evidence" ]; then
        echo "$set_name: FAILED, the run wrote no fresh evidence (log: $logs/$set_name.log)"
        failed=1
        continue
    fi
    for file in $evidence; do
        if ! python3 - "$set_name" "$file" <<'EOF'
import json, sys
from collections import Counter
name, path = sys.argv[1], sys.argv[2]
data = json.load(open(path))
controls = data if isinstance(data, list) else data.get("controls", data)
counts = Counter(c.get("outcome") for c in controls)
print(f"{name}: {path}: {len(controls)} controls, {dict(counts)}")
sys.exit(0 if controls and set(counts) == {"reddened"} else 1)
EOF
        then
            echo "$set_name: FAILED, a control did not turn its test red"
            failed=1
        fi
    done
done
git checkout -q -- docs/findings
if [ "$failed" -ne 0 ]; then
    echo "FAILED (logs in $logs)"
    exit 1
fi
echo "every control set ran and every control turned its test red"
