# board.read paged reply

`reply.json` is captured from the real `board.read` handler by
`flow_audiences::board_read_names_and_vectors_match_live_reply`. The fixture
contains two named agents.

Each `boards` entry is `{agent_id, name, board}`. `name` is the agent's registry
name, the one `peer_send` and `peer_list` use. Entries omit `name` when the agent
has none, so a reader falls back to `agent_id`. Single-agent reads return the
unwrapped board instead.

The test checks the bytes and SHA256SUMS. To regenerate the files:

```
UPDATE_BOARD_READ_VECTORS=1 cargo nextest run -p prefrontal-core-module --test it -E 'test(board_read_names_and_vectors_match_live_reply)'
```
