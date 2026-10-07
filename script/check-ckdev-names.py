#!/usr/bin/env python3
"""Refuse production executable names at test and smoke-launch boundaries.

This is a source fence, not a shell/Rust interpreter. It checks Cargo executable
references, literal executable/path constructors in test Rust, shell launch
positions (including the stage loop), worker verification, and rig placement
tables. Signing identifiers, build inputs, logs and placement cards are not
executable paths. The production pool and the copy helper own those names.
"""

import argparse
from pathlib import Path
import re
import sys


CARGO = re.compile(r"CARGO_BIN_EXE_ck-[\w-]+")
RUST_PATH = re.compile(
    r'(?:Command::new|DisclaimedCommand::new|Path::new|PathBuf::from|\.join)\s*\(\s*"([^"\n]*\bck-[\w-]+)"'
)
SHELL_LAUNCH = re.compile(
    r'(?:\$\(\s*|^\s*(?:(?:run|rig_env|exec|nohup|debugger_probe|hold)\s+)*)'
    r'["\']?([^\s"\']*/ck-[\w-]+|\$(?:SCRATCH|STAGE_DIR|BIN)/\$(?:bin|file))(?=[\s"\'])'
)
WORKER_VERIFY = re.compile(r'\bverify\s+["\']?([^\s"\']*/ck-basal-worker)(?=[\s"\'])')
RIG_PLACEMENT = re.compile(r"^\S+\t\S+\t\S+\t(ck-[\w-]+)$")
PYTHON_LAUNCH = re.compile(
    r'(?:subprocess\.(?:run|Popen|call|check_call|check_output))\s*\(\s*\[\s*["\']([^"\']*\bck-[\w-]+)["\']'
)


def placed(path):
    return "/.local/share/cortexkit/bin/ck-" in path


def checked_files(root):
    for path in sorted((root / "crates").rglob("*.rs")):
        relative = path.relative_to(root).as_posix()
        if relative == "crates/basal-testkit/src/binaries.rs":
            continue
        if "/tests/" in relative or "/basal-testkit/" in relative or relative.endswith("/harness.rs"):
            yield path
    for path in sorted((root / "script").rglob("*")):
        if path.suffix in (".sh", ".py") and path.name not in ("check-ckdev-names.py", "check_ckdev_names.py"):
            yield path


def violations(path, text):
    # Exclude comments before inspecting executable expressions, so prose
    # explaining the production name does not become an exception to the fence.
    lines = text.splitlines()
    cleaned = "\n".join("" if line.lstrip().startswith(("//", "#")) else line for line in lines)
    if path.suffix == ".rs":
        for match in CARGO.finditer(cleaned):
            start = cleaned.rfind(";", 0, match.start()) + 1
            end = cleaned.find(";", match.end())
            statement = cleaned[start:end if end != -1 else len(cleaned)]
            if not re.search(r"\bdev_binary\s*\([^;]*" + re.escape(match.group()), statement):
                yield cleaned.count("\n", 0, match.start()) + 1, "Cargo executable bypasses dev_binary"
        for match in RUST_PATH.finditer(cleaned):
            start = cleaned.rfind(";", 0, match.start()) + 1
            prefix = cleaned[start:match.start()]
            if not placed(match.group(1)) and not re.search(r"\bdev_binary\s*\([^;]*$", prefix):
                yield cleaned.count("\n", 0, match.start()) + 1, "production-named test executable path"
    else:
        patterns = (SHELL_LAUNCH, WORKER_VERIFY, RIG_PLACEMENT) if path.suffix == ".sh" else (PYTHON_LAUNCH,)
        for number, line in enumerate(cleaned.splitlines(), 1):
            for pattern in patterns:
                for match in pattern.finditer(line):
                    if not placed(match.group(1)):
                        yield number, "production-named script executable path"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    count = 0
    failures = 0
    for path in checked_files(args.root):
        count += 1
        for line, reason in violations(path, path.read_text()):
            failures += 1
            print(f"{path.relative_to(args.root)}:{line}: {reason}; use a ckdev- scratch copy", file=sys.stderr)
    print(f"ckdev name check: {count} files checked, {failures} violations")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
