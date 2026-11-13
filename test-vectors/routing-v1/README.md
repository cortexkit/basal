# Routing selection prices

`consumers-and-placement.json` is copied byte-for-byte from the published
`cortexkit-role-routing` 0.7.22 package's `test-vectors/routing-v1/` directory.
The package version and checksum are pinned in the workspace and `Cargo.lock`.
Tests adapt only the fixture runner's module id to `broca`, since basal opens
Broca sessions itself; the selected rates, estimates and lifetime keys remain
the published values.
