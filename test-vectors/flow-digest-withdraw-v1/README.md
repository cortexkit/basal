# Flow digest withdrawal v1 wire vectors

These vectors pin a successful `sink.digest` request with `withdraw: true` and its reply. Core uses the same `sink.digest` operation and manifest `digest` grant as writes; withdrawals are journaled under the usual flow/run/call-position request key.

The test first commits a pending item for `replace_key: daily-report`, then invokes the request vector and compares the live reply to `reply.json`. A second call asserts the same outcome is replayed from the receipt.

Verify the pinned files with:

```sh
shasum -a 256 -c SHA256SUMS
```
