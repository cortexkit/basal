// BEGIN OWNERS: repository -> owning agent, copied from the CI failure router
// (flows/ci-failure-router/flow.js); flow scripts cannot import shared constants.
const OWNERS = {
  'cortexkit/aft': 'agent_ab64ea0873bbee0c', // AFT
  'cortexkit/prefrontal': 'agent_fff43bb5f4bea20b', // ALF
  'cortexkit/magic-context': 'agent_b613e5cf2ee55b8c', // MC
  'cortexkit/subconscious': 'agent_2bfd6927602f7977', // SUBC
  'cortexkit/subconscious-design': 'agent_2bfd6927602f7977', // SUBC
  'cortexkit/commons': 'agent_2bfd6927602f7977', // SUBC
  'cortexkit/cingulate': 'agent_2bfd6927602f7977', // SUBC
  'cortexkit/plexus': 'agent_309edd5e94fbb2fe', // PLEX
  'cortexkit/cerebellum': 'agent_b1a9c546c33b84c9', // CEREB
  'cortexkit/basal': 'agent_12eb4c0c42cecdb5', // BASAL
  'cortexkit/broca': 'agent_65e9fe998e14b47d', // BROCA
  'cortexkit/broca-private': 'agent_65e9fe998e14b47d', // BROCA
  'cortexkit/entorhinal': 'agent_fef99bdc81d3110f', // ENTO
  'cortexkit/engram': 'agent_ad4b5a2aa78261f4', // ENGRAM
  'cortexkit/insula': 'agent_2fd02cfeb9c0484f', // QTA
  'cortexkit/claustrum': 'agent_847ef2a2861e98ec', // CKCRED
  'cortexkit/callosum': 'agent_26399f830c6eb22a', // CALLO
  'cortexkit/thalamus': 'agent_62d6b770df582b4a', // THALAMUS
  'cortexkit/fusiform': 'agent_ce8142a135f2ba63', // FUSI
  'cortexkit/motor': 'agent_ef98bc82ef005a63', // MOTOR
  'cortexkit/motor-private': 'agent_ef98bc82ef005a63', // MOTOR
  'cortexkit/synapse': 'agent_80d5db38cd991ef1', // SYNAPSE
  'cortexkit/astrocyte': 'agent_5acf4ca3cb0138c4', // ASTRO
  'cortexkit/cortexkit-cloud': 'agent_fb7fb88282f843b5', // CLOUD
  'cortexkit/cortexkit-account': 'agent_fb7fb88282f843b5', // CLOUD
  'cortexkit/alfonso-ios': 'agent_fcd30f86aefd4fd9', // CKIOS
  'cortexkit/alfonso-tui': 'agent_b54b7f85f177ca5d', // CKTUI
  'cortexkit/alfonso-desktop': 'agent_16013c86', // CKDESK
  'cortexkit/avatar': 'agent_6f5aa47e4c5ad277', // AVA
  'cortexkit/wernicke': 'agent_20dabecd80549388', // WERNI
  'cortexkit/cortexkit-e2e': 'agent_d444250e2d503c07', // CKE2E
  'cortexkit/anthropic-auth': 'agent_74a1517f3695e9b1', // ANTAUTH
  'cortexkit/antigravity-auth': 'agent_87f64a79b52bf905', // AGAUTH
  'cortexkit/openai-auth': 'agent_0f4d7713e64e7cc4', // OAIAUTH
  'cortexkit/common-auth': 'agent_0f4d7713e64e7cc4', // OAIAUTH
};
// END OWNERS

// GitHub App slugs from Plexus's repository-to-agent bindings query (bindings.read),
// snapshot repo_binding_generation 32. The motor and cloud owners have no bound bot:
// a missing slug means no own-bot rule, not an invented bot identity.
const BOT_SLUGS = {
  agent_ab64ea0873bbee0c: 'aft-alfonso',
  agent_16013c86: 'ckdesk-alfonso',
  agent_fcd30f86aefd4fd9: 'ckios-alfonso',
  agent_b54b7f85f177ca5d: 'cktui-alfonso',
  agent_74a1517f3695e9b1: 'antauth-alfonso',
  agent_87f64a79b52bf905: 'agauth-alfonso',
  agent_5acf4ca3cb0138c4: 'astro-alfonso',
  agent_6f5aa47e4c5ad277: 'ava-alfonso',
  agent_12eb4c0c42cecdb5: 'basal-alfonso',
  agent_65e9fe998e14b47d: 'broca-alfonso',
  agent_26399f830c6eb22a: 'callo-alfonso',
  agent_b1a9c546c33b84c9: 'cereb-alfonso',
  agent_847ef2a2861e98ec: 'ckcred-alfonso',
  agent_2bfd6927602f7977: 'subc-alfonso',
  agent_d444250e2d503c07: 'cke2e-alfonso',
  agent_ad4b5a2aa78261f4: 'engram-alfonso',
  agent_fef99bdc81d3110f: 'ento-alfonso',
  agent_ce8142a135f2ba63: 'fusi-alfonso',
  agent_2fd02cfeb9c0484f: 'qta-alfonso',
  agent_b613e5cf2ee55b8c: 'magic-alfonso',
  agent_0f4d7713e64e7cc4: 'oaiauth-alfonso',
  agent_309edd5e94fbb2fe: 'plex-alfonso',
  agent_fff43bb5f4bea20b: 'alf-alfonso',
  agent_80d5db38cd991ef1: 'synapse-alfonso',
  agent_62d6b770df582b4a: 'thalamus-alfonso',
  agent_20dabecd80549388: 'werni-alfonso',
};

// Operator-suppressed GitHub logins: mirrors prefrontal-core's module_config
// setting events_suppress_actors as of 2026-10-16.
const SUPPRESS_ACTORS = ['ualtinok', 'alfonso-aft', 'MC-Alfonso'];
// Project-suppressed logins: mirrors events_suppress_actors:<project_id> as of
// 2026-10-16. Only cortexkit/aft has a row; key by repo because v2 event bodies
// have no project id.
const SUPPRESS_ACTORS_BY_REPO = { 'cortexkit/aft': ['cubic-dev-ai', 'greptile-apps'] };

// Owner actions: a newly opened issue or PR wakes its owner; every other
// accepted event is an update that piggybacks on the owner's next wake.
const ACTION = { opened: 'wake', update: 'piggyback' };
const MAX_AGE_MS = 6 * 60 * 60 * 1000;
const ROLLING_MS = 30 * 60 * 1000;
const CAP_MS = 60 * 60 * 1000;

// Incremental helper from templates/github-ci-watch, adapted to a flow's body
// and item metadata instead of template params. Never reserialize per character.
function boundedDigest(title, body, fields, observed, links = []) {
  const utf8Bytes = text => text.replace(/[\u0080-\u07ff]|[\ud800-\udbff][\udc00-\udfff]|[\u0800-\uffff]/g,
    ch => ch.length === 2 ? 'xxxx' : ch.charCodeAt(0) <= 0x7ff ? 'xx' : 'xxx').length;
  const size = value => utf8Bytes(JSON.stringify(value));
  const data = { ...fields, observed: { ...observed } };
  const item = {
    title: Array.from(title).slice(0, 199).join(''),
    body,
    data,
    links: links.filter(link => link.kind === 'url' &&
      typeof link.url === 'string' && /^https:\/\/[A-Za-z0-9.-]+(?:[/?#]|$)/.test(link.url)),
  };
  let dataBytes = size(data);
  let itemBytes = size(item);
  const mark = (field, text) => {
    if (data.truncated?.[field] != null) return;
    let added = 0;
    if (!data.truncated) {
      data.truncated = {};
      added += ',"truncated":{}'.length;
    }
    const marker = { original_bytes: utf8Bytes(text) };
    added += size({ [field]: marker }) - 2 + (Object.keys(data.truncated).length ? 1 : 0);
    data.truncated[field] = marker;
    dataBytes += added;
    itemBytes += added;
  };
  while (item.links.length && itemBytes > 16384) {
    const link = item.links.pop();
    itemBytes -= size(link) + (item.links.length ? 1 : 0);
    mark('link_url', link.url);
  }
  if (dataBytes > 8192 || itemBytes > 16384) {
    const fields = [];
    for (const key of Object.keys(data.observed)) {
      const text = data.observed[key];
      if (typeof text === 'string' && text.length) fields.push({ key, text, bytes: size(text) });
    }
    while (dataBytes > 8192 || itemBytes > 16384) {
      let longest;
      for (const field of fields) {
        if (field.text.length && (!longest || field.bytes > longest.bytes)) longest = field;
      }
      if (!longest) throw new Error('Digest metadata cannot fit');
      const { key, text } = longest;
      mark(`observed.${key}`, text);
      const chars = Array.from(text);
      const shortened = chars.slice(0, Math.floor(chars.length / 2)).join('');
      const next = size(shortened);
      data.observed[key] = shortened;
      dataBytes += next - longest.bytes;
      itemBytes += next - longest.bytes;
      longest.text = shortened;
      longest.bytes = next;
    }
  }
  return item;
}

function bytes(text) {
  return text.replace(/[\u0080-\u07ff]|[\ud800-\udbff][\udc00-\udfff]|[\u0800-\uffff]/g,
    ch => ch.length === 2 ? 'xxxx' : ch.charCodeAt(0) <= 0x7ff ? 'xx' : 'xxx').length;
}

function short(text, maxBytes) {
  text = String(text ?? 'unknown');
  if (bytes(text) <= maxBytes) return text;
  let result = '';
  let size = 0;
  for (const ch of text) {
    const cp = ch.codePointAt(0);
    const next = cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
    if (size + next > maxBytes - 3) break;
    result += ch;
    size += next;
  }
  return result + '...';
}

// Keep vendor titles inside one quoted line: replace double quotes (not
// apostrophes), '<', and CR/LF exactly as core does (commands.rs:1426-1433).
// README.md identifies the full paths of the cited core source files.
function sanitize(text) {
  return String(text ?? '').replace(/</g, '\u2039').replace(/"/g, "'").replace(/[\r\n]/g, ' ');
}

// Mirrors core's chrono::Local on the operator's Europe/Madrid host
// (commands.rs:955-963). EU DST switches at 01:00 UTC on the last Sundays
// of March and October. UTC getters keep replay independent of worker location.
function madridHHMM(ms) {
  const year = new Date(ms).getUTCFullYear();
  const lastSunday = month => {
    const last = new Date(Date.UTC(year, month + 1, 0));
    return Date.UTC(year, month, last.getUTCDate() - last.getUTCDay(), 1);
  };
  const summer = ms >= lastSunday(2) && ms < lastSunday(9);
  return new Date(ms + (summer ? 2 : 1) * 3600000).toISOString().slice(11, 16);
}

function render(repo, kind, number, action, history, summary) {
  const labels = new Map([['issue', 'Issue'], ['pull_request', 'PR'], ['release', 'Release']]);
  const label = labels.get(kind) ?? sanitize(kind);
  const subject = `${repo} ${label} #${sanitize(number)}`;
  // Map known actions to fixed words: vendor-supplied action text must not
  // enter trusted prose verbatim (commands.rs:997-1004).
  const words = new Map([['opened', 'opened'], ['created', 'new comment'], ['edited', 'edit'],
    ['closed', 'closed'], ['reopened', 'reopened']]);
  const word = words.get(action) ?? 'update';
  let prefix;
  if (history.count === 1) {
    prefix = `- ${subject} ${word} observed at ${madridHHMM(history.times[0])}. `;
  } else {
    // Repeated observations show the first six times, an overflow count, and
    // only the newest summary (commands.rs:1567-1617).
    const times = history.times.map(madridHHMM);
    if (history.count > 6) times.push(`${history.count - 6} more`);
    const last = times.pop();
    prefix = `- ${subject} updated at ${times.join(', ')} and ${last}: `;
  }
  return `${prefix}"${short(sanitize(summary), 4096 - bytes(prefix) - 2)}"`;
}

async function save(memory) {
  // Bound fleet memory below a single KV value's 64 KiB default, including
  // escaped titles. Eviction loses old observation history, not delivery identity.
  while (bytes(JSON.stringify(memory)) > 48000) {
    const oldest = Object.keys(memory.items).sort((a, b) =>
      memory.items[a].latest_ms - memory.items[b].latest_ms)[0];
    if (!oldest) break;
    delete memory.items[oldest];
  }
  await kv.set('github-events', memory);
}

const event = trigger.event;
if (!event || event.module !== 'plexus' || event.version !== 2 ||
    !['github_issue_changed', 'github_pr_changed', 'github_release_published'].includes(event.name)) return;
const body = event.body;
if (!body || typeof body.repo !== 'string' || !/^cortexkit\/[a-z0-9_.-]+$/i.test(body.repo)) return;
const repo = body.repo.toLowerCase();
// No release body is documented here. Assume tag_name (or tag), not a numeric
// issue id; this is the explicit release exception to core's numbered identity.
const release = event.name === 'github_release_published';
const kind = release ? 'release' : body.kind;
const number = release ? body.tag_name ?? body.tag : body.number;
const owner = OWNERS[repo] ?? null;
const isNew = (kind === 'issue' || kind === 'pull_request') && body.action === 'opened';
const comparison = { repo, kind: kind ?? null, number: number ?? null,
  event_key: body.event_key ?? event.event_key, would_owner: owner,
  would_action: isNew ? ACTION.opened : ACTION.update,
  suppressed_reason: null, rule: 'deliver' };
// events[] keeps the routing decision for every event, delivered or dropped,
// so a run result still explains why an event did or did not reach its owner.
const result = { events: [comparison], delivered: 0, unmapped: 0, reaction: null };
const drop = reason => {
  comparison.suppressed_reason = reason;
  comparison.rule = reason;
  return result;
};
// A repository, nonempty kind and item number earn a digest item; there is no
// kind/action whitelist (commands.rs:866-891).
if (typeof kind !== 'string' || !kind || number == null ||
    (release ? typeof number !== 'string' || !number : !Number.isSafeInteger(number))) return drop('missing_identity');
const key = `github:${repo}:${kind}:${number}`;
if (bytes(key) > 200 || /[\u0000-\u001f\u007f]/.test(key)) return drop('invalid_identity');
// Deliberate difference from core: drop rescan events older than six hours, as
// the CI router does, and expose each drop in the comparison result. Core has
// no corresponding deliverable-event cutoff (runtime.rs:904-915).
const timestamp = Date.parse(body.completed_at ?? body.updated_at);
if (Number.isFinite(timestamp) && Date.now() - timestamp > MAX_AGE_MS) return drop('stale');
const memory = await kv.get('github-events') ?? { items: {}, unmapped: 0 };
if (!owner) {
  memory.unmapped = Math.min(Number.MAX_SAFE_INTEGER, memory.unmapped + 1);
  result.unmapped = 1;
  await save(memory);
  return drop('unmapped_repo');
}
// A reopened action forces open even if the body still says closed
// (consumers.rs:1121-1128).
const state = body.action === 'reopened' ? 'open' : body.state;
const actors = body.actor_logins;
// Drop closed/merged PRs only with an explicit empty actor array: missing or
// unknown actor enrichment is not evidence of no activity
// (consumers.rs:1227-1253,1437-1451).
if (kind === 'pull_request' && state === 'closed' &&
    Array.isArray(actors) && actors.length === 0) return drop('closed_pr_empty_actors');
if (body.action === 'suppressed_own_echo') return drop('own_echo');
const slug = BOT_SLUGS[owner];
const bot = slug ? `${slug}[bot]`.toLowerCase() : null;
const ownAuthored = bot != null && String(body.author ?? '').toLowerCase() === bot;
// consumers.rs:1282-1327,1673-1701: authorship is not comment authorship.
// Plexus has no user type or separate reviews; comment_author is its evidence.
const humanComment = typeof body.comment_author === 'string' && body.comment_author.length > 0 &&
  !body.comment_author.toLowerCase().endsWith('[bot]');
if (ownAuthored && !humanComment) return drop('own_bot_automated');
// Logins compare case-insensitively and without a trailing `[bot]`, so a
// review bot's account and its App identity are the same actor.
const login = name => String(name).toLowerCase().replace(/\[bot\]$/, '');
const suppressed = new Set([...SUPPRESS_ACTORS, ...(SUPPRESS_ACTORS_BY_REPO[repo] ?? []), ...(bot ? [bot] : [])]
  .map(login));
const GITHUB_ACTIONS = 'github-actions';
// An agent's own action, and the automation it sets off, never wakes anyone:
// github-actions only acts because someone else did (labellers, gates, check
// reruns). So an event is an own echo when every actor is suppressed, or at
// least one is and every other is github-actions. github-actions alone, any
// human, or any unlisted bot still wakes, and a missing actor set still wakes.
const echoOf = list => Array.isArray(list) && list.length > 0 &&
  list.some(name => suppressed.has(login(name))) &&
  // Every actor must be suppressed or github-actions: one suppressed actor
  // cannot hide a human acting in the same event.
  list.every(name => suppressed.has(login(name)) || login(name) === GITHUB_ACTIONS);
// When the event names who started its run, that account decides: a run a
// suppressed login started, whose actors are all suppressed or github-actions
// (an empty set counts), is an echo; a run anyone else started wakes even if
// the agent also acted.
// Plexus carries a run's starter flat on the body; a nested workflow_run, when
// present, is the run itself. Each starter is a login or an object with one.
const subject = body.workflow_run && typeof body.workflow_run === 'object' ? body.workflow_run : body;
const loginOf = value => typeof value === 'string' ? value :
  value && typeof value === 'object' && typeof value.login === 'string' ? value.login : '';
const starter = [subject.triggering_actor, subject.actor].map(loginOf).find(name => name.length > 0) ?? null;
const echo = starter != null ?
  suppressed.has(login(starter)) && Array.isArray(actors) &&
    actors.every(name => suppressed.has(login(name)) || login(name) === GITHUB_ACTIONS) :
  echoOf(actors);
// A human comment on an own-authored subject bypasses this gate too
// (consumers.rs:1397-1411,1696-1701).
if (!(ownAuthored && humanComment) && echo) return drop('suppressed_actor_set');

const now = Date.now();
const prior = memory.items[key];
if (prior?.event_keys.includes(comparison.event_key)) return drop('duplicate_event');
// Opened issues/PRs bypass update debounce. Updates would become due 30 minutes
// after the latest observation, capped at 60 minutes after the first update.
// Event-only flows cannot schedule trailing-edge runs: deliver now, but record
// the modeled due time (commands.rs:424-491,751-815).
const first = !prior || prior.is_new || prior.due_ms <= now || isNew ? now : prior.first_seen_ms;
const history = prior ?? { count: 0, times: [], event_keys: [] };
history.count = Math.min(Number.MAX_SAFE_INTEGER, history.count + 1);
if (history.times.length < 6) history.times.push(now);
history.event_keys.push(comparison.event_key);
history.event_keys = history.event_keys.slice(-64);
history.first_seen_ms = first;
history.latest_ms = now;
history.due_ms = isNew ? now : Math.min(now + ROLLING_MS, first + CAP_MS);
history.is_new = isNew;
Object.assign(comparison, { state: state ?? null, first_seen_ms: first, latest_ms: now,
  due_ms: history.due_ms, observation_count: history.count, observation_times: history.times,
  overflow: Math.max(0, history.count - 6) });
const summary = String(body.title ?? '');
// The delivered item carries the item's identity and observation history, not
// the routing comparison fields (would_*, suppressed_reason, rule), which stay
// in the run result's events[] only.
const fields = { repo, kind, number, event_key: comparison.event_key, state: comparison.state,
  first_seen_ms: first, latest_ms: now, due_ms: history.due_ms, observation_count: history.count,
  observation_times: history.times, overflow: comparison.overflow };
const item = boundedDigest(sanitize(`${repo} ${kind} #${number}`), render(repo, kind, number, body.action, history, summary),
  fields, { summary }, [{ kind: 'url', url: body.url }]);
await sink.digest(owner, item, comparison.would_action, { replace_key: key });
result.delivered = 1;
// React once, on the first delivery of a newly opened issue or PR. Updates and
// releases never react. The history records the attempt before the call, so
// a later event for the same item never reacts again, whatever the outcome.
const react = isNew && !prior?.reacted;
if (react) history.reacted = true;
memory.items[key] = history;
await save(memory);
if (react) {
  // Plexus keys the nonce per (agent, nonce) so a retried request replays its
  // first outcome instead of reacting twice. Admission starts at most one run
  // of this flow per event_key, so the event_key already identifies the run;
  // the flow id prefix keeps the nonce distinct from other flows' nonces.
  const request = { repository: repo, kind: kind === 'pull_request' ? 'pr' : 'issue',
    target: { number }, reaction: 'eyes', request_nonce: `github-events:${comparison.event_key}` };
  // The digest is already delivered and the history saved: a refused or
  // failed reaction is recorded in the run result and never undoes either.
  let code = null;
  try {
    const answer = await ops.call('plexus', 'github_react', request, { for: owner });
    if (answer?.error) code = String(answer.error.code ?? 'github_react_failed');
  } catch (error) {
    code = String(error?.data?.code ?? error?.data?.error?.code ?? 'github_react_failed');
  }
  result.reaction = { owner, request_nonce: request.request_nonce, code };
}
return result;
