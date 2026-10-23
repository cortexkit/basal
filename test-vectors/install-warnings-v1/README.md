# Install warnings v1 vectors

`protected-root.json` is the exact `flow.install_request` body basal sends core
for a synthetic flow whose only `fs.read` root is `~/Documents/notes`. That root
lies in a folder both macOS and Windows protect, so `flow_install.warnings`
holds one `protected_root` warning whose `detail` has one line per operating
system, separated by `\n`:

- On macOS, the first access to ~/Documents asks the operator for permission.
- On Windows, writes under ~/Documents are blocked while Controlled Folder
  Access is on.

The warning's `code` is the install warning kind. Before this kind existed,
basal sent `macos_protected_root` for macOS folders only.

The body is captured from basal's real install path (the module's
`flow.install` through `CoreConsent`) by
`crates/basal-module/tests/install_warnings_vector.rs`, which fails if the
code and this vector drift apart. Install refuses a root that does not exist,
so that test points `HOME` at a scratch folder holding `Documents/notes`; it
is not built on Windows, where `~` is the process profile instead of `HOME`.
It is the request for a core that does not
list `flow-install-dry-run-text/v1`, so it carries no `dry_run_text`. All ids
and text are synthetic.

Verify the pinned files with:

```sh
shasum -a 256 -c SHA256SUMS
```

Regenerate them after an intended change with
`BASAL_WRITE_VECTORS=1 cargo test -p basal-module --test install_warnings_vector`.
