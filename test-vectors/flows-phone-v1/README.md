# flows-phone-v1 vectors

Generated from basal `8d15dc14` (`flows-phone-v1`).

- Each **success** file holds the bare op result. Basal's live management
  reply wraps that body as `{"result": <body>}`
  (`crates/basal-module/src/serve.rs:464`).
- Each **refusal** file holds `{"error": {code, message}}`, which arrives
  live as a protocol error frame.
- `SHA256SUMS` covers the vector files in this directory.

## Held installs

`overview-held.json` and `details-held.json` show a held install with
`reason: "catalog_status_stale"` and `since_ms` present.
`overview-held-no-since.json` and `details-held-no-since.json` show the same
hold with `since_ms` absent because core supplied no stale time.
`install_hold` is optional and present only while the pinned flow version is
held; a hold does not disable the flow.

The overview request is `flow.overview` with `{}`; the details request is
`flow.details` with `{"flow_id":"flow-phone-held"}`. Like the other success
samples, these files contain the bare handler result, not a request or a
protocol envelope.

Regenerate all vectors and checksums through the real handlers with:

```sh
BASAL_WRITE_VECTORS=1 RUST_TEST_THREADS=1 cargo test --locked -p basal-module --test phone_flows phone_vectors_match_real_dispatch_and_sha256sums -- --exact
```

Run the same command without `BASAL_WRITE_VECTORS=1` to check for drift.
