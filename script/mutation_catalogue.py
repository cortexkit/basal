#!/usr/bin/env python3
"""Load the ckdev-mutate catalogue as the runner does, from either of its forms.

The catalogue is a single `mutations.toml` or a `mutations.d/` directory of
fragments at the repository root. With no explicit path, the directory is used
when it exists, and having both is an error. In a directory, only visible
`*.toml` files directly inside it are read, in filename byte order. Every row
keeps the repository-relative file it came from and its `id` line, so a
failing check can name where to fix the row.

Run as a script, it compares the working tree's catalogue with the one at a Git
revision field for field. Use it to prove that moving rows between files, or
between the two forms, changed no row.

Uses Python 3.11+ for the standard-library TOML parser.
"""

import argparse
from dataclasses import dataclass
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
SINGLE = "mutations.toml"
DIRECTORY = "mutations.d"


@dataclass(frozen=True)
class Source:
    """Where a row is defined: a repository-relative file and its `id` line."""

    file: str
    line: int

    def __str__(self):
        return f"{self.file}:{self.line}" if self.line else self.file


def fragment_names(names):
    """Keep visible `*.toml` names in the byte order the runner reads them."""
    return sorted((name for name in names if name.endswith(".toml") and not name.startswith(".")),
                  key=os.fsencode)


def id_lines(text):
    """Map each row id to the line of its `id = ...` key (1-based)."""
    lines = {}
    for number, line in enumerate(text.splitlines(), start=1):
        match = re.match(r"""\s*id\s*=\s*(?:"([^"\\]*)"|'([^']*)')\s*(?:#.*)?$""", line)
        if match:
            lines.setdefault(match[1] if match[1] is not None else match[2], number)
    return lines


def parse(fragments):
    """Parse (repository-relative name, text) pairs into rows and their sources.

    An id defined twice is refused, naming every place that defines it, as the
    runner refuses it.
    """
    rows, sources, places = [], {}, {}
    for name, text in fragments:
        try:
            document = tomllib.loads(text)
        except tomllib.TOMLDecodeError as error:
            raise ValueError(f"{name}: {error}") from error
        lines = id_lines(text)
        for row in document.get("control", []):
            identity = row.get("id")
            source = Source(name, lines.get(identity, 0))
            places.setdefault(identity, []).append(source)
            sources.setdefault(identity, source)
            rows.append(row)
    duplicates = {identity: found for identity, found in places.items() if len(found) > 1}
    if duplicates:
        raise ValueError("; ".join(f"duplicate catalogue ID {identity}: defined at "
                                   + ", ".join(map(str, found)) for identity, found in duplicates.items()))
    return rows, sources


def relative(root, path):
    try:
        return path.resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return path.as_posix()


def load(root=ROOT, catalogue=None):
    """Load the catalogue under `root`, or the explicit file or directory given.

    Returns the rows in load order and a mapping from row id to its Source.
    """
    root = Path(root)
    if catalogue is None:
        single, directory = root / SINGLE, root / DIRECTORY
        if single.exists() and directory.exists():
            raise ValueError(f"both {SINGLE} and {DIRECTORY}/ exist; keep only one catalogue form")
        catalogue = directory if directory.exists() else single
    else:
        catalogue = Path(catalogue)
        if not catalogue.is_absolute():
            catalogue = root / catalogue
    if catalogue.is_dir():
        files = [catalogue / name for name in fragment_names(entry.name for entry in catalogue.iterdir()
                                                             if entry.is_file())]
    elif catalogue.is_file():
        files = [catalogue]
    else:
        raise ValueError(f"no mutation catalogue at {relative(root, catalogue)}")
    return parse([(relative(root, path), path.read_text()) for path in files])


def load_at_revision(root, revision):
    """Load the catalogue committed at a Git revision, in whichever form it had."""
    def git(*args):
        return subprocess.run(["git", *args], cwd=root, check=True, text=True,
                               stdout=subprocess.PIPE).stdout
    names = git("ls-tree", "--name-only", f"{revision}:")
    present = set(names.splitlines())
    if SINGLE in present and DIRECTORY in present:
        raise ValueError(f"{revision}: both {SINGLE} and {DIRECTORY}/ exist")
    if DIRECTORY in present:
        # ls-tree on the directory lists only its direct entries; subtrees are
        # filtered out by type so they are ignored as the runner ignores them.
        entries = git("ls-tree", f"{revision}:{DIRECTORY}").splitlines()
        blobs = [line.split("\t", 1)[1] for line in entries if line.split()[1] == "blob"]
        files = [f"{DIRECTORY}/{name}" for name in fragment_names(blobs)]
    elif SINGLE in present:
        files = [SINGLE]
    else:
        raise ValueError(f"{revision}: no mutation catalogue")
    return parse([(name, git("show", f"{revision}:{name}")) for name in files])


def differences(before, after):
    """List the rows added, removed or changed between two loaded catalogues."""
    (old_rows, old_sources), (new_rows, new_sources) = before, after
    old = {row.get("id"): row for row in old_rows}
    new = {row.get("id"): row for row in new_rows}
    found = []
    for identity in sorted(old.keys() - new.keys()):
        found.append(f"{identity}: removed (was {old_sources[identity]})")
    for identity in sorted(new.keys() - old.keys()):
        found.append(f"{identity}: added at {new_sources[identity]}")
    for identity in sorted(old.keys() & new.keys()):
        if old[identity] != new[identity]:
            fields = sorted(key for key in old[identity].keys() | new[identity].keys()
                            if old[identity].get(key) != new[identity].get(key))
            found.append(f"{identity}: changed {', '.join(fields)} "
                         f"(was {old_sources[identity]}, now {new_sources[identity]})")
    return found


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--same-rows-as", metavar="REVISION", required=True,
                        help="Git revision whose catalogue rows must equal the working tree's")
    args = parser.parse_args(argv)
    before = load_at_revision(ROOT, args.same_rows_as)
    after = load(ROOT)
    found = differences(before, after)
    if found:
        print("\n".join(found), file=sys.stderr)
        raise SystemExit(f"{len(found)} catalogue rows differ from {args.same_rows_as}")
    files = sorted({source.file for source in after[1].values()})
    print(f"Catalogue rows unchanged from {args.same_rows_as}: {len(after[0])} rows, "
          f"field for field, now in {len(files)} file(s)")


if __name__ == "__main__":
    main()
