const MINUTE = 60 * 1000;
const HOUR = 60 * MINUTE;
const INDEX_SHARDS = 16;
const INDEX_CAP = 128;
// A merged delivery nudges its owner once it has been seen merged and
// unsettled for this long, counted from the first run that saw it merged:
// core does not report when the merge happened.
const UNSETTLED_DELAY = 20 * MINUTE;
// One KV value holds the first-seen time of every merged, owned, unsettled
// delivery. At most 384 ids of at most 128 bytes, each with a timestamp, keep
// it below the 64 KiB value limit.
const UNSETTLED_SEEN_KEY = 'unsettled_seen';
const UNSETTLED_SEEN_CAP = 384;
const UNSETTLED_ID_BYTES = 128;
const UNSETTLED_REF_BYTES = 256;
// Dependents listed per delivery; the rest are counted.
const UNSETTLED_BLOCKS_SHOWN = 16;
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

// A wait whose peer declined, or that has otherwise ended, is the owner's to
// act on: never nudge the peer about it.
function peerWait(wait) {
  if (wait?.ref_state === 'terminal' || wait?.declined_by != null) return false;
  return typeof wait?.on === 'string' && (boardOwners.has(wait.on) || /^(ag_|agent_)/.test(wait.on)) &&
    (wait.ref == null || (typeof wait.ref === 'string' && wait.ref.startsWith('peer:')));
}

// A duration for prose: minutes under an hour, hours under two days, then days.
function span(now, since) {
  const minutes = Math.max(0, Math.floor((now - since) / MINUTE));
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours} h`;
  const days = Math.floor(hours / 24);
  return `${days} days`;
}

function age(now, since) {
  return `${span(now, since)} ago`;
}

function shortText(value, max = 80) {
  const text = String(value ?? '');
  const chars = Array.from(text);
  return chars.length > max ? `${chars.slice(0, max - 1).join('')}…` : text;
}

function etaTime(now, eta) {
  const date = new Date(eta);
  const current = new Date(now);
  if (!Number.isFinite(date.getTime())) return 'unknown time';
  const time = `${String(date.getUTCHours()).padStart(2, '0')}:${String(date.getUTCMinutes()).padStart(2, '0')}Z`;
  if (date.getUTCFullYear() === current.getUTCFullYear() &&
      date.getUTCMonth() === current.getUTCMonth() && date.getUTCDate() === current.getUTCDate()) return time;
  return `${String(date.getUTCMonth() + 1).padStart(2, '0')}-${String(date.getUTCDate()).padStart(2, '0')} ${time}`;
}

function itemCondition(rule, agent, item, since, now) {
  if (rule === 'wait_ended') {
    return `wait on ${shortText(item.wait?.ref ?? item.wait?.on ?? 'its wait')} ended ${age(now, since)}`;
  }
  if (rule === 'eta_passed') {
    return peerWait(item.wait) ?
      `${shortText(agent)} waits on you, ETA passed ${etaTime(now, since)}` :
      `ETA passed ${etaTime(now, since)}`;
  }
  if (rule === 'stale_pending') return `planned, idle ${span(now, since)}, no ETA`;
  if (rule === 'stale_wait') {
    if (item.wait?.on === 'user' && item.wait?.ref == null) {
      return `waiting ${span(now, since)} on the user with no ask`;
    }
    return `waiting ${span(now, since)} on ${shortText(item.wait?.ref ?? item.wait?.on ?? 'its wait')}, no ETA`;
  }
  if (rule === 'stale_active') return `active, unchanged ${span(now, since)}`;
  return `all items done ${age(now, since)}`;
}

function digestInstruction(rule, entries) {
  if (rule === 'wait_ended') return 'Update each item to done.';
  if (rule === 'eta_passed') {
    return entries.some(({item}) => peerWait(item?.wait)) ?
      'For your items, mark each done or set a new ETA; for peer waits, commit with an item on your own board with an ETA, or decline to the waiting owner.' :
      'Mark each done, or set a new ETA.';
  }
  if (rule === 'stale_pending') {
    return 'Start each, make it wait on whoever it depends on (a peer, or an ask for the user), set an ETA, or remove it.';
  }
  if (rule === 'stale_wait') {
    return entries.some(({item}) => item?.wait?.on === 'user' && item.wait?.ref == null) ?
      'Check each wait and update its item or set a new ETA; for a user wait with no ask, file an ask so the user receives the question.' :
      'Check each wait and update its item, or set a new ETA.';
  }
  if (rule === 'stale_active') return 'Update each item, or set a new ETA.';
  if (rule === 'unsettled_merged') return 'Score and settle each delivery.';
  return 'Retire each completed lane.';
}

// The `blocks` dependents that are well-formed enough to name.
function workBlocks(work) {
  return Array.isArray(work.blocks) ? work.blocks.filter(block =>
    block !== null && typeof block === 'object' && typeof block.item_id === 'string') : [];
}

// One merged-but-unsettled delivery, for its owner's unsettled_merged list.
function workEntry(agent, work, since, now) {
  const blocks = workBlocks(work);
  const shown = blocks.slice(0, UNSETTLED_BLOCKS_SHOWN).map(block => ({
    item_id: cut(block.item_id, UNSETTLED_ID_BYTES),
    title: cut(typeof block.title === 'string' ? block.title : block.item_id, 200),
  }));
  const data = {kind: 'board_nudge', rule: 'unsettled_merged', agent_id: agent, item_id: work.item_id,
    item_title: work.title, merged_ref: work.merged_ref, since_ms: since, blocks: shown};
  if (blocks.length > shown.length) data.more_blocks = blocks.length - shown.length;
  if (typeof work.campaign_id === 'string') data.campaign_id = work.campaign_id;
  if (Number.isFinite(work.delivered_at_ms)) data.delivered_at_ms = work.delivered_at_ms;
  const text = cut(work.title, 600);
  const condition = `merged into ${shortText(work.merged_ref)}, unsettled ${span(now, since)}` +
    (typeof work.campaign_id === 'string' && blocks.length ?
      `; blocks ${blocks.length} ${blocks.length === 1 ? 'slice' : 'slices'}` : '');
  const body = `- ${JSON.stringify(shortText(work.title ?? work.item_id))} (${work.item_id}): ${condition}`;
  return bounded({title: `Merged, not settled: ${text}`, body, data}, 3600, 7000);
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
  let title;
  if (rule === 'wait_ended') {
    title = `Wait ended: ${text}`;
  } else if (rule === 'eta_passed') {
    title = `ETA passed: ${text}`;
  } else if (rule === 'stale_pending') {
    title = `Planned item idle: ${text}`;
  } else if (rule === 'stale_wait') {
    title = `Wait unchanged: ${text}`;
  } else if (rule === 'stale_active') {
    title = `Active item unchanged: ${text}`;
  } else {
    title = `Lane complete: ${text}`;
  }
  const laneTitle = shortText(lane.title ?? lane.id);
  const condition = item ? itemCondition(rule, agent, item, since, now) : `all items done ${age(now, since)}`;
  const body = rule === 'lane_done' ?
    `- ${JSON.stringify(laneTitle)}: ${condition}` :
    `- ${JSON.stringify(laneTitle)}: ${JSON.stringify(shortText(item?.text ?? lane.title ?? lane.id))} (${item.id}): ${condition}`;
  return bounded({title, body, data}, 3600, 7000);
}

// What a list shows apart from its ages: an edit to an item's text, lane or
// start time changes it, while time passing does not. FNV-1a, 32 bits.
function contentFingerprint(entries) {
  const parts = entries.map(({member, lane, item, work, since}) => work ?
    JSON.stringify([member, work.title, work.merged_ref, work.campaign_id ?? null,
      workBlocks(work).map(block => [block.item_id, block.title ?? null]), since]) :
    JSON.stringify([member, lane.id, lane.title, item?.text ?? null, item?.state ?? null,
      item?.eta_ms ?? null, item?.wait?.ref ?? null, since]));
  parts.sort();
  let hash = 0x811c9dc5;
  for (const ch of parts.join('\n')) {
    hash ^= ch.codePointAt(0);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash.toString(16).padStart(8, '0');
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
    rule === 'stale_wait' ? `${count} ${noun} waiting with no ETA.` :
    rule === 'stale_pending' ? `${count} planned ${noun} idle with no ETA.` :
    rule === 'stale_active' ? `${count} stale active ${noun}.` :
    rule === 'unsettled_merged' ? `${count} merged ${count === 1 ? 'delivery is' : 'deliveries are'} waiting to be settled.` :
    `${count} completed board ${count === 1 ? 'lane' : 'lanes'}.`;
  const result = {title, body: '', data: {kind: 'board_nudge', rule, agent_id: agent,
    entries: [], total_count: count, more_count: count}};
  const instruction = digestInstruction(rule, entries);
  const lines = [];
  const size = value => utf8Bytes(JSON.stringify(value));
  const emptyDataBytes = size(result.data);
  const envelopeBytes = size(result) - emptyDataBytes - size('');
  let bodyBytes = utf8Bytes(instruction);
  let bodyJsonBytes = size(instruction) - 2;
  let entryBytes = 0;
  entries.sort((a, b) => a.since - b.since || (a.member < b.member ? -1 : a.member > b.member ? 1 : 0));
  for (const candidate of entries) {
    const next = candidate.work ? workEntry(candidate.agent, candidate.work, candidate.since, now) :
      entry(rule, candidate.agent, candidate.lane, candidate.item, candidate.since, now);
    const remaining = count - lines.length - 1;
    const suffix = remaining ? `\nand ${remaining} more` : '';
    const separator = 1;
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
  result.body = count ?
    [instruction, ...lines, ...(result.data.more_count ? [`and ${result.data.more_count} more`] : [])].join('\n') :
    'Nothing is outstanding.';
  return result;
}

function shardFor(key) {
  let hash = 0;
  for (const char of key) hash = (hash * 31 + char.codePointAt(0)) >>> 0;
  return hash % INDEX_SHARDS;
}

// Every page of core's list of delivered work that its owner has not settled.
// A page cut short by core's time budget still carries next_cursor, so follow
// the cursor rather than counting items. Any failure fails the whole read: a
// partial list would make missing items look settled.
async function readUnsettled() {
  const items = [];
  const cursors = new Set();
  let cursor = null;
  do {
    const page = await ops.call('prefrontal-core', 'work.unsettled', cursor === null ? {} : {cursor});
    if (!Array.isArray(page?.items)) throw new Error('work.unsettled reply has no items list');
    items.push(...page.items);
    cursor = page.next_cursor ?? null;
    if (cursor !== null && (typeof cursor !== 'string' || cursors.has(cursor))) {
      throw new Error('work.unsettled repeated or garbled its cursor');
    }
    cursors.add(cursor);
  } while (cursor !== null);
  return items;
}

// Whether a work.unsettled item has the fields this flow relies on. A merged
// item must name its integration ref, which the settle action needs.
function unsettledItem(work) {
  return work !== null && typeof work === 'object' && !Array.isArray(work) &&
    typeof work.item_id === 'string' && work.item_id !== '' &&
    utf8Bytes(work.item_id) <= UNSETTLED_ID_BYTES &&
    typeof work.title === 'string' && typeof work.delivery === 'string' &&
    (work.agent_id === null || (typeof work.agent_id === 'string' && work.agent_id !== '')) &&
    (work.delivery !== 'merged' || (typeof work.merged_ref === 'string' && work.merged_ref !== '' &&
      utf8Bytes(work.merged_ref) <= UNSETTLED_REF_BYTES));
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

  let withdrawalUnsupportedLogged = false;
  async function clearList(agent, item) {
    const key = replacementKey(item.data);
    if (key === null) return send(agent, item, 'silent');
    try {
      await sink.digest.withdraw(agent, {replace_key: key});
      return true;
    } catch (error) {
      if (error.data?.code !== 'digest_withdraw_unsupported') {
        log('board hygiene digest withdrawal refused', {agent, code: error.data?.code ?? String(error)});
        return false;
      }
      if (!withdrawalUnsupportedLogged) {
        log('board hygiene digest withdrawal unsupported', {code: error.data.code});
        withdrawalUnsupportedLogged = true;
      }
      return send(agent, item, 'silent');
    }
  }

  function collect(rule, agent, lane, item, since, recipient, work) {
    const key = memoryKey(recipient, rule);
    if (key === null) {
      log('board hygiene memory key too long', {agent: recipient, rule});
      return;
    }
    if (!groups.has(key)) groups.set(key, {agent: recipient, rule, entries: new Map()});
    const member = JSON.stringify([agent, work?.item_id ?? item?.id ?? lane.id]);
    // Delay rendering until the group's complete membership fits in KV (64 KiB).
    // digest() renders candidates only until the first one exceeds its size limits.
    groups.get(key).entries.set(member, {agent, lane, item, work, since, member});
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
        const group = (await facts(agent, {fields: ['activity']})).activity;
        // primary_turn_end_at is stored by core and survives its restarts;
        // last_activity_at lives only in core's memory, so it is unknown after a
        // restart until core observes the head again. Use the later known value.
        activity = [group?.primary_turn_end_at, group?.last_activity_at]
          .filter(fact => fact?.status === 'ok' && Number.isFinite(fact.value))
          .reduce((latest, fact) => (latest && latest.value >= fact.value ? latest : fact), undefined);
      } catch (error) {
        log('board hygiene activity unavailable', {agent, code: error.data?.code ?? String(error)});
      }
    }
    for (const lane of lanes) {
      if (lane.status === 'retired') continue;
      for (const item of lane.items) {
        // An item still blocked on a wait that has ended is the owner's to move,
        // even when it changed after the end or the wait was set on something
        // already finished. The 30 minutes count from the later of the end and
        // the item's last change, so an owner mid-update isn't nudged.
        const terminal = item.wait?.ref_terminal_at_ms;
        await check('wait_ended', agent, lane, item,
          item.state === 'blocked' && item.wait?.ref_state === 'terminal' &&
            Number.isFinite(terminal) &&
            now >= Math.max(terminal, Number.isFinite(item.changed_at_ms) ? item.changed_at_ms : terminal) + 30 * MINUTE,
          terminal, 0, agent);
        await check('eta_passed', agent, lane, item,
          item.state !== 'done' && Number.isFinite(item.eta_ms) && !itemRef(item.wait),
          item.eta_ms, 0, peerWait(item.wait) ? item.wait.on : agent);
        // A blocked item whose wait has not changed for a day, with no ETA to
        // bound it, may be waiting on something that went silent (a worker that
        // never finished, a peer that never answered). Ended or declined waits
        // are covered by wait_ended; an ETA hands the item to eta_passed. A wait
        // on an ask is the user's to answer: the owner can't move it and the ask
        // already reaches the user, so it is left out.
        const waitSince = Math.max(
          Number.isFinite(item.wait?.since_ms) ? item.wait.since_ms : -Infinity,
          Number.isFinite(item.changed_at_ms) ? item.changed_at_ms : -Infinity);
        await check('stale_wait', agent, lane, item,
          item.state === 'blocked' && !Number.isFinite(item.eta_ms) &&
            item.wait?.ref_state !== 'terminal' && item.wait?.declined_by == null &&
            !(typeof item.wait?.ref === 'string' && item.wait.ref.startsWith('ask:')) &&
            Number.isFinite(waitSince),
          waitSince, 24 * HOUR, agent);
        // A planned item with no ETA that nobody has touched for 6 hours
        // usually needs a decision now: start it, wait it on its dependency
        // (a peer, or an ask for the user), date it, or drop it.
        await check('stale_pending', agent, lane, item,
          item.state === 'pending' && !Number.isFinite(item.eta_ms) && Number.isFinite(item.changed_at_ms),
          item.changed_at_ms, 6 * HOUR, agent);
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
  // Merged deliveries their owner has not settled block the campaign slices
  // waiting on them. A failed read is logged and leaves this rule's lists and
  // memory as they were, so the other rules still run and nothing is cleared
  // on missing data.
  const unsettled = {scanned: 0, unowned: 0, malformed: 0, error: null};
  let works = null;
  try {
    works = await readUnsettled();
  } catch (error) {
    unsettled.error = error.data?.code ?? String(error);
    log('board hygiene unsettled read failed', {code: unsettled.error});
  }
  if (works !== null) {
    unsettled.scanned = works.length;
    const stored = await kv.get(UNSETTLED_SEEN_KEY);
    const before = new Map((Array.isArray(stored) ? stored : []).filter(pair =>
      Array.isArray(pair) && typeof pair[0] === 'string' && Number.isFinite(pair[1])));
    const seen = new Map();
    const fresh = [];
    const merged = new Map();
    for (const work of works) {
      if (!unsettledItem(work)) {
        unsettled.malformed++;
        continue;
      }
      const kept = before.get(work.item_id);
      // Only `unmerged` and `none` say the delivery is not merged, so only
      // they restart its clock. `unknown` and `error` mean core had no Git
      // facts, and a missing owner says nothing about the merge: both keep
      // the recorded time without nudging anyone.
      if (work.delivery === 'unmerged' || work.delivery === 'none') continue;
      if (work.delivery === 'merged' && work.agent_id === null) unsettled.unowned++;
      if (work.delivery !== 'merged' || work.agent_id === null) {
        if (kept !== undefined) seen.set(work.item_id, kept);
        continue;
      }
      if (kept !== undefined) seen.set(work.item_id, kept);
      else fresh.push(work.item_id);
      merged.set(work.item_id, work);
    }
    // Items already remembered keep their place; new ones fill what is left.
    let full = false;
    for (const id of fresh) {
      if (seen.has(id)) continue;
      if (seen.size >= UNSETTLED_SEEN_CAP) {
        full = true;
        continue;
      }
      seen.set(id, now);
    }
    if (full) log('board hygiene unsettled memory full', {cap: UNSETTLED_SEEN_CAP});
    for (const [id, work] of merged) {
      const since = seen.get(id);
      if (since === undefined || now < since + UNSETTLED_DELAY) continue;
      collect('unsettled_merged', work.agent_id, null, null, since, work.agent_id, work);
    }
    // Items that left the list are forgotten; write only when memory changed.
    const next = [...seen].sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
    if (JSON.stringify(next) !== JSON.stringify(stored ?? [])) {
      try {
        if (next.length) await kv.set(UNSETTLED_SEEN_KEY, next);
        else await kv.delete(UNSETTLED_SEEN_KEY);
      } catch (error) {
        if (error.data?.code !== 'kv_limit') throw error;
        log('board hygiene memory refused', {rule: 'unsettled_merged', code: error.data.code});
      }
    }
  }
  // Remove empty lists' pending items before freeing their membership quota.
  // A catalog without withdrawal support gets a silent empty replacement instead,
  // so cleanup never wakes an owner.
  for (const index of indexes) {
    for (const key of [...index]) {
      if (groups.has(key)) continue;
      if (works === null && key.endsWith(':unsettled_merged')) continue;
      const previous = await kv.get(key);
      if (previous?.agent && previous?.rule) {
        await clearList(previous.agent, digest(previous.agent, previous.rule, []));
      }
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
      const fingerprint = contentFingerprint([...group.entries.values()]);
      // Write only when the list changed. Rewriting an unchanged list every
      // five minutes replaced a still-undelivered wake with a silent update.
      if (!joined && members.length === previousMembers.size && previous?.fingerprint === fingerprint) continue;
      const value = {agent, rule, members, fingerprint};
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
    skipped_lane_items: skipped.lane_items, unsettled_scanned: unsettled.scanned,
    unsettled_read_error: unsettled.error, skipped_unsettled_unowned: unsettled.unowned,
    skipped_unsettled_items: unsettled.malformed};
}

return await run();
