# flows-phone-v1 vectors

Generated from basal `8d15dc14` (`flows-phone-v1`).

- Each **success** file holds the bare op result. Basal's live management
  reply wraps that body as `{"result": <body>}`
  (`crates/basal-module/src/serve.rs:464`).
- Each **refusal** file holds `{"error": {code, message}}`, which arrives
  live as a protocol error frame.
- `SHA256SUMS` covers the vector files in this directory.
