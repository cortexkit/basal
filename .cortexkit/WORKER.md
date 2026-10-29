# Working in basal

Standing rules for workers. Follow them without asking. Ask only about what the last section lists.

## Building and testing

- Never compile, test or format Rust on the Mac. Use `runon: "linux"` for every cargo command. Use `runon: "linux,16c"` for a whole package suite and `runon: "windows"` for Windows-only code.
- macOS-only tests (Seatbelt, process attribution) can't run inside a worker shell, because macOS refuses a nested sandbox. When the change touches macOS code, run them on a fresh macOS machine with `runon: "macos"`. Otherwise leave them to CI and say so.
- Format with `cargo fmt --all` on Linux, then apply the formatter's changes before committing.
- `Cargo.lock` may be updated on the Mac with `cargo update -p <crate>` for the crates your change needs. That command records versions and compiles nothing. Never use `--workspace`.

## Before you deliver

- Before delivery, run `script/gate.sh --print-plan` locally, then execute the printed lane commands through the `runon` tool with the printed targets (`linux,16c`, `windows`, and conditional `macos`; script suites use `linux,2c`). Worker Seatbelt cannot read MOTOR's connection file, so do not call `ck motor run` from a worker shell.
- Run the printed `script/gate.sh --prepare-tools` locally before dispatch: it caches and verifies the CI-pinned NATS archives without Cargo. Run the printed `script/gate.sh --clean-tools` afterward, before committing; never commit `.gate-inputs/` binary inputs. The remote lanes verify the archives again and report their installed Rust version, refusing versions below the workspace minimum.
- Package-local changes select changed packages and their dependents; workspace/tooling changes, unknown package paths and an empty diff select all packages. Use `--workspace` to select all packages and `--all-platforms` to force the macOS lane. Report commands and results. The supervising agent with MOTOR access runs `script/gate.sh` before pushing; its read-only local rustfmt check is allowed, but all Cargo commands stay remote.
- A bug fix comes with a test that fails without the fix. Show it once (revert the fix, run the test, restore) and quote the failure in your report.
- Run a sidekick comment review on your diff and fix what it flags.
- Commit as `ualtinok <94532+ualtinok@users.noreply.github.com>`.

## Pre-approved; don't ask

- Updating test fixtures, stand-ins and assertions that encode the old behaviour your change deliberately replaces. Say which ones, and why.
- Regenerating a sample or golden file with its own generator when the output differs only because of your change. Never hand-edit a generated file.
- Adding a test-only seam behind `#[cfg(test)]` or the `test-support` feature.
- Merging `origin/main` into your branch and keeping both sides of a conflict.
- Adding a fingerprint to `.gitleaksignore` for a synthetic test value, with a comment saying why.
- Bounding a hanging test with a deadline. Widening a timing assertion only with measured evidence.

## Ask first

- A store migration, a worker protocol change, or anything a rollback can't undo.
- A change to a tool's description or input schema (`codemode`, `flow`). Agents' hosts register these, and a change can lock agents out.
- A new manifest field, refusal code or wire shape another module reads.
- Security boundaries: sandbox, caller admission, secrets, route scopes.
- A premise in your brief that turns out to be false. Stop and say so; don't build around it.
- Removing or loosening an assertion.
