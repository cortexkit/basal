// BEGIN BINDINGS: kept identical in both flows because scripts cannot import helpers.
// Only repositories without a bot binding use these CI-owner fallbacks.
const UNBOUND_OWNERS = {
  'cortexkit/subconscious-design': 'agent_2bfd6927602f7977', // SUBC
  'cortexkit/cingulate': 'agent_2bfd6927602f7977', // SUBC
  'cortexkit/broca-private': 'agent_65e9fe998e14b47d', // BROCA
  'cortexkit/motor-private': 'agent_ef98bc82ef005a63', // MOTOR
};
const BINDINGS_TTL_MS = 10 * 60 * 1000;

async function routing(now) {
  let cached = await kv.get('github-bindings');
  if (!cached || now - cached.checked_at >= BINDINGS_TTL_MS) {
    let snapshot;
    try {
      const answer = await ops.call('plexus', 'github_read', { action: 'github.bindings.list' });
      if (answer?.error) throw { data: answer.error };
      snapshot = answer?.result;
      if (!Number.isSafeInteger(snapshot?.generation) || !Array.isArray(snapshot?.bindings)) {
        throw new Error('github_bindings_unreadable');
      }
    } catch (error) {
      if (!cached) throw new Error(`repository owners unknown: Plexus bindings could not be read (${error.data?.code ?? String(error)})`);
      snapshot = cached;
    }
    // checked_at records the last attempt, not the last successful read.
    cached = { generation: snapshot.generation, checked_at: now, bindings: snapshot.bindings };
    await kv.set('github-bindings', cached);
  }
  const owners = { ...UNBOUND_OWNERS };
  const bots = {};
  const ownerBots = {};
  for (const binding of cached.bindings) {
    owners[binding.repository] = binding.agent_id;
    bots[binding.repository] = `${binding.app_handle_id}[bot]`.toLowerCase();
    ownerBots[binding.agent_id] = bots[binding.repository];
  }
  for (const [repo, owner] of Object.entries(UNBOUND_OWNERS)) {
    bots[repo] ??= ownerBots[owner] ?? null;
  }
  return { owners, bots };
}
// END BINDINGS

// Ignore workflows and jobs that generate routine failures without actionable
// owner work. The '*' repository applies fleet-wide. An entry is a name or
// {name, case_insensitive?, unless_branch?}: ignored except on branches matching
// an unless_branch pattern, where a failure still wakes the owner.
// magic-context's design gate blocks contributor PRs until their issue is
// labelled design-approved, which is expected, not a CI failure.
const MC_DESIGN_GATE_BRANCHES = ['master', 'train/*'];
const IGNORED_WORKFLOWS = {
  // Review findings stays red while threads are open; its own flow reports them.
  '*': [{ name: 'Review findings', case_insensitive: true }, 'review-findings'],
  'cortexkit/aft': ['Design gate'],
  'cortexkit/magic-context': [{ name: 'design-gate', unless_branch: MC_DESIGN_GATE_BRANCHES }],
};
const IGNORED_JOBS = {
  '*': ['review-findings'],
  'cortexkit/aft': ['design-gate'],
  'cortexkit/magic-context': [
    { name: 'design-gate', unless_branch: MC_DESIGN_GATE_BRANCHES },
    { name: 'design-gate / design-gate', unless_branch: MC_DESIGN_GATE_BRANCHES },
  ],
};

function ignoredName(entries, name, branch) {
  return (entries ?? []).some(entry => {
    if (typeof entry === 'string') return entry === name;
    const same = entry.case_insensitive ? typeof name === 'string' &&
      entry.name.toLowerCase() === name.toLowerCase() : entry.name === name;
    return same && !(entry.unless_branch ?? []).some(pattern => matches(pattern, branch));
  });
}

// Branches whose failures always wake the owner, besides the default branch.
const PROTECTED_BRANCHES = ['gh-readonly-queue/*', 'train/*', 'release/*'];

// Seats push with the operator's git credential, so a seat's own branch shows
// the operator as actor; a seat's GitHub App pushes as `<seat>-alfonso[bot]`.
// `actor` is who pushed, not who re-ran: a re-run of a contributor's run by the
// operator keeps the contributor as actor.
const OPERATOR_LOGIN = 'ualtinok';
function seatActor(login) {
  if (typeof login !== 'string' || login.length === 0) return false;
  const lower = login.toLowerCase();
  return lower === OPERATOR_LOGIN || /^[a-z0-9-]+-alfonso\[bot\]$/.test(lower);
}

// Repositories whose owner wakes only for these branches; a failure on any other
// branch, whoever pushed it, records a silent entry. This replaces the general
// protected-branch and seat-push rule for that repository.
const WAKE_BRANCHES_BY_REPO = {
  'cortexkit/magic-context': ['master', 'train/*', 'bench/*'],
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

// Plexus rescans can republish old news long after it happened. Plexus stamps
// observed_at when it first stores an item, so an item is stale when it
// happened more than maxAgeMs before Plexus first saw it. A later delay before
// the event is published (an event bus outage) never makes it stale; the
// manifest's max_age separately limits how long a published event may wait.
// Bodies stored before Plexus added observed_at fall back to the item's age
// at run time. An unknown or unparseable activity time is never stale.
function staleActivity(activityText, observedText, maxAgeMs, now) {
  const activity = Date.parse(activityText);
  if (!Number.isFinite(activity)) return false;
  const observed = typeof observedText === 'string' ? Date.parse(observedText) : NaN;
  if (Number.isFinite(observed)) return observed - activity > maxAgeMs;
  return now - activity > maxAgeMs;
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

function failingTests(log) {
  const names = [];
  const seen = new Set();
  let failures = false;
  // Inspect only the tail requested from Plexus. The extra count describes
  // names found here, not failures that may have scrolled out of the log tail.
  for (let line of log.slice(-65536).split('\n')) {
    line = line.replace(/^\d{4}-\d\d-\d\dT[\d:.]+Z\s?/, '').replace(/\r$/, '');
    if (/^\s*failures:\s*$/.test(line)) {
      failures = true;
      continue;
    }
    let name;
    // nextest prints `FAIL [ 1.2s] ( 623/2506) binary-id test::path`. Keep only
    // the test path, so it reads cleanly and deduplicates against libtest's
    // `test test::path ... FAILED` lines for the same test.
    const nextest = line.match(
      /^\s*(?:FAIL|TIMEOUT)\s+\[[^\]\r\n]{1,40}\]\s+(?:\(\s*\d+\/\d+\)\s+)?(?:[A-Za-z0-9_-]+(?:::[A-Za-z0-9_-]+)?\s+)?(\S.*)$/,
    );
    const match = line.match(/^\s*test (.+?) \.\.\. FAILED(?:\s|$)/) ??
      nextest ??
      line.match(/^\s*\(fail\)\s+(.+?)(?:\s+\[[\d.]+(?:ms|s)\])?\s*$/) ??
      line.match(/^\s*--- FAIL:\s+(\S+)/) ??
      line.match(/^\s*FAILED\s+(.+?::.+?)(?:\s+-\s+.*)?$/) ??
      line.match(/^\s*FAIL\s+(?!\S+\s+[\d.]+s(?:\s|$))(.+)$/) ??
      line.match(/^\s*[×✕]\s+(.+?)(?:\s+\(?[\d.]+\s*m?s\)?)?\s*$/);
    if (match) {
      name = match[1];
      failures = false;
    } else if (failures && /^\s{2,}\S/.test(line) && !/^\s*----/.test(line)) {
      name = line.trim();
    } else if (line.trim()) {
      failures = false;
    }
    if (name == null) continue;
    name = name.replace(/[\u0000-\u001f\u007f-\u009f\u2028\u2029]/g, '').trim();
    if (!name || seen.has(name)) continue;
    seen.add(name);
    if (names.length < 5) names.push(short(name, 160));
  }
  return { names, more: seen.size - names.length };
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

function silentBranchDigest(memory, repo, branch) {
  // Reuse the bounded run-attempt history: jobs and completions update one
  // attempt, not the count. Age pruning and byte eviction also bound this view.
  const runs = Object.values(memory.runs)
    .filter(run => run.silentSummary?.repo === repo && run.branch === branch)
    .sort((a, b) => b.completed - a.completed || b.silentSummary.order - a.silentSummary.order);
  const recent = runs.slice(0, 4).map(run => run.silentSummary);
  const summaries = recent.map(run => run.body).join('\n\n');
  const item = boundedDigest(recent[0].title, {
    repo, branch, run_count: runs.length, recent_runs: summaries,
  }, recent.flatMap(run => run.links).slice(0, 4));
  item.body = `${runs.length} recent run attempt${runs.length === 1 ? '' : 's'} on ${short(branch, 400)}\n` +
    `Newest ${recent.length} shown; history limited to 14 days and the retained byte budget.\n\n${summaries}`;
  // JSON escaping can exceed the item cap even when the UTF-8 body fits.
  while (item.links.length && bytes(JSON.stringify(item)) > 16384) item.links.pop();
  while (bytes(JSON.stringify(item)) > 16384) item.body = short(item.body, Math.floor(bytes(item.body) / 2));
  return item;
}

const event = trigger.event;
if (!event || event.module !== 'plexus' || event.version !== 2) return;
const jobEvent = event.name === 'github_ci_job_failed';
if (!jobEvent && event.name !== 'github_ci_run_completed') return;
const body = event.body;
if (!body || body.kind !== (jobEvent ? 'ci_job' : 'ci_run') ||
    !matches('cortexkit/*', body.repo, true) || !matches('*', body.head_branch)) return;

// Reuse one clock sample so cache age and event age share an instant.
const now = Date.now();
const completed = Date.parse(body.completed_at);
const stale = staleActivity(body.completed_at, body.observed_at, MAX_COMPLETION_AGE_MS, now);
const repo = body.repo.replace(/[A-Z]/g, ch => ch.toLowerCase());
const { owners } = await routing(now);
const owner = owners[repo];
// A single aggregate caps retained attempts across the entire fleet. Run keys
// include the repo so identical job/run ids in different repos cannot collide.
const memory = await kv.get('ci-router') ?? { newest: 0, runs: {}, unmapped: 0 };
memory.mainFailures ??= {};
if (stale) {
  await saveMemory(memory);
  return;
}
const workflowName = jobEvent ? body.workflow_name : body.name;
if (ignoredName([...IGNORED_WORKFLOWS['*'], ...(IGNORED_WORKFLOWS[repo] ?? [])], workflowName, body.head_branch)) {
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
// Per-run notices omit replace_key when identity is unknown. Silent branch
// digests can still group them by their repository and branch.
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
if (jobEvent && ignoredName([...IGNORED_JOBS['*'], ...(IGNORED_JOBS[repo] ?? [])], body.name, body.head_branch)) {
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
// A failed job no runner ever picked up never started: GitHub refused it
// before it ran, as it does for a private repo whose billing is off. Plexus
// sends `runner_name: null` for exactly these. Its failure tells the owner
// nothing new, so it is recorded silently. An older body without the field,
// or a job that failed during runner setup (a runner, no steps), still wakes.
const notStarted = jobEvent && body.conclusion === 'failure' &&
  Object.hasOwn(body, 'runner_name') && body.runner_name === null;
if (jobEvent) {
  delete run.finished;
  const id = String(body.id ?? event.event_key);
  run.jobs[id] = notStarted ? `${short(body.name, 240)} (not started)` : short(body.name, 256);
  if (notStarted) run.notStarted = [...(run.notStarted ?? []), id];
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
const workflow = short(knownWorkflow ? workflowName : 'CI', Math.max(4, 60 - Array.from(repo).length));
const titlePrefix = green ? `${repo}: ${workflow} green again on ` : `${repo}: ${workflow} failed on `;
const branchLimit = Math.max(1, 80 - Array.from(titlePrefix).length);
const branch = Array.from(run.branch ?? 'unknown');
const titleBranch = branch.length > branchLimit ?
  `${branch.slice(0, Math.max(0, branchLimit - 1)).join('')}…` : branch.join('');
const title = `${titlePrefix}${titleBranch}`;
const links = run.url ? [run.url, ...run.links.filter(url => url !== run.url).slice(0, 3)] : run.links.slice(0, 3);
const item = boundedDigest(title, {
  repo, workflow: workflowName, run_id: runId, run_attempt: attempt, branch: run.branch, head_sha: body.head_sha,
  conclusion: run.conclusion, failed_jobs: names.join(', '),
  failed_jobs_since_first_notice: !jobEvent && hasFirstNotice ? newNames.join(', ') : '',
  status: run.finished ?? 'jobs arriving',
}, links.map(url => ({ kind: 'url', url })));
// data.observed.failed_jobs retains the job list beyond the short body summary;
// boundedDigest records any metadata truncation in data.truncated.
const bodyJobIds = !green && !jobEvent && newNames.length ? jobsSinceFirstNotice : Object.keys(run.jobs);
const bodyJobs = bodyJobIds.map(id => run.jobs[id]);
// A green run's jobs are the ones that were failing before; call them that,
// so the body never reads as a fresh failure.
const failedJobsLine = green ?
  (names.length ? `Previously failed: ${names.join(', ')}` : 'No failed jobs') :
  bodyJobs.length ?
    `${!jobEvent && newNames.length ? 'Failed since first notice: ' : 'Failed jobs: '}${bodyJobs.join(', ')}` :
    'Failed jobs not reported yet';
const bodyStatus = run.finished ?
  (run.finished === 'now green' ? 'finished green (success)' : run.finished.replace(/^run /, '')) :
  'jobs arriving';
const bodyLines = [
  `Workflow ${knownWorkflow ? short(workflowName, 240) : 'unknown'}`,
  short(failedJobsLine, 1400),
  short(`Run ${runId ?? 'unknown'}, attempt ${attempt ?? 'unknown'}, ${bodyStatus}`, 240),
  `Commit ${run.sha}`,
];
// A run completion can compare its accumulated job IDs with the first notice,
// even though Plexus v2 does not include per-job conclusions in that event.
const runHasReportedJob = Object.keys(run.jobs).length > 0;
const needsFirstWake = !hasFirstNotice;
const startedSinceFirstNotice = jobsSinceFirstNotice.filter(id => !(run.notStarted ?? []).includes(id));
const runFinishedNeedsWake = !jobEvent && runHasReportedJob &&
  hasFirstNotice &&
  startedSinceFirstNotice.length > 0 && !run.completionWoke;
const quiet = Object.hasOwn(QUIET_REPOS, repo);
// Off the default, merge-queue and release-train branches, a failure wakes its
// owner only on a branch a fleet seat started. A contributor's branch (say, a
// draft held by a design gate) is the contributor's to fix, so it records a
// silent entry. Unknown fork or actor counts as not a seat's run.
const protectedBranch = mainBranch || PROTECTED_BRANCHES.some(pattern => matches(pattern, body.head_branch));
const seatRun = body.head_repo_is_fork === false && seatActor(body.actor);
const wakeBranches = WAKE_BRANCHES_BY_REPO[repo];
const contributorBranch = wakeBranches ?
  !wakeBranches.some(pattern => matches(pattern, body.head_branch)) :
  !protectedBranch && !seatRun;
// A red run with no failed job in hand records its item silently: either its
// job events are still arriving (the first one wakes), or no job ever ran,
// as when GitHub closes runs that waited for a maintainer's approval. Only
// startup_failure wakes on the run alone, because no job can run there.
const runAloneWakes = run.conclusion === 'startup_failure';
const action = quiet || green || mergeQueueRepeat || contributorBranch || notStarted ? 'silent' :
  jobEvent ? (needsFirstWake ? 'wake' : 'silent') :
    runHasReportedJob ? (runFinishedNeedsWake ? 'wake' : 'silent') :
      (needsFirstWake && runAloneWakes ? 'wake' : 'silent');
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
// Later jobs still enrich the same digest after the first wake. Policy-silenced
// jobs do not need a log, and superseded attempts have already returned above.
// A job's log is read once: a repeated notice for the same job reuses what the
// first read found, or the fact that it found nothing.
run.failingTests ??= {};
const jobKey = String(body.id);
if (jobEvent && !quiet && !mergeQueueRepeat && !contributorBranch && !notStarted && body.id != null &&
  !Object.hasOwn(run.failingTests, jobKey)) {
  run.failingTests[jobKey] = { names: [], more: 0 };
  try {
    const answer = await ops.call('plexus', 'github_read', {
      action: 'github.actions.job_log', repository: repo, args: { job_id: body.id, max_bytes: 65536 },
    });
    if (answer?.error) throw { data: answer.error };
    if (typeof answer?.result?.text !== 'string') throw new Error('github_actions_job_log_unreadable');
    run.failingTests[jobKey] = failingTests(answer.result.text);
  } catch (error) {
    refusals.push({repo, owner, code: error.data?.code ?? error.data?.error?.code ?? String(error)});
  }
}
for (const id of bodyJobIds) {
  const tests = run.failingTests?.[id];
  if (!tests?.names.length) continue;
  const label = green ? 'Previously failing tests' : 'Failing tests';
  const line = `${label}: ${tests.names.join(', ')}${tests.more ? ` (+${tests.more} more)` : ''}`;
  const entry = bodyJobIds.length > 1 ? `${run.jobs[id]}\n${line}` : line;
  // Keep whole per-job summaries rather than cutting a test name mid-line.
  if (bytes([...bodyLines, entry].join('\n')) > 4096) break;
  bodyLines.push(entry);
}
item.body = bodyLines.join('\n');
while (item.links.length && bytes(JSON.stringify(item)) > 16384) item.links.pop();
let digestItem = item;
let replaceKey = identified && bytes(key) <= 200 ? key : undefined;
const branchKey = `ci-branch:${repo}:${run.branch}`;
if (action === 'silent' && !hasFirstNotice && bytes(branchKey) <= 200) {
  // Already-woken runs keep their per-run key, including later silent updates.
  // Only silent-only attempts share a branch item; never shorten an oversized
  // branch key into a colliding identity, falling back to the per-run key.
  // The sequence breaks equal completion timestamps by latest observation.
  memory.silentSequence = Math.min(Number.MAX_SAFE_INTEGER, (memory.silentSequence ?? 0) + 1);
  run.silentSummary = {
    repo, order: memory.silentSequence, title: item.title, body: short(item.body, 850), links: item.links.slice(0, 1),
  };
  memory.runs[key] = run;
  digestItem = silentBranchDigest(memory, repo, run.branch);
  replaceKey = branchKey;
} else {
  delete run.silentSummary;
}
let digestSucceeded = false;
try {
  await sink.digest(owner, digestItem, action, replaceKey ? { replace_key: replaceKey } : undefined);
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
