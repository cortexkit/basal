# Working in basal

Standing rules for workers. Follow them without asking. Ask only about what the last section lists.

## Building and testing

- Never compile, test or format Rust on the Mac. Use `runon: "linux"` (`linux,4c` for a build that needs it) for every cargo command, and `runon: "windows"` for Windows-only code.
- Run only filtered tests: the tests for the files and behaviour you changed, by name or module filter (`cargo test -p <pkg> --test <file> <filter>`). Never run a whole package suite, the workspace, or `script/gate.sh`; the supervising agent runs the full suites once in its push gate. A filtered run longer than about 15 minutes means the filter is too broad.
- A failure that looks unrelated to your change: rerun that one test by name on the base commit, never the suite.
- macOS-only tests (Seatbelt, process attribution) can't run inside a worker shell, because macOS refuses a nested sandbox. When the change touches macOS code, run the affected tests by name on a fresh macOS machine with `runon: "macos"`. Otherwise leave them to CI and say so.
- Format on Linux with `cargo fmt --all -- --emit stdout`, then apply its output to the files you changed with your edit tool, and confirm with `cargo fmt --all --check`. Plain `cargo fmt --all` under `runon` is refused before it starts, because the write guard treats it as rewriting the whole checkout, so naming files in `return_paths` doesn't help. Don't wrap it in `sh -c` to get around the guard.
- `Cargo.lock` may be updated on the Mac with `cargo update -p <crate>` for the crates your change needs. That command records versions and compiles nothing. Never use `--workspace`.

## Before you deliver

- Before delivery: `cargo fmt --all --check`, `cargo clippy -p <changed packages> --all-targets --locked -- -D warnings`, and your filtered tests, all on Linux. Report the commands and results. Don't run `script/gate.sh` or its lanes, and don't call `ck motor run` (worker Seatbelt can't read MOTOR's connection file).
- A bug fix comes with a test that fails without the fix. Show it once (revert the fix, run that test by name, restore) and quote the failure in your report.
- Run a sidekick comment review on your diff and fix what it flags.
- Commit as `ualtinok <94532+ualtinok@users.noreply.github.com>`.
- Design notes, field mappings, evidence and investigation write-ups go in the git-ignored `.cortexkit/internal-docs/` (they reach the main checkout when your work is settled). Never commit them under `crates/` or `docs/`; `docs/` is public user documentation. Anything a later engineer needs to read the code goes in a code comment.

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
