"""Build-free helpers for gate.sh; never invoke Cargo on the caller's machine."""

import glob
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib


def git(*args):
    return subprocess.check_output(["git", *args])


def tracked_paths():
    return [
        Path(os.fsdecode(name))
        for name in git("ls-files", "-z", "--cached", "--others", "--exclude-standard").split(b"\0")
        if name
    ]


def workspace():
    manifest = tomllib.loads(Path("Cargo.toml").read_text())
    packages = {}
    for pattern in manifest["workspace"]["members"]:
        for directory in glob.glob(pattern):
            path = Path(directory)
            data = tomllib.loads((path / "Cargo.toml").read_text())
            packages[data["package"]["name"]] = (path, data)
    return manifest, packages


def changed_paths(base):
    # Use staged, unstaged, deleted and nonignored untracked paths to select packages.
    paths = set(git("diff", "--name-only", "-z", base, "--").split(b"\0"))
    paths.update(git("ls-files", "-z", "--others", "--exclude-standard").split(b"\0"))
    return {os.fsdecode(path) for path in paths if path}


def select_packages(manifest, packages, paths, whole_workspace=False):
    selected = set()
    for filename in paths:
        owners = {
            name for name, (directory, _) in packages.items()
            if Path(filename).is_relative_to(directory)
        }
        if not owners:
            # Workspace tooling and unknown/deleted package paths can affect all crates.
            return sorted(packages)
        selected.update(owners)
    if whole_workspace or not paths:
        return sorted(packages)

    dependencies = manifest["workspace"].get("dependencies", {})
    dependents = {name: set() for name in packages}
    for name, (_, data) in packages.items():
        tables = [data, *data.get("target", {}).values()]
        for table in tables:
            for kind in ("dependencies", "dev-dependencies", "build-dependencies"):
                for alias, spec in table.get(kind, {}).items():
                    if isinstance(spec, dict) and spec.get("workspace"):
                        spec = dependencies.get(alias, {})
                    dependency = spec.get("package", alias) if isinstance(spec, dict) else alias
                    if dependency in dependents:
                        dependents[dependency].add(name)
    # testkit's build.rs builds the worker executable outside Cargo's dependency graph.
    if "basal-worker" in dependents and "basal-testkit" in dependents:
        dependents["basal-worker"].add("basal-testkit")
    while True:
        expanded = selected | set().union(*(dependents[name] for name in selected))
        if expanded == selected:
            return sorted(selected)
        selected = expanded


def macos_paths_changed(paths):
    native_paths = {
        "crates/basal-worker/src/confinement.rs", "crates/basal-worker/sandbox/worker.sb",
        "crates/basal-worker/src/main.rs", "crates/basal-worker/src/probe.rs",
        "crates/basal-worker/tests/confinement.rs", "crates/basal-worker/tests/ipc.rs",
        "crates/basal-module/src/process.rs", "crates/basal-module/src/main.rs", "crates/basal-module/src/pool.rs",
        "crates/basal-module/tests/privacy_launch.rs", "crates/basal-module/tests/e2e.rs", "crates/basal-module/tests/confinement.rs",
        "crates/basal-host/src/builtins/fs.rs", "crates/basal-host/src/builtins/git.rs", "crates/basal-host/src/builtins/git/tests.rs",
        "crates/basal-testkit/src/process.rs", "crates/basal-module/Cargo.toml", "Cargo.toml", "Cargo.lock",
        "script/sign-worker.sh", "script/signing.sh", "script/stage.sh", "script/flows-rig.sh",
    }
    return any(
        "macos" in path.lower() or "darwin" in path.lower()
        or path in native_paths
        for path in paths
    )


def format_roots(manifest, packages):
    roots = {}
    for directory, data in packages.values():
        edition = data["package"].get("edition", "2015")
        if isinstance(edition, dict):
            edition = manifest["workspace"]["package"]["edition"]
        candidates = [directory / "src/lib.rs", directory / "src/main.rs"]
        if data["package"].get("build", True) is not False:
            candidates.append(directory / data["package"].get("build", "build.rs"))
        for kind, folder, automatic in (("bin", "src/bin", "autobins"), ("test", "tests", "autotests"), ("bench", "benches", "autobenches"), ("example", "examples", "autoexamples")):
            if data["package"].get(automatic, True):
                candidates.extend((directory / folder).glob("*.rs"))
                candidates.extend((directory / folder).glob("*/main.rs"))
            for target in data.get(kind, []):
                if "path" in target:
                    candidates.append(directory / target["path"])
        if "path" in data.get("lib", {}):
            candidates.append(directory / data["lib"]["path"])
        for path in candidates:
            if path.is_file():
                roots.setdefault(str(edition), set()).add(path)
    return roots


def check_format():
    manifest, packages = workspace()
    roots = format_roots(manifest, packages)
    subprocess.check_call(["rustfmt", "--version"])
    print(f"Checking {sum(map(len, roots.values()))} Rust target roots (including their modules)", flush=True)
    for edition, paths in sorted(roots.items()):
        subprocess.check_call(["rustfmt", "--check", "--edition", edition, *map(str, sorted(paths))])


def snapshot(destination):
    destination = Path(destination)
    subprocess.check_call(["git", "clone", "--quiet", "--shared", "--no-checkout", ".", str(destination)])
    subprocess.check_call(["git", "-C", str(destination), "reset", "--mixed", "--quiet", "HEAD"])
    for path in set(tracked_paths()):
        if not path.exists() and not path.is_symlink():
            continue
        target = destination / path
        target.parent.mkdir(parents=True, exist_ok=True)
        if path.is_symlink():
            target.symlink_to(os.readlink(path))
        else:
            shutil.copy2(path, target)


def check_author(base):
    expected = "94532+ualtinok@users.noreply.github.com"
    authors = git("log", "--format=%ae", f"{base}..HEAD").decode().splitlines()
    identity = git("var", "GIT_AUTHOR_IDENT").decode()
    authors.append(identity.rsplit("<", 1)[1].split(">", 1)[0])
    wrong = sorted(set(authors) - {expected})
    if wrong:
        raise SystemExit(f"Unexpected author email(s): {', '.join(wrong)}; expected {expected}")
    print(f"Author email: {expected} ({len(authors) - 1} commits plus current identity)")


def nats_pins():
    action = Path(".github/actions/install-nats-server/action.yml").read_text()
    version = re.search(r'archive="nats-server-v([0-9.]+)-\$platform', action)
    pins = re.findall(r"platform=([a-z0-9-]+); extension=([a-z.]+); sha=([a-f0-9]{64})", action)
    if version is None or len(pins) != 6:
        raise SystemExit("Cannot read NATS pins from the CI composite action; update the gate parser")
    return version[1], {platform: (extension, sha) for platform, extension, sha in pins}


def prepare_tools(destination, lanes):
    version, pins = nats_pins()
    cache = Path("target/scratch/gate-downloads")
    cache.mkdir(parents=True, exist_ok=True)
    output = Path(destination) / ".gate-inputs/nats"
    output.mkdir(parents=True, exist_ok=True)
    platforms = {"linux": ["linux-amd64"], "windows": ["windows-amd64"], "macos": ["darwin-amd64", "darwin-arm64"]}
    cases = {"linux-amd64": "Linux:x86_64", "windows-amd64": "MINGW*:x86_64|MSYS*:x86_64", "darwin-amd64": "Darwin:x86_64", "darwin-arm64": "Darwin:arm64"}
    manifest = ['# Generated from the CI NATS action; do not edit.', 'case "$(uname -s):$(uname -m)" in']
    for platform in sorted({platform for lane in lanes for platform in platforms[lane]}):
        extension, expected = pins[platform]
        archive = f"nats-server-v{version}-{platform}.{extension}"
        cached = cache / archive
        if not cached.exists() or hashlib.sha256(cached.read_bytes()).hexdigest() != expected:
            partial = cache / f"{archive}.{os.getpid()}.partial"
            try:
                subprocess.check_call(["curl", "--fail", "--location", "--retry", "3", f"https://github.com/nats-io/nats-server/releases/download/v{version}/{archive}", "-o", str(partial)])
                if hashlib.sha256(partial.read_bytes()).hexdigest() != expected:
                    raise SystemExit(f"NATS checksum mismatch: {archive}")
                partial.replace(cached)
            finally:
                partial.unlink(missing_ok=True)
        print(f"Verified cached NATS archive: {archive}", flush=True)
        shutil.copy2(cached, output / archive)
        manifest.append(f'  {cases[platform]}) platform={platform}; extension={extension}; sha={expected}; archive={archive} ;;')
    manifest.extend(["  *) echo 'NATS archive not prepared for this runner; run script/gate.sh --prepare-tools first' >&2; exit 1 ;;", "esac"])
    (output / "manifest.sh").write_text("\n".join(manifest) + "\n")


def main():
    command, *args = sys.argv[1:]
    if command == "plan":
        base, mode = args
        manifest, packages = workspace()
        paths = changed_paths(base)
        selected = select_packages(manifest, packages, paths, mode == "workspace")
        print("yes" if macos_paths_changed(paths) else "no", *selected)
    elif command == "fmt":
        check_format()
    elif command == "snapshot":
        snapshot(args[0])
    elif command == "author":
        check_author(args[0])
    elif command == "tools":
        prepare_tools(args[0], args[1:] or ["linux", "windows", "macos"])
    elif command == "clean-tools":
        shutil.rmtree(".gate-inputs/nats", ignore_errors=True)
        if Path(".gate-inputs").exists() and not any(Path(".gate-inputs").iterdir()):
            Path(".gate-inputs").rmdir()
    else:
        raise SystemExit(f"Unknown helper command: {command}")


if __name__ == "__main__":
    main()
