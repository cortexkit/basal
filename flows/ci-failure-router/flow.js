// BEGIN OWNERS: lowercase owner/repo -> stable agent_id; comments name the agents.
// Changing this map changes delivery recipients, so publish it as a new flow version.
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

// Ignore workflows that generate routine failures without actionable owner work.
const IGNORED_WORKFLOWS = {
  'cortexkit/aft': ['Design gate'],
};
const IGNORED_JOBS = {
  'cortexkit/aft': ['design-gate'],
};

const QUIET_REPOS = {
  'cortexkit/cerebellum': 'CI is off by owner decision: GitHub billing refuses its jobs',
};
// Remove an entry when that repository's CI is back on.

// Plexus rescans can republish old completions; real CI notices arrive within minutes.
const MAX_COMPLETION_AGE_MS = 6 * 60 * 60 * 1000;

// Adapted from templates/github-ci-watch: bounded digest metadata, per-job
// deduplication and fourteen-day, byte-capped retention of run attempts.
function boundedDigest(title, observed, links = []) {
  // Native regexp scanning skips ASCII without running JavaScript per character.
  const utf8Bytes = text => text.replace(/[\u0080-\u07ff]|[\ud800-\udbff][\udc00-\udfff]|[\u0800-\uffff]/g,
    ch => ch.length === 2 ? 'xxxx' : ch.charCodeAt(0) <= 0x7ff ? 'xx' : 'xxx').length;
  // Sorting object keys changes no byte counts. These JSON values contain only
  // strings, booleans, finite integers, nulls, arrays and plain objects.
  const size = value => utf8Bytes(JSON.stringify(value));
  const data = { observed: { ...observed } };
  const item = {
    title: Array.from(title).slice(0, 199).join(''),
    body: '',
    data,
    links: links.filter(link => link.kind === 'url' &&
      typeof link.url === 'string' && /^https:\/\/[A-Za-z0-9.-]+(?:[/?#]|$)/.test(link.url)),
  };
  // Serialize data and the complete digest once. Track field-size deltas below;
  // markers add bytes, and link removal also drops a separator when one remains.
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
  // Drop oversized links before reducing metadata; a shortened URL would no
  // longer identify the observed item reliably.
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

// Notice headers can name a different repo or branch from the fetched body.
// Recheck the verified body independently; every branch, including unknown, is eligible.
function matches(pattern, value, repo = false) {
  if (value == null || value === '') return pattern === '*';
  const fold = text => text.replace(/[A-Z]/g, ch => ch.toLowerCase());
  if (repo) { pattern = fold(pattern); value = fold(value); }
  const escaped = pattern.split('*').map(part => part.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'));
  return new RegExp(`^${escaped.join('.*')}$`).test(value);
}

async function saveMemory(memory) {
  // Age pruning handles ordinary traffic; a byte cap also bounds bursts and
  // unknown timestamps, including growth of the unmapped-notice counter.
  while (bytes(JSON.stringify(memory)) > 48000) {
    const entries = [memory.runs, memory.mainFailures].flatMap(store =>
      Object.entries(store).map(([id, value]) => ({ store, id, completed: value.completed })));
    const oldest = entries.sort((a, b) => a.completed - b.completed)[0];
    delete oldest.store[oldest.id];
  }
  await kv.set('ci-router', memory);
}

const event = trigger.event;
if (!event || event.module !== 'plexus' || event.version !== 2) return;
const jobEvent = event.name === 'github_ci_job_failed';
if (!jobEvent && event.name !== 'github_ci_run_completed') return;
const body = event.body;
if (!body || body.kind !== (jobEvent ? 'ci_job' : 'ci_run') ||
    !matches('cortexkit/*', body.repo, true) || !matches('*', body.head_branch)) return;

const completed = Date.parse(body.completed_at);
const stale = Number.isFinite(completed) && Date.now() - completed > MAX_COMPLETION_AGE_MS;
const repo = body.repo.replace(/[A-Z]/g, ch => ch.toLowerCase());
const owner = OWNERS[repo];
// A single aggregate caps retained attempts across the entire fleet. Run keys
// include the repo so identical job/run ids in different repos cannot collide.
const memory = await kv.get('ci-router') ?? { newest: 0, runs: {}, unmapped: 0 };
memory.mainFailures ??= {};
if (stale) {
  await saveMemory(memory);
  return;
}
const workflowName = jobEvent ? body.workflow_name : body.name;
if ((IGNORED_WORKFLOWS[repo] ?? []).includes(workflowName)) {
  await saveMemory(memory);
  return;
}
if (!owner) {
  memory.unmapped = Math.min(Number.MAX_SAFE_INTEGER, (memory.unmapped ?? 0) + 1);
  await saveMemory(memory);
  return;
}
const runId = body.run_id ?? (jobEvent ? null : body.id);
const attempt = body.run_attempt;
// When run identity is incomplete, remember the event key to suppress repeats.
// Omit replace_key so unrelated unknown runs cannot overwrite one another.
const identified = runId != null && attempt != null;
const key = identified ? `ci:${repo}:${runId}:${attempt}` : `ci-event:${repo}:${event.event_key}`;
if (Number.isFinite(completed)) memory.newest = Math.max(memory.newest, completed);
const cutoff = memory.newest - 14 * 86400000;
for (const [id, run] of Object.entries(memory.runs)) {
  if (run.completed < cutoff) delete memory.runs[id];
}
for (const [id, failure] of Object.entries(memory.mainFailures)) {
  if (failure.completed < cutoff) delete memory.mainFailures[id];
}
// Ignore late deliveries outside the retained deduplication window.
if (Number.isFinite(completed) && completed < cutoff) {
  await saveMemory(memory);
  return;
}
const existing = memory.runs[key];
const green = !jobEvent && body.conclusion === 'success';
const defaultBranch = body.default_branch;
const mainBranch = typeof defaultBranch === 'string' && defaultBranch.length ?
  body.head_branch === defaultBranch : ['main', 'master'].includes(body.head_branch);
const knownWorkflow = typeof workflowName === 'string' && workflowName.length > 0;
if (green && mainBranch && knownWorkflow) {
  for (const [id, failure] of Object.entries(memory.mainFailures)) {
    if (failure.repo === repo && failure.workflow === workflowName) delete memory.mainFailures[id];
  }
}
if (!jobEvent && body.conclusion === 'cancelled' && Object.keys(existing?.jobs ?? {}).length === 0) {
  await saveMemory(memory);
  return;
}
if (green && !existing) {
  await saveMemory(memory);
  return;
}
if (!jobEvent && !green && !['failure', 'cancelled', 'startup_failure', null].includes(body.conclusion)) {
  await saveMemory(memory);
  return;
}
const run = existing ?? { jobs: {}, links: [], completed: Number.isFinite(completed) ? completed : memory.newest };
if (run.superseded) {
  await saveMemory(memory);
  return;
}
if (jobEvent && Object.hasOwn(run.jobs, String(body.id ?? event.event_key))) {
  await saveMemory(memory);
  return;
}
if (jobEvent && !['failure', 'timed_out', null].includes(body.conclusion)) return;

run.completed = Math.max(run.completed, Number.isFinite(completed) ? completed : memory.newest);
run.branch = body.head_branch ?? 'unknown';
run.sha = typeof body.head_sha === 'string' ? body.head_sha.slice(0, 12) : 'unknown';
run.conclusion = body.conclusion ?? 'unknown';
if (jobEvent && (IGNORED_JOBS[repo] ?? []).includes(body.name)) {
  // Run completions have no job list; retain ignored failures so they cannot
  // later masquerade as a red run whose failed jobs have not arrived yet.
  run.ignoredFailures = true;
  memory.runs[key] = run;
  await saveMemory(memory);
  return;
}
if (!jobEvent && run.ignoredFailures && Object.keys(run.jobs).length === 0) {
  await saveMemory(memory);
  return;
}
const mainFailureKey = JSON.stringify([repo, workflowName, body.name]);
const mergeQueueRepeat = jobEvent && body.head_branch?.startsWith('gh-readonly-queue/') &&
  knownWorkflow && Object.hasOwn(memory.mainFailures, mainFailureKey);
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
const hasFirstNotice = Array.isArray(run.firstWakeJobIds);
const firstWakeJobIds = new Set(hasFirstNotice ? run.firstWakeJobIds : []);
const jobsSinceFirstNotice = hasFirstNotice ?
  Object.keys(run.jobs).filter(id => !firstWakeJobIds.has(id)) : [];
const newNames = jobsSinceFirstNotice.map(id => run.jobs[id]);
const result = green ? 'now green' : !jobEvent && newNames.length ?
  `failed jobs since first notice: ${newNames.join(', ')}` : names.length ? `failed jobs ${names.join(', ')}` :
  // Plexus's run event carries no job list, so a red run with no job in hand
  // only means its job events haven't arrived; they update this item later.
  run.conclusion === 'unknown' ? 'run conclusion unknown; failed jobs not yet reported' :
    `run failed (${run.conclusion}); failed jobs not yet reported`;
const summary = `${repo} run ${runId ?? 'unknown'} (attempt ${attempt ?? 'unknown'}) on ${run.branch}: ${result}`;
const status = run.finished && !green ? `; ${run.finished}` : '';
const links = [...run.links];
if (run.url && !links.includes(run.url)) {
  if (links.length === 8) links.pop();
  links.push(run.url);
}
const item = boundedDigest(summary + status, {
  repo, run_id: runId, run_attempt: attempt, branch: run.branch, head_sha: body.head_sha,
  conclusion: run.conclusion, failed_jobs: names.join(', '),
  failed_jobs_since_first_notice: !jobEvent && hasFirstNotice ? newNames.join(', ') : '',
  status: run.finished ?? 'jobs arriving',
}, links.map(url => ({ kind: 'url', url })));
// data.observed.failed_jobs retains the job list beyond the short body summary;
// boundedDigest records any metadata truncation in data.truncated.
item.body = `${short(summary + status, 1400)}\nHead sha: ${run.sha}\nConclusion: ${run.conclusion}\ntest names unavailable`;
while (item.links.length && bytes(JSON.stringify(item)) > 16384) item.links.pop();
// A run completion can compare its accumulated job IDs with the first notice,
// even though Plexus v2 does not include per-job conclusions in that event.
const runHasReportedJob = Object.keys(run.jobs).length > 0;
const needsFirstWake = !hasFirstNotice;
const runFinishedNeedsWake = !jobEvent && runHasReportedJob &&
  hasFirstNotice &&
  jobsSinceFirstNotice.length > 0 && !run.completionWoke;
const quiet = Object.hasOwn(QUIET_REPOS, repo);
const action = quiet || green || mergeQueueRepeat ? 'silent' :
  jobEvent ? (needsFirstWake ? 'wake' : 'silent') :
    runHasReportedJob ? (runFinishedNeedsWake ? 'wake' : 'silent') :
      (needsFirstWake ? 'wake' : 'silent');
const refusals = [];
if (action === 'wake' && !run.supersessionChecked && !hasFirstNotice) {
  // Check only before the first wake, including when an earlier sink refused
  // delivery. An unreadable answer must never discard an actionable alert.
  run.supersessionChecked = true;
  try {
    if (runId == null) throw new Error('github_actions_supersession_unreadable');
    const answer = await ops.call('plexus', 'github_read', {
      action: 'github.actions.run.superseded', repository: repo, args: { run_id: runId },
    });
    if (answer?.error) throw { data: answer.error };
    if (typeof answer?.result?.superseded !== 'boolean') throw new Error('github_actions_supersession_unreadable');
    run.superseded = answer.result.superseded;
  } catch (error) {
    refusals.push({repo, owner, code: error.data?.code ?? error.data?.error?.code ?? String(error)});
  }
  if (run.superseded) {
    memory.runs[key] = run;
    await saveMemory(memory);
    return {repo, owner, refusals};
  }
}
if (jobEvent && mainBranch && knownWorkflow) {
  memory.mainFailures[mainFailureKey] = { repo, workflow: workflowName, completed: run.completed };
}
let digestSucceeded = false;
try {
  await sink.digest(owner, item, action, identified && bytes(key) <= 200 ? { replace_key: key } : undefined);
  digestSucceeded = true;
} catch (error) {
  // Audience recipients are checked by core at delivery time. A retired or
  // unknown owner must not fail the run; retain the refusal code for operators.
  if (error.data?.code === 'captured') {
    digestSucceeded = true;
  } else {
    refusals.push({repo, owner, code: error.data?.code ?? String(error)});
  }
}
if (digestSucceeded && action === 'wake') {
  if (needsFirstWake) run.firstWakeJobIds = Object.keys(run.jobs);
  if (!jobEvent) run.completionWoke = true;
}
memory.runs[key] = run;
await saveMemory(memory);
return {repo, owner, refusals};
