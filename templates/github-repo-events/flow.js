// Keep the digest text and observation history in step with flows/github-events.
const MAX_AGE_MS = 6 * 60 * 60 * 1000;
const ROLLING_MS = 30 * 60 * 1000;
const CAP_MS = 60 * 60 * 1000;
const MAX_COMMENTS = 6;
const MAX_COMMENT_AUTHOR_BYTES = 128;
const MAX_COMMENT_EXCERPT_BYTES = 280;
// Leave room in KV for six links, their excerpts and observation history.
const MAX_COMMENT_URL_JSON_BYTES = 4096;

function bytes(text) {
  return text.replace(/[\u0080-\u07ff]|[\ud800-\udbff][\udc00-\udfff]|[\u0800-\uffff]/g,
    ch => ch.length === 2 ? 'xxxx' : ch.charCodeAt(0) <= 0x7ff ? 'xx' : 'xxx').length;
}

function boundedDigest(title, body, fields, observed, links) {
  const size = value => bytes(JSON.stringify(value));
  const data = { ...fields, observed: { ...observed } };
  const item = {
    title: Array.from(title).slice(0, 199).join(''),
    body,
    data,
    links: links.filter(link => link.kind === 'url' &&
      typeof link.url === 'string' && /^https:\/\/[A-Za-z0-9.-]+(?:[/?#]|$)/.test(link.url)),
  };
  // Track byte deltas instead of reserializing the whole item for each cut.
  let dataBytes = size(data);
  let itemBytes = size(item);
  const mark = (field, text) => {
    if (data.truncated?.[field] != null) return;
    let added = 0;
    if (!data.truncated) {
      data.truncated = {};
      added += ',"truncated":{}'.length;
    }
    const marker = { original_bytes: bytes(text) };
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
    const observedFields = [];
    for (const key of Object.keys(data.observed)) {
      const text = data.observed[key];
      if (typeof text === 'string' && text.length) observedFields.push({ key, text, bytes: size(text) });
    }
    while (dataBytes > 8192 || itemBytes > 16384) {
      let longest;
      for (const field of observedFields) {
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

// Compare activity with when Plexus first saw it, not when a delayed notice
// finally arrived. Older bodies without observed_at use the run time instead.
function staleActivity(activityText, observedText, now) {
  const activity = Date.parse(activityText);
  if (!Number.isFinite(activity)) return false;
  const observed = typeof observedText === 'string' ? Date.parse(observedText) : NaN;
  return (Number.isFinite(observed) ? observed : now) - activity > MAX_AGE_MS;
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

// Keep text from GitHub inside one quoted line.
function sanitize(text) {
  return String(text ?? '').replace(/</g, '\u2039').replace(/"/g, "'").replace(/[\r\n]/g, ' ');
}

function utcHHMM(ms) {
  return new Date(ms).toISOString().slice(11, 16) + 'Z';
}

function render(repo, kind, number, action, history, summary) {
  const labels = new Map([['issue', 'Issue'], ['pull_request', 'PR'], ['release', 'Release']]);
  const label = labels.get(kind) ?? sanitize(kind);
  const subject = `${repo} ${label} #${sanitize(number)}`;
  // Known actions have fixed words; unknown GitHub actions render as updates.
  const words = new Map([['opened', 'opened'], ['created', 'new comment'], ['edited', 'edit'],
    ['closed', 'closed'], ['reopened', 'reopened']]);
  const word = words.get(action) ?? 'update';
  let prefix;
  if (history.count === 1) {
    prefix = `- ${subject} ${word} observed at ${utcHHMM(history.times[0])}. `;
  } else {
    const times = history.times.map(utcHHMM);
    if (history.count > 6) times.push(`${history.count - 6} more`);
    const last = times.pop();
    prefix = `- ${subject} updated at ${times.join(', ')} and ${last}: `;
  }
  const comments = history.comments ?? [];
  let suffix = '';
  for (const { author, time, excerpt } of comments) {
    suffix += `\nNew comment by ${author} at ${utcHHMM(time)}`;
    suffix += excerpt ? `: "${excerpt}"` : '.';
  }
  if (history.comment_count > comments.length) {
    const overflow = history.comment_count - comments.length;
    suffix += `\n${overflow} earlier ${overflow === 1 ? 'comment' : 'comments'}.`;
  }
  return `${prefix}"${short(sanitize(summary), 4096 - bytes(prefix) - bytes(suffix) - 2)}"${suffix}`;
}

async function save(memory) {
  // Leave room below the KV value limit, including for escaped text.
  while (bytes(JSON.stringify(memory)) > 48000) {
    const oldest = Object.keys(memory.items).sort((a, b) =>
      memory.items[a].latest_ms - memory.items[b].latest_ms)[0];
    if (!oldest) break;
    delete memory.items[oldest];
  }
  await kv.set('github-events', memory);
}

const event = trigger?.event;
if (!event || event.module !== 'plexus' || event.version !== 2 ||
    !['github_issue_changed', 'github_pr_changed', 'github_release_published'].includes(event.name)) return;
const body = event.body;
// The notice header filters admission; only the verified body identifies the repo.
// Require an exact owner/name even though header parameters can contain wildcards.
if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(params.repo) || typeof body?.repo !== 'string') return;
const repo = body.repo.toLowerCase();
if (repo !== params.repo.toLowerCase()) return;

const release = event.name === 'github_release_published';
const kind = release ? 'release' : body.kind;
const number = release ? body.tag_name ?? body.tag : body.number;
if (typeof kind !== 'string' || !kind ||
    (release ? typeof number !== 'string' || !number : !Number.isSafeInteger(number))) return;
const key = `github:${repo}:${kind}:${number}`;
if (bytes(key) > 200 || /[\u0000-\u001f\u007f]/.test(key)) return;

const now = Date.now();
if (staleActivity(body.completed_at ?? body.updated_at, body.observed_at, now)) return;
const state = body.action === 'reopened' ? 'open' : body.state;
// Missing actor data is not evidence of no activity on a closed PR.
if (kind === 'pull_request' && state === 'closed' &&
    Array.isArray(body.actor_logins) && body.actor_logins.length === 0) return;
if (body.action === 'suppressed_own_echo') return;

const eventKey = body.event_key ?? event.event_key;
const memory = await kv.get('github-events') ?? { items: {} };
const prior = memory.items[key];
if (prior?.event_keys.includes(eventKey)) return;

const isNew = (kind === 'issue' || kind === 'pull_request') && body.action === 'opened';
const commentAuthor = typeof body.comment_author === 'string' && body.comment_author.length > 0 ?
  body.comment_author : null;
const humanComment = commentAuthor != null && !commentAuthor.toLowerCase().endsWith('[bot]');
// Deliver updates now. An event-only watch cannot schedule a later delivery,
// but the observation metadata still records the rolling update window.
const first = !prior || prior.is_new || prior.due_ms <= now || isNew ? now : prior.first_seen_ms;
const history = prior ?? { count: 0, times: [], event_keys: [] };
history.count = Math.min(Number.MAX_SAFE_INTEGER, history.count + 1);
if (history.times.length < 6) history.times.push(now);
if (commentAuthor != null) {
  const commentUrl = body.comment_url;
  // Plexus supplies the first line in comment_excerpt. A hidden marker can
  // be complete or cut short by the excerpt limit; quote neither form.
  const excerpt = body.comment_excerpt;
  const trimmedExcerpt = typeof excerpt === 'string' ? excerpt.trim() : '';
  const markerOnly = trimmedExcerpt.startsWith('<!--') &&
    (trimmedExcerpt.endsWith('-->') || !trimmedExcerpt.includes('-->'));
  const comment = {
    author: short(sanitize(commentAuthor), MAX_COMMENT_AUTHOR_BYTES),
    time: now,
    comment_url: typeof commentUrl === 'string' && bytes(JSON.stringify(commentUrl)) <= MAX_COMMENT_URL_JSON_BYTES ?
      commentUrl : null,
    excerpt: typeof excerpt === 'string' && excerpt.length > 0 && !markerOnly ?
      short(sanitize(excerpt), MAX_COMMENT_EXCERPT_BYTES) : null,
  };
  history.comments = [comment, ...(history.comments ?? [])].slice(0, MAX_COMMENTS);
  history.comment_count = Math.min(Number.MAX_SAFE_INTEGER, (history.comment_count ?? 0) + 1);
}
history.event_keys.push(eventKey);
history.event_keys = history.event_keys.slice(-64);
history.first_seen_ms = first;
history.latest_ms = now;
history.due_ms = isNew ? now : Math.min(now + ROLLING_MS, first + CAP_MS);
history.is_new = isNew;

const summary = String(body.title ?? '');
const fields = { repo, kind, number, event_key: eventKey, state: state ?? null,
  first_seen_ms: first, latest_ms: now, due_ms: history.due_ms, observation_count: history.count,
  observation_times: history.times, overflow: Math.max(0, history.count - 6) };
const links = [{ kind: 'url', url: body.url }];
for (const comment of history.comments ?? []) links.push({ kind: 'url', url: comment.comment_url });
const item = boundedDigest(sanitize(`${repo} ${kind} #${number}`), render(repo, kind, number, body.action, history, summary),
  fields, { summary }, links);
await sink.digest(self.agent_id, item, isNew || humanComment ? 'wake' : 'piggyback', { replace_key: key });
memory.items[key] = history;
await save(memory);
