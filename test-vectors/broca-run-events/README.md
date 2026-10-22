# Broca run-event samples

Real `run.ended` and `run.skipped` messages captured from Broca 0.3.245+
(from sessions opened by Core's research helper; a basal flow session has
the same shapes).

- `*.event_get.json`: the exact `run.event.get` reply bytes, unmodified.
  `jcs` is the body; its SHA-256 (lowercase hex) is the bus digest.
- `*.bus.json`: the bus message: subject, message id (the event key),
  application headers, the bind triple the session was opened with, and the
  digest. The bind triple's `project_root` was a machine-local path and is
  redacted; nothing else is changed.

`SHA256SUMS` covers the files as committed. The live event keys expired 24 h
after capture, so tests read these files and never look anything up.
