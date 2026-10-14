// BEGIN boundedDigest: this copy is kept in step with the other templates by
// the template_digest_helpers_are_identical test.
function boundedDigest(title, observed, links = []) {
  const utf8Bytes = text => {
    let bytes = 0;
    for (const ch of text) {
      const cp = ch.codePointAt(0);
      bytes += cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
    }
    return bytes;
  };
  // Sorting object keys changes no byte counts. These JSON values contain only
  // strings, booleans, finite integers, nulls, arrays and plain objects.
  const size = value => utf8Bytes(JSON.stringify(value));
  const values = { ...params };
  delete values.note;
  const data = { params: values, observed: { ...observed } };
  const item = {
    title: Array.from(title).slice(0, 199).join(''),
    body: params.note,
    data,
    links: links.filter(link => link.kind === 'url' &&
      typeof link.url === 'string' && /^https:\/\/[A-Za-z0-9.-]+(?:[/?#]|$)/.test(link.url)),
  };
  const mark = (field, text) => {
    data.truncated ??= {};
    data.truncated[field] ??= { original_bytes: utf8Bytes(text) };
  };
  // Never shorten the note. Drop oversized links before reducing metadata;
  // a shortened URL would no longer identify the observed item reliably.
  while (item.links.length && size(item) > 16384) {
    const link = item.links.pop();
    mark('link_url', link.url);
  }
  while (size(data) > 8192 || size(item) > 16384) {
    let longest;
    for (const group of ['params', 'observed']) {
      for (const key of Object.keys(data[group])) {
        const text = data[group][key];
        if (typeof text === 'string' && text.length &&
            (!longest || size(text) > size(longest.text))) {
          longest = { group, key, text };
        }
      }
    }
    if (!longest) throw new Error('Digest metadata cannot fit');
    const { group, key, text } = longest;
    mark(`${group}.${key}`, text);
    data[group][key] = Array.from(text).slice(0, Math.floor(Array.from(text).length / 2)).join('');
  }
  return item;
}

async function finishWatch(title, observed, links = []) {
  const item = boundedDigest(title, observed, links);
  for (const call of [() => sink.digest(self.agent_id, item, 'wake'), () => instance.end()]) {
    try {
      await call();
    } catch (error) {
      if (error.data?.code !== 'captured') throw error;
    }
  }
}
// END boundedDigest

function bytes(text) {
  let size = 0;
  for (const ch of text) {
    const cp = ch.codePointAt(0);
    size += cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
  }
  return size;
}

function short(text, maxBytes) {
  text = String(text ?? 'unknown');
  if (bytes(text) <= maxBytes) return text;
  let result = '';
  for (const ch of text) {
    if (bytes(result + ch) > maxBytes - 3) break;
    result += ch;
  }
  return result + '...';
}

// Repo/branch headers filter out unrelated notices before fetching their bodies,
// but can be mislabeled. Check the fetched repo/head_branch fields independently.
// Only '*' is special; fold ASCII case for repos as the header filter does.
function matches(pattern, value, repo = false) {
  if (value == null || value === '') return pattern === '*';
  const fold = text => text.replace(/[A-Z]/g, ch => ch.toLowerCase());
  if (repo) { pattern = fold(pattern); value = fold(value); }
  const escaped = pattern.split('*').map(part => part.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'));
  return new RegExp(`^${escaped.join('.*')}$`).test(value);
}

const event = trigger.event;
if (!event || event.module !== 'plexus' || event.version !== 2) return;
const jobEvent = event.name === 'github_ci_job_failed';
if (!jobEvent && event.name !== 'github_ci_run_completed') return;
const body = event.body;
if (!body || body.kind !== (jobEvent ? 'ci_job' : 'ci_run') ||
    !matches(params.repo, body.repo, true) || !matches(params.branch, body.head_branch)) return;

const repo = body.repo ?? 'unknown';
const runId = body.run_id ?? (jobEvent ? null : body.id);
const attempt = body.run_attempt;
// When run identity is incomplete, remember the event key in kv to suppress
// repeated events. Omit replace_key so unrelated unknown runs cannot overwrite
// one another's digest items.
const identified = runId != null && attempt != null && body.repo != null;
const key = identified ? `ci:${repo}:${runId}:${attempt}` : `ci-event:${event.event_key}`;
const memory = await kv.get('ci-watch') ?? { newest: 0, runs: {} };
const completed = Date.parse(body.completed_at);
if (Number.isFinite(completed)) memory.newest = Math.max(memory.newest, completed);
const cutoff = memory.newest - 14 * 86400000;
for (const [id, run] of Object.entries(memory.runs)) {
  if (run.completed < cutoff) delete memory.runs[id];
}
// Ignore late deliveries outside the retained deduplication window.
if (Number.isFinite(completed) && completed < cutoff) {
  await kv.set('ci-watch', memory);
  return;
}
const existing = memory.runs[key];
const green = !jobEvent && body.conclusion === 'success';
if (green && !existing) {
  await kv.set('ci-watch', memory);
  return;
}
if (!jobEvent && !green && !['failure', 'cancelled', 'startup_failure', null].includes(body.conclusion)) {
  await kv.set('ci-watch', memory);
  return;
}
const run = existing ?? { jobs: {}, links: [], completed: Number.isFinite(completed) ? completed : memory.newest };
if (jobEvent && Object.hasOwn(run.jobs, String(body.id ?? event.event_key))) {
  await kv.set('ci-watch', memory);
  return;
}
if (jobEvent && !['failure', 'timed_out', null].includes(body.conclusion)) return;

run.completed = Math.max(run.completed, Number.isFinite(completed) ? completed : memory.newest);
run.branch = body.head_branch ?? 'unknown';
run.sha = typeof body.head_sha === 'string' ? body.head_sha.slice(0, 12) : 'unknown';
run.conclusion = body.conclusion ?? 'unknown';
const validUrl = url => typeof url === 'string' && bytes(url) <= 1024 && /^https:\/\/[A-Za-z0-9.-]+(?:[/?#]|$)/.test(url);
if (jobEvent) {
  delete run.finished;
  run.jobs[String(body.id ?? event.event_key)] = short(body.name, 256);
  if (validUrl(body.url) && run.links.length < 8 && !run.links.includes(body.url)) run.links.push(body.url);
} else {
  if (validUrl(body.url)) run.url = body.url;
  run.finished = green ? 'now green' : run.conclusion === 'unknown' ? 'run finished (unknown)' :
    `run finished red (${run.conclusion})`;
}
const names = Object.values(run.jobs);
const result = green ? 'now green' : names.length ? `failed jobs ${names.join(', ')}` :
  run.conclusion === 'unknown' ? 'run conclusion unknown; no failed job reported' :
    `run failed with no failed job reported (${run.conclusion})`;
const summary = `${repo} run ${runId ?? 'unknown'} (attempt ${attempt ?? 'unknown'}) on ${run.branch}: ${result}`;
const status = run.finished && !green ? `; ${run.finished}` : '';
const links = [...run.links];
if (run.url && !links.includes(run.url)) {
  if (links.length === 8) links.pop();
  links.push(run.url);
}
const item = boundedDigest(summary + status, {
  repo, run_id: runId, run_attempt: attempt, branch: run.branch, head_sha: body.head_sha,
  conclusion: run.conclusion, failed_jobs: names.join(', '), status: run.finished ?? 'jobs arriving',
}, links.map(url => ({ kind: 'url', url })));
// Reserve body space for the complete note and fixed CI metadata. The longer
// job list remains in observed metadata, where boundedDigest marks truncation.
item.body = `${params.note}\n\n${short(summary + status, 1400)}\nHead sha: ${run.sha}\nConclusion: ${run.conclusion}\ntest names unavailable`;
while (item.links.length && bytes(JSON.stringify(item)) > 16384) item.links.pop();
const action = green ? 'silent' : body.head_branch === params.main_branch ? 'wake' : 'piggyback';
try {
  try {
    await sink.digest(self.agent_id, item, action, identified && bytes(key) <= 200 ? { replace_key: key } : undefined);
  } catch (error) {
    if (error.data?.code !== 'digest_replace_key_unsupported') throw error;
    await sink.digest(self.agent_id, item, action);
  }
} catch (error) {
  if (error.data?.code !== 'captured') throw error;
}
memory.runs[key] = run;
// Age pruning handles ordinary traffic; a byte cap also bounds bursts and
// events with unknown timestamps. Evict the oldest attempts first.
while (bytes(JSON.stringify(memory)) > 48000) {
  const oldest = Object.keys(memory.runs).sort((a, b) => memory.runs[a].completed - memory.runs[b].completed)[0];
  delete memory.runs[oldest];
}
await kv.set('ci-watch', memory);
