const MINUTE = 60 * 1000;
const HOUR = 60 * MINUTE;
const INTERVAL = {wait_ended: 30 * MINUTE, eta_passed: 2 * HOUR, stale_active: 6 * HOUR, lane_done: 2 * HOUR};
const INDEX_SHARDS = 16;
const INDEX_CAP = 128;
let boardOwners = new Set();

function utf8Bytes(value) {
  let size = 0;
  for (const char of value) {
    const code = char.codePointAt(0);
    size += code <= 0x7f ? 1 : code <= 0x7ff ? 2 : code <= 0xffff ? 3 : 4;
  }
  return size;
}

function cut(value, bytes) {
  let out = '';
  let size = 0;
  for (const char of String(value)) {
    const next = utf8Bytes(char);
    if (size + next > bytes) break;
    out += char;
    size += next;
  }
  return out;
}

function bounded(item) {
  item.title = cut(item.title, 200).slice(0, 200);
  item.body = cut(item.body, 4096);
  // Escaping can make JSON much larger than the original strings.
  while (utf8Bytes(JSON.stringify(item.data)) > 8192) {
    const fields = [];
    for (const key of Object.keys(item.data)) {
      if (typeof item.data[key] === 'string') fields.push([item.data, key]);
    }
    for (const key of Object.keys(item.data.item_wait ?? {})) {
      if (typeof item.data.item_wait[key] === 'string') fields.push([item.data.item_wait, key]);
    }
    fields.sort(([a, ka], [b, kb]) => utf8Bytes(b[kb]) - utf8Bytes(a[ka]));
    const [object, key] = fields[0];
    object[key] = cut(object[key], Math.floor(utf8Bytes(object[key]) / 2));
  }
  while (utf8Bytes(JSON.stringify(item)) > 16384) {
    item.body = cut(item.body, Math.floor(utf8Bytes(item.body) / 2));
  }
  return item;
}

function replacementKey(data) {
  const key = `board_nudge:${data.agent_id}:${data.item_id ?? data.lane_id}:${data.rule}`;
  return utf8Bytes(key) <= 200 ? key : null;
}

function itemRef(wait) {
  return typeof wait?.ref === 'string' ? /^item:([^/]+)\/(.+)$/.exec(wait.ref) : null;
}

function peerWait(wait) {
  return typeof wait?.on === 'string' && (boardOwners.has(wait.on) || /^(ag_|agent_)/.test(wait.on)) &&
    (wait.ref == null || (typeof wait.ref === 'string' && wait.ref.startsWith('peer:')));
}

function age(now, since) {
  const minutes = Math.max(0, Math.floor((now - since) / MINUTE));
  return minutes < 60 ? `${minutes} min ago` : `${Math.floor(minutes / 60)} h ago`;
}

function digest(rule, agent, lane, item, since, now, copyOf, copyPeer, waitingItem, waitingLane) {
  const data = {kind: 'board_nudge', rule, agent_id: agent, lane_id: lane.id,
    lane_title: lane.title, since_ms: since};
  if (item) {
    data.item_id = item.id;
    data.item_text = item.text;
    const current = waitingItem ?? item;
    data.item_state = current.state;
    if (current.state === 'blocked') {
      data.item_wait = {};
      if (current.wait?.on != null) data.item_wait.on = current.wait.on;
      if (current.wait?.ref != null) data.item_wait.ref = current.wait.ref;
    }
    for (const key of ['eta_ms', 'follow_up', 'note_ref']) {
      if (item[key] !== undefined) data[key] = item[key];
    }
    if (item.wait?.ref !== undefined) data.ref = item.wait.ref;
    if (item.wait?.ref_terminal_at_ms !== undefined) data.ref_terminal_at_ms = item.wait.ref_terminal_at_ms;
  }
  if (copyOf !== undefined) {
    data.copy_of = copyOf;
    data.own_lane_id = (waitingLane ?? lane).id;
    data.own_item_id = (waitingItem ?? item).id;
    data.own_item_text = (waitingItem ?? item).text;
  }
  const text = cut(item?.text ?? lane.title, 600);
  const call = `board update: item ${JSON.stringify(text)} → done`;
  let title;
  let body;
  if (rule === 'wait_ended') {
    title = `Wait ended: ${text}`;
    body = `Your wait ended ${age(now, since)}. Update your item: ${call}.`;
  } else if (rule === 'eta_passed') {
    title = `ETA passed: ${text}`;
    if (copyOf !== undefined) {
      const peer = copyPeer ?? agent;
      body = `${peer}'s item you wait on passed its ETA ${age(now, since)}. Follow up with ${peer}, or re-plan your item: set a new ETA or change its wait.`;
    } else if (peerWait(item.wait)) {
      body = `${agent} is waiting on you; the ETA passed ${age(now, since)}. Commit with an item on your own board, or decline: board update: add item ${JSON.stringify(text)} with an ETA; or tell ${agent} you decline.`;
    } else {
      body = `Your ETA passed ${age(now, since)}. Update your item: ${call}; or extend: set a new ETA on your item.`;
    }
  } else if (rule === 'stale_active') {
    title = `Active item unchanged: ${text}`;
    body = `Your active item last changed ${age(now, since)}, although you have been active since. Update your item: ${call}; or extend: set a new ETA on your item.`;
  } else {
    title = `Lane complete: ${text}`;
    body = `Every item in this lane has been done since ${age(now, since)}. Retire the lane: board update: lane ${JSON.stringify(text)} → retired.`;
  }
  if (item?.follow_up) body += ` Follow-up: ${cut(item.follow_up, 2400)}`;
  return bounded({title, body, data});
}

function memoryKey(agent, id, rule) {
  const key = `nudge:${agent}:${id}:${rule}`;
  // Core ids are bounded names; refuse an oversized composite instead of aliasing two items.
  return utf8Bytes(key) <= 256 ? key : null;
}

function shardFor(key) {
  let hash = 0;
  for (const char of key) hash = (hash * 31 + char.codePointAt(0)) >>> 0;
  return hash % INDEX_SHARDS;
}

async function run() {
  const now = Date.now();
  const refusals = [];
  function log(message, detail) {
    if (refusals.length < 128) refusals.push({message, ...detail});
  }
  const boards = [];
  const cursors = new Set();
  let cursor;
  do {
    const page = await ops.call('prefrontal-core', 'board.read', {limit: 16, ...(cursor ? {cursor} : {})});
    boards.push(...page.boards);
    cursor = page.next_cursor;
    if (cursor && cursors.has(cursor)) throw new Error('board.read repeated a cursor');
    cursors.add(cursor);
  } while (cursor != null);

  boardOwners = new Set(boards.map(board => board.agent_id));
  const indexes = [];
  for (let i = 0; i < INDEX_SHARDS; i++) indexes.push(await kv.get(`index:${i}`) ?? []);
  const seen = new Set();
  const reverse = new Map();
  for (const {agent_id: agent, board} of boards) {
    for (const lane of board.lanes) {
      for (const item of lane.items) {
        const ref = itemRef(item.wait);
        if (item.state === 'done' || !ref) continue;
        const key = JSON.stringify([ref[1], ref[2]]);
        if (!reverse.has(key)) reverse.set(key, new Map());
        reverse.get(key).set(agent, {item, lane});
      }
    }
  }

  async function forget(key) {
    await kv.delete(key);
    const shard = shardFor(key);
    const next = indexes[shard].filter(entry => entry !== key);
    if (next.length !== indexes[shard].length) {
      await kv.set(`index:${shard}`, next);
      indexes[shard] = next;
    }
  }

  async function remember(key, value) {
    const shard = shardFor(key);
    if (!indexes[shard].includes(key)) {
      // Agent and item/lane ids contain only name characters. At most 128 keys of 256 bytes plus JSON punctuation fit below 64 KiB.
      if (indexes[shard].length >= INDEX_CAP) {
        log('board hygiene memory index full', {shard});
        return false;
      }
      const next = [...indexes[shard], key];
      await kv.set(`index:${shard}`, next);
      indexes[shard] = next;
    }
    await kv.set(key, value);
    return true;
  }

  let replacementUnsupportedLogged = false;
  async function send(agent, item, action) {
    const key = replacementKey(item.data);
    try {
      try {
        await sink.digest(agent, item, action, key === null ? undefined : {replace_key: key});
      } catch (error) {
        // Unsupported replacement is refused before dispatch, so retrying without it cannot duplicate a delivery.
        if (key === null || error.data?.code !== 'digest_replace_key_unsupported') throw error;
        if (!replacementUnsupportedLogged) {
          log('board hygiene digest replacement unsupported', {code: error.data.code});
          replacementUnsupportedLogged = true;
        }
        await sink.digest(agent, item, action);
      }
      return true;
    } catch (error) {
      log('board hygiene digest refused', {agent, code: error.data?.code ?? String(error)});
      return false;
    }
  }

  async function check(rule, agent, lane, item, condition, since, firstDelay, recipient) {
    const key = memoryKey(agent, item?.id ?? lane.id, rule);
    if (key === null) {
      log('board hygiene memory key too long', {agent, rule});
      return;
    }
    seen.add(key);
    const changed = item ? item.changed_at_ms ?? null : since;
    try {
      const previous = await kv.get(key);
      if (previous && previous.changed_at_ms !== changed) {
        await forget(key);
        return;
      }
      if (!condition) {
        if (previous) await forget(key);
        return;
      }
      if (now < since + firstDelay) return;
      if (previous && now - previous.last_send_ms < INTERVAL[rule]) return;
      const count = (previous?.count ?? 0) + 1;
      // Record before delivery so a full KV store cannot create unthrottled wakes.
      if (!await remember(key, {changed_at_ms: changed, last_send_ms: now, count})) return;
      const action = rule === 'wait_ended' || (rule === 'eta_passed' && count === 1) ? 'wake' : 'piggyback';
      if (!await send(recipient, digest(rule, agent, lane, item, since, now), action)) {
        if (previous) await kv.set(key, previous);
        else await forget(key);
        return;
      }
      if (rule === 'eta_passed' && count === 1) {
        if (peerWait(item.wait)) {
          await send(agent, digest(rule, agent, lane, item, since, now, agent, item.wait.on), 'piggyback');
        }
        for (const [owner, waiting] of reverse.get(JSON.stringify([agent, item.id])) ?? []) {
          await send(owner, digest(rule, agent, lane, item, since, now, owner, agent, waiting.item, waiting.lane), 'piggyback');
        }
      }
    } catch (error) {
      if (error.data?.code !== 'kv_limit') throw error;
      log('board hygiene memory refused', {agent, rule, code: error.data?.code});
    }
  }

  // Prune before adding memory, allowing disappeared boards to free quota this run.
  const live = new Set();
  for (const {agent_id: agent, board} of boards) {
    for (const lane of board.lanes) {
      live.add(memoryKey(agent, lane.id, 'lane_done'));
      for (const item of lane.items) {
        for (const rule of ['wait_ended', 'eta_passed', 'stale_active']) live.add(memoryKey(agent, item.id, rule));
      }
    }
  }
  for (const index of indexes) {
    for (const key of [...index]) if (!live.has(key)) await forget(key);
  }

  for (const {agent_id: agent, board} of boards) {
    const staleCandidate = board.lanes.some(lane => lane.items.some(item =>
      item.state === 'active' && Number.isFinite(item.changed_at_ms) && now - item.changed_at_ms >= 6 * HOUR));
    let activity;
    if (staleCandidate) {
      try {
        activity = (await facts(agent, {include: ['activity']})).activity?.last_activity_at;
      } catch (error) {
        log('board hygiene activity unavailable', {agent, code: error.data?.code ?? String(error)});
      }
    }
    for (const lane of board.lanes) {
      for (const item of lane.items) {
        const terminal = item.wait?.ref_terminal_at_ms;
        await check('wait_ended', agent, lane, item,
          item.state === 'blocked' && item.wait?.ref_state === 'terminal' &&
            Number.isFinite(terminal) && item.changed_at_ms < terminal,
          terminal, 30 * MINUTE, agent);
        await check('eta_passed', agent, lane, item,
          item.state !== 'done' && Number.isFinite(item.eta_ms) && !itemRef(item.wait),
          item.eta_ms, 0, peerWait(item.wait) ? item.wait.on : agent);
        await check('stale_active', agent, lane, item,
          item.state === 'active' && Number.isFinite(item.changed_at_ms) &&
            activity?.status === 'ok' && Number.isFinite(activity.value) && activity.value > item.changed_at_ms,
          item.changed_at_ms, 6 * HOUR, agent);
      }
      const newest = Math.max(...lane.items.map(item => item.changed_at_ms ?? lane.updated_at_ms));
      await check('lane_done', agent, lane, null,
        lane.items.length > 0 && lane.items.every(item => item.state === 'done') && Number.isFinite(newest),
        newest, 30 * MINUTE, agent);
    }
  }
  // A successful full scan is the only authority to remove unvisited rule entries.
  for (const index of indexes) {
    for (const key of [...index]) if (!seen.has(key)) await forget(key);
  }
  return {boards_scanned: boards.length, refusals};
}

return await run();
