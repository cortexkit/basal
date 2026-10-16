const MINUTE = 60 * 1000;
const HOUR = 60 * MINUTE;
const INDEX_SHARDS = 16;
const INDEX_CAP = 128;
let boardOwners = new Set();

function utf8Bytes(value) {
  // Native regexp scanning skips ASCII without running JavaScript per character.
  return value.replace(/[\u0080-\u07ff]|[\ud800-\udbff][\udc00-\udfff]|[\u0800-\uffff]/g,
    char => char.length === 2 ? 'xxxx' : char.charCodeAt(0) <= 0x7ff ? 'xx' : 'xxx').length;
}

function cut(value, bytes) {
  value = String(value);
  if (value.length <= bytes && utf8Bytes(value) <= bytes) return value;
  let out = '';
  let size = 0;
  for (const char of value) {
    const code = char.codePointAt(0);
    const next = code <= 0x7f ? 1 : code <= 0x7ff ? 2 : code <= 0xffff ? 3 : 4;
    if (size + next > bytes) break;
    out += char;
    size += next;
  }
  return out;
}

function bounded(item, bodyLimit = 4096, dataLimit = 8192) {
  item.title = cut(item.title, 200).slice(0, 200);
  item.body = cut(item.body, bodyLimit);
  // Cache JSON string sizes; replacing a string leaves keys and punctuation unchanged.
  const size = value => utf8Bytes(JSON.stringify(value));
  let dataBytes = size(item.data);
  if (dataBytes > dataLimit) {
    const fields = [];
    function strings(object) {
      for (const key of Object.keys(object)) {
        if (typeof object[key] === 'string') fields.push({object, key, bytes: utf8Bytes(object[key]), jsonBytes: size(object[key])});
        else if (object[key] && typeof object[key] === 'object') strings(object[key]);
      }
    }
    strings(item.data);
    while (dataBytes > dataLimit) {
      let longest = fields[0];
      for (const field of fields) if (field.bytes > longest.bytes) longest = field;
      const {object, key} = longest;
      object[key] = cut(object[key], Math.floor(longest.bytes / 2));
      const next = size(object[key]);
      dataBytes += next - longest.jsonBytes;
      longest.bytes = utf8Bytes(object[key]);
      longest.jsonBytes = next;
    }
  }
  let itemBytes = size(item);
  let bodyBytes = size(item.body);
  while (itemBytes > 16384) {
    item.body = cut(item.body, Math.floor(utf8Bytes(item.body) / 2));
    const next = size(item.body);
    itemBytes += next - bodyBytes;
    bodyBytes = next;
  }
  return item;
}

function replacementKey(data) {
  const key = `board_nudge:${data.agent_id}:${data.rule}`;
  return utf8Bytes(key) <= 200 ? key : null;
}

function itemRef(wait) {
  return typeof wait?.ref === 'string' ? /^item:([^/]+)\/(.+)$/.exec(wait.ref) : null;
}

function boardLanes(board, skipped) {
  const blocks = Array.isArray(board.laneBlocks) ? board.laneBlocks : [];
  const lanes = [];
  for (const lane of blocks) {
    if (lane === null || typeof lane !== 'object' || Array.isArray(lane) ||
        typeof lane.id !== 'string' || !Array.isArray(lane.items)) {
      skipped.lane_blocks++;
      continue;
    }
    const items = lane.items.filter(item => {
      if (item !== null && typeof item === 'object' && !Array.isArray(item) &&
          typeof item.id === 'string' && typeof item.state === 'string') return true;
      skipped.lane_items++;
      return false;
    });
    lanes.push({...lane, items});
  }
  return lanes;
}

function peerWait(wait) {
  return typeof wait?.on === 'string' && (boardOwners.has(wait.on) || /^(ag_|agent_)/.test(wait.on)) &&
    (wait.ref == null || (typeof wait.ref === 'string' && wait.ref.startsWith('peer:')));
}

function age(now, since) {
  const minutes = Math.max(0, Math.floor((now - since) / MINUTE));
  return minutes < 60 ? `${minutes} min ago` : `${Math.floor(minutes / 60)} h ago`;
}

function entry(rule, agent, lane, item, since, now) {
  const data = {kind: 'board_nudge', rule, agent_id: agent, lane_id: lane.id,
    lane_title: lane.title, since_ms: since};
  if (item) {
    data.item_id = item.id;
    data.item_text = item.text;
    const current = item;
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
  const text = cut(item?.text ?? lane.title, 600);
  const call = `board update: item ${JSON.stringify(text)} → done`;
  let title;
  let body;
  if (rule === 'wait_ended') {
    title = `Wait ended: ${text}`;
    body = `Your wait ended ${age(now, since)}. Update your item: ${call}.`;
  } else if (rule === 'eta_passed') {
    title = `ETA passed: ${text}`;
    if (peerWait(item.wait)) {
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
  body = `${JSON.stringify(cut(lane.title ?? lane.id, 200))}: ${JSON.stringify(text)}. ${body}`;
  // Leave room for the digest envelope and its omitted-entry count.
  return bounded({title, body, data}, 3600, 7000);
}

function memoryKey(agent, rule) {
  const key = `nudge:${agent}:${rule}`;
  // Refuse an oversized owner key rather than aliasing two owners.
  return utf8Bytes(key) <= 256 ? key : null;
}

function digest(agent, rule, entries, now) {
  const count = entries.length;
  const noun = count === 1 ? 'board item' : 'board items';
  const title = rule === 'eta_passed' ? `${count} ${noun} ${count === 1 ? 'is' : 'are'} past ${count === 1 ? 'its' : 'their'} ETA.` :
    rule === 'wait_ended' ? `${count} ${noun} with ended waits.` :
    rule === 'stale_active' ? `${count} stale active ${noun}.` : `${count} completed board ${count === 1 ? 'lane' : 'lanes'}.`;
  const result = {title, body: '', data: {kind: 'board_nudge', rule, agent_id: agent,
    entries: [], total_count: count, more_count: count}};
  const lines = [];
  const size = value => utf8Bytes(JSON.stringify(value));
  const emptyDataBytes = size(result.data);
  const envelopeBytes = size(result) - emptyDataBytes - size('');
  let bodyBytes = 0;
  let bodyJsonBytes = 0;
  let entryBytes = 0;
  entries.sort((a, b) => a.since - b.since || (a.member < b.member ? -1 : a.member > b.member ? 1 : 0));
  for (const candidate of entries) {
    const next = entry(rule, candidate.agent, candidate.lane, candidate.item, candidate.since, now);
    const remaining = count - lines.length - 1;
    const suffix = remaining ? `\n\nand ${remaining} more` : '';
    const separator = lines.length ? 2 : 0;
    const nextBodyBytes = bodyBytes + separator + utf8Bytes(next.body);
    const nextBodyJsonBytes = bodyJsonBytes + 2 * separator + size(next.body) - 2;
    const nextEntryBytes = entryBytes + (lines.length ? 1 : 0) + size(next.data);
    const dataBytes = emptyDataBytes + nextEntryBytes + String(remaining).length - String(count).length;
    if (nextBodyBytes + utf8Bytes(suffix) > 4096 || dataBytes > 8192 ||
        envelopeBytes + dataBytes + nextBodyJsonBytes + size(suffix) > 16384) break;
    lines.push(next.body);
    result.data.entries.push(next.data);
    result.data.more_count = remaining;
    bodyBytes = nextBodyBytes;
    bodyJsonBytes = nextBodyJsonBytes;
    entryBytes = nextEntryBytes;
  }
  result.body = lines.length ? [...lines, ...(result.data.more_count ? [`and ${result.data.more_count} more`] : [])].join('\n\n') :
    count ? `and ${count} more` : 'Nothing is outstanding.';
  return result;
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
  const skipped = {lane_blocks: 0, lane_items: 0};
  const owners = boards.map(({agent_id, board}) => ({
    agent_id, lanes: boardLanes(board, skipped),
  }));
  const indexes = [];
  for (let i = 0; i < INDEX_SHARDS; i++) indexes.push(await kv.get(`index:${i}`) ?? []);
  const groups = new Map();

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
      // Core name characters need no JSON escaping: 128 keys of at most 256
      // UTF-8 bytes, plus array punctuation, fit below the 64 KiB value limit.
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

  function collect(rule, agent, lane, item, since, recipient) {
    const key = memoryKey(recipient, rule);
    if (key === null) {
      log('board hygiene memory key too long', {agent: recipient, rule});
      return;
    }
    if (!groups.has(key)) groups.set(key, {agent: recipient, rule, entries: new Map()});
    const member = JSON.stringify([agent, item?.id ?? lane.id]);
    // Delay rendering until the group's complete membership fits in KV (64 KiB).
    // digest() renders candidates only until the first one exceeds its size limits.
    groups.get(key).entries.set(member, {agent, lane, item, since, member});
  }

  function check(rule, agent, lane, item, condition, since, firstDelay, recipient) {
    if (!condition || now < since + firstDelay) return;
    collect(rule, agent, lane, item, since, recipient);
  }

  for (const {agent_id: agent, lanes} of owners) {
    const staleCandidate = lanes.some(lane => lane.items.some(item =>
      item.state === 'active' && Number.isFinite(item.changed_at_ms) && now - item.changed_at_ms >= 6 * HOUR));
    let activity;
    if (staleCandidate) {
      try {
        activity = (await facts(agent, {fields: ['activity']})).activity?.last_activity_at;
      } catch (error) {
        log('board hygiene activity unavailable', {agent, code: error.data?.code ?? String(error)});
      }
    }
    for (const lane of lanes) {
      if (lane.status === 'retired') continue;
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
  // Clear empty lists silently before freeing their membership quota. Unsupported
  // replacement still uses the same silent fallback, so cleanup never wakes an owner.
  for (const index of indexes) {
    for (const key of [...index]) {
      if (groups.has(key)) continue;
      const previous = await kv.get(key);
      if (previous?.agent && previous?.rule) await send(previous.agent, digest(previous.agent, previous.rule, []), 'silent');
      await forget(key);
    }
  }
  for (const [key, group] of groups) {
    const {agent, rule} = group;
    try {
      const previous = await kv.get(key);
      const members = [...group.entries.keys()].sort();
      const previousMembers = new Set(previous?.members ?? []);
      const joined = members.some(member => !previousMembers.has(member));
      const action = joined ? 'wake' : 'silent';
      const value = {agent, rule, members};
      // Never save partial membership: omitted members would look new on every scan.
      if (utf8Bytes(JSON.stringify(value)) > 65536) {
        log('board hygiene membership too large', {agent, rule});
        continue;
      }
      // Reserve memory before delivery so exhausted KV cannot create repeated wakes.
      if (!await remember(key, value)) continue;
      if (!await send(agent, digest(agent, rule, [...group.entries.values()], now), action)) {
        if (previous) await kv.set(key, previous);
        else await forget(key);
      }
    } catch (error) {
      if (error.data?.code !== 'kv_limit') throw error;
      log('board hygiene memory refused', {agent, rule, code: error.data?.code});
    }
  }
  return {boards_scanned: boards.length, refusals, skipped_lane_blocks: skipped.lane_blocks,
    skipped_lane_items: skipped.lane_items};
}

return await run();
