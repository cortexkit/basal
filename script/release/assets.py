"""Package and verify Basal's installer assets on the release runners."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
from zipfile import ZIP_DEFLATED, BadZipFile, ZipFile


BINARIES = ("ck-basal", "ck-basal-worker")
TARGETS = ("darwin-arm64", "linux-x64", "linux-arm64", "windows-x64", "windows-arm64")
TRIPLET = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
WORKER_GATE = Path(__file__).resolve().parents[1] / "sign-worker.sh"


def archive_path(directory, target, binary):
    return directory / f"{binary}-{target}.zip"


def member_name(target, binary):
    return f"{binary}.exe" if target.startswith("windows-") else binary


def pack(args):
    name = member_name(args.target, args.binary)
    args.directory.mkdir(parents=True, exist_ok=True)
    with ZipFile(archive_path(args.directory, args.target, args.binary), "x", ZIP_DEFLATED) as archive:
        archive.write(args.source / name, arcname=name)


def checksum(archive):
    digest = hashlib.sha256()
    with archive.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return f"{digest.hexdigest()}  {archive.name}\n".encode("ascii")


def archive_member(archive, target, binary):
    expected = member_name(target, binary)
    entries = archive.infolist()
    if len(entries) != 1 or entries[0].filename != expected or not entries[0].file_size:
        raise ValueError(f"{archive.filename}: expected only a nonempty {expected} at the ZIP root")
    if not target.startswith("windows-"):
        mode = entries[0].external_attr >> 16
        if not stat.S_ISREG(mode) or not mode & stat.S_IXUSR:
            raise ValueError(f"{archive.filename}: {expected} must be a regular executable")
    return entries[0]


def probe(path, flag):
    return subprocess.check_output([str(path), flag], text=True, timeout=15).strip()


def verify_target(args):
    validate_revision(args.revision)
    with tempfile.TemporaryDirectory() as scratch:
        executables = {}
        for binary in BINARIES:
            path = archive_path(args.directory, args.target, binary)
            with ZipFile(path) as archive:
                member = archive_member(archive, args.target, binary)
                # Run the archived bytes side by side, with the same suffix
                # relationship as the installed names, but under ckdev- names
                # to distinguish verification from a running installation.
                executable = Path(scratch) / f"ckdev-{member.filename.removeprefix('ck-')}"
                with archive.open(member) as source, executable.open("xb") as destination:
                    shutil.copyfileobj(source, destination)
                executable.chmod(0o755)
                executables[binary] = (path, executable)

        for binary, (path, executable) in executables.items():
            version = probe(executable, "--version")
            reported = re.fullmatch(rf"{binary} (\S+) \({args.revision}\)", version)
            if not reported or (binary == "ck-basal" and reported[1] != args.version):
                raise ValueError(f"{path}: unexpected --version for {args.revision}: {version!r}")
            if binary == "ck-basal":
                manifest = json.loads(probe(executable, "--manifest"))
                if manifest["module_id"] != "basal" or manifest["module_version"] != args.version:
                    raise ValueError(f"{path}: --manifest must declare basal {args.version}")
                revision = manifest["provenance"]["build_git_sha"]
                if revision != args.revision:
                    raise ValueError(f"{path}: --manifest reports {revision!r}, expected {args.revision}")
            elif args.target.startswith("darwin-"):
                # Check the embedded Seatbelt profile and live confinement
                # after Developer ID signing, on the bytes from the ZIP.
                subprocess.run(["sh", str(WORKER_GATE), "verify", str(executable)], check=True, timeout=60,
                               env={**os.environ, "BASAL_WORKER_IDENTIFIER": "ck-basal-worker"})
            print(f"{path.name}: {version}")

    for path, _ in executables.values():
        path.with_suffix(".zip.sha256").write_bytes(checksum(path))


def validate_revision(revision):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("expected a full 40-character Git commit SHA")


def core_floor(dry_run):
    floor = os.environ.get("BASAL_REQUIRES_CORE", "")
    if not floor and dry_run:
        return None
    if not re.fullmatch(TRIPLET, floor):
        raise ValueError("BASAL_REQUIRES_CORE must be set to the confirmed MAJOR.MINOR.PATCH floor before publishing")
    return floor


def assemble(args):
    validate_revision(args.revision)
    floor = core_floor(args.dry_run)
    archives = [archive_path(args.directory, target, binary) for target in TARGETS for binary in BINARIES]
    files = {path.name for path in archives} | {f"{path.name}.sha256" for path in archives}
    actual = {path.name for path in args.directory.iterdir()}
    if actual != files:
        raise ValueError(f"wrong asset inventory: missing {sorted(files - actual)}; unexpected {sorted(actual - files)}")
    args.output.mkdir(parents=True)
    for target in TARGETS:
        for binary in BINARIES:
            path = archive_path(args.directory, target, binary)
            sidecar = path.with_suffix(".zip.sha256")
            if sidecar.read_bytes() != checksum(path):
                raise ValueError(f"{sidecar}: checksum does not match the ZIP")
            with ZipFile(path) as archive:
                archive_member(archive, target, binary)
                if archive.testzip() is not None:
                    raise ValueError(f"{path}: ZIP integrity check failed")
            # All targets are verified even when Windows is not yet released.
            if not target.startswith("windows-") or args.dry_run or args.publish_windows:
                shutil.copyfile(path, args.output / path.name)
                shutil.copyfile(sidecar, args.output / sidecar.name)

    # `ck upgrade` compares a binary's first --version token with the release
    # version whenever `reports` is set. The worker prints its own crate
    # version, which need not match, so it publishes `reports: null`: upgrade
    # then checks only that it runs, and its provenance is the archive's sha256.
    manifest: dict[str, object] = {"binaries": {
        "ck-basal": {"reports": args.revision},
        "ck-basal-worker": {"reports": None},
    }}
    if floor is not None:
        manifest["requires_core"] = floor
    (args.output / "release-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"Verified {len(archives)} ZIPs and sidecars; prepared {len(list(args.output.iterdir()))} files in {args.output}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    packaging = commands.add_parser("pack")
    packaging.add_argument("source", type=Path)
    packaging.add_argument("directory", type=Path)
    packaging.add_argument("target", choices=TARGETS)
    packaging.add_argument("binary", choices=BINARIES)
    verification = commands.add_parser("verify-target")
    verification.add_argument("directory", type=Path)
    verification.add_argument("target", choices=TARGETS)
    verification.add_argument("version")
    verification.add_argument("revision")
    assembly = commands.add_parser("assemble")
    assembly.add_argument("directory", type=Path)
    assembly.add_argument("output", type=Path)
    assembly.add_argument("revision")
    assembly.add_argument("--dry-run", action="store_true")
    assembly.add_argument("--publish-windows", action="store_true")
    commands.add_parser("check-floor")
    args = parser.parse_args()
    try:
        if args.command == "pack":
            pack(args)
        elif args.command == "verify-target":
            verify_target(args)
        elif args.command == "assemble":
            assemble(args)
        elif args.command == "check-floor":
            print(f"requires_core: {core_floor(dry_run=False)}")
    except (BadZipFile, KeyError, OSError, TypeError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"release assets: {error}\n")


if __name__ == "__main__":
    main()
