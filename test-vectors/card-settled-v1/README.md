# Card-settled bus pointer v1

Pinned byte-for-byte from the Cingulate repository, git commit `9866794`,
`test-vectors/card-settled-v1/basal.basal.*`. `SHA256SUMS` retains the three
corresponding checksums from that source.

The subject addresses basal. The headers contain no content digest, and the
body carries only a pointer: basal rereads `consent.answers` from its own
cursor. None of the three sample files has a trailing newline.
