# Working in basal

Standing rules for workers. Follow them without asking. Ask only about what the last section lists.

## Building and testing

- Never compile, test or format Rust on the Mac. Use `runon: "linux"` for every cargo command. Use `runon: "linux,16c"` for a whole package suite and `runon: "windows"` for Windows-only code.
- macOS-only tests (Seatbelt, process attribution) can't run inside a worker shell, because macOS refuses a nested sandbox. When the change touches macOS code, run them on a fresh macOS machine with `runon: "macos"`. Otherwise leave them to CI and say so.
- Format with `cargo fmt --all` on Linux, then apply the formatter's changes before committing.
- `Cargo.lock` may be updated on the Mac with `cargo update -p <crate>` for the crates your change needs. That command records versions and compiles nothing. Never use `--workspace`.

## Before you deliver

- Run the full test suite of every package you changed on Linux, for example `cargo test -p basal-core` on `linux,16c`, plus clippy with `-D warnings` for those packages. Report the results in your delivery.
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
