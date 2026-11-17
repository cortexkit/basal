-- Moving an agent's flows to another computer (agent-data/v1, driven by
-- Entorhinal). The rows below are basal's side of one move: whether the
-- agent's flows are frozen or already live elsewhere, how far each cut has
-- got, the first reply to each state-changing request, and the moved flows
-- still waiting for this computer's approval.

-- One row per agent whose flows may not run here. `frozen` while a cut is
-- being taken (between prepare and release or abort): nothing is admitted,
-- schedule fires wait, and events wait in the backlog. `moved` once the
-- agent's flows live on another computer (`holder`): the old copy stays as a
-- tombstone and local installs, enables and instances for the agent are
-- refused until a move back replaces it.
CREATE TABLE agent_move_holds (
    agent_id TEXT PRIMARY KEY NOT NULL,
    kind     TEXT NOT NULL CHECK (kind IN ('frozen', 'moved')),
    cut_id   TEXT NOT NULL,
    holder   TEXT,
    at       INTEGER NOT NULL,
    CHECK ((kind = 'moved') = (holder IS NOT NULL))
) WITHOUT ROWID;

-- Which agent each flow belongs to: its owner, or before any approval the
-- single author of its installed versions (the rule the flow ops use).
CREATE VIEW agent_move_flow_agents AS
SELECT flow_id, owner AS agent_id FROM flows WHERE owner IS NOT NULL
UNION ALL
SELECT f.flow_id, MIN(i.author) FROM flows f JOIN installs i USING (flow_id)
WHERE f.owner IS NULL GROUP BY f.flow_id HAVING COUNT(DISTINCT i.author) = 1;

-- Flows whose agent holds a freeze or a tombstone: admission skips them.
CREATE VIEW agent_move_held_flows AS
SELECT a.flow_id, h.agent_id, h.kind
FROM agent_move_flow_agents a JOIN agent_move_holds h USING (agent_id);

-- The checkpoint of one cut on this computer: the step it reached, the ref
-- set its first op named, the stream item it produced or read, and the
-- content fingerprint of the agent's flows at the cut (what `diverged`
-- compares a later local copy with).
CREATE TABLE agent_move_cuts (
    cut_id      TEXT PRIMARY KEY NOT NULL,
    agent_id    TEXT,
    side        TEXT NOT NULL CHECK (side IN ('source', 'receiver', 'unknown')),
    step        TEXT NOT NULL CHECK (step IN ('prepared', 'exported', 'imported', 'activated', 'aborted', 'released')),
    refs        TEXT NOT NULL CHECK (json_valid(refs)),
    session_ref TEXT,
    end_offset  INTEGER CHECK (end_offset IS NULL OR end_offset >= 0),
    digest      TEXT,
    fingerprint TEXT,
    dropped     TEXT CHECK (dropped IS NULL OR json_valid(dropped)),
    updated_at  INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX agent_move_cuts_agent ON agent_move_cuts (agent_id, updated_at);

-- The first reply to each state-changing request, keyed by (cut, op), so a
-- repeat of the same request gets the same reply.
CREATE TABLE agent_move_replies (
    cut_id  TEXT NOT NULL,
    op      TEXT NOT NULL,
    request TEXT NOT NULL CHECK (json_valid(request)),
    reply   TEXT NOT NULL CHECK (json_valid(reply)),
    PRIMARY KEY (cut_id, op)
) WITHOUT ROWID;

-- Moved flows that arrived approved on the other computer and wait until
-- this computer's prefrontal reports the same code active. `restore` is the
-- source's own enable state, put back when the flow is approved here.
CREATE TABLE agent_move_pending (
    flow_id    TEXT PRIMARY KEY NOT NULL REFERENCES flows (flow_id),
    agent_id   TEXT NOT NULL,
    cut_id     TEXT NOT NULL,
    version    INTEGER NOT NULL CHECK (version >= 1),
    code_hash  BLOB NOT NULL CHECK (length(code_hash) = 32),
    restore    TEXT NOT NULL CHECK (json_valid(restore)),
    since      INTEGER NOT NULL,
    checked_at INTEGER,
    last_check TEXT
);
