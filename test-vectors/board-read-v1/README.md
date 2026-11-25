# board.read paged reply

`flow_audiences::board_read_names_and_vectors_match_live_reply` captures both
samples from the real `board.read` handler. `reply.json` contains two named
agents. `oversized-entry.json` shows a page with an oversized board replaced by
an error entry.

A page contains at most `limit` entries (16 by default) and at most 256 KiB of
compact JSON, including the page fields and cursor. If the next board does not
fit, use `next_cursor` to resume at that board on the next page.

A successful `boards` entry is `{agent_id, name, board}`. When a board cannot
fit on a page by itself, its entry is `{agent_id, name, error}` instead. The
error has `code: "board_read_too_large"`, `message`, and `size_bytes`, the size
of the compact board JSON in bytes. Paging treats that agent as returned, so
the next page does not retry its board. `name` is the agent's registry name,
the one `peer_send` and `peer_list` use. Entries omit `name` when the agent has
none, so a reader falls back to `agent_id`. Single-agent reads still return the
unwrapped board and are not subject to the page byte cap.

The test checks the bytes and SHA256SUMS. To regenerate the files:

```
UPDATE_BOARD_READ_VECTORS=1 cargo nextest run -p prefrontal-core-module --test it -E 'test(board_read_names_and_vectors_match_live_reply)'
```
