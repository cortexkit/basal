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

if (params.kind === 'issue' && params.state === 'merged') {
  await finishWatch('GitHub watch stopped: issues cannot merge', { reason: 'issue_cannot_merge' });
  return;
}
if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(params.repo)) {
  await finishWatch('GitHub watch stopped: misconfigured repository name', { reason: 'invalid_repo' });
  return;
}

const event = trigger?.event;
const names = { pr: 'github_pr_changed', issue: 'github_issue_changed', release: 'github_release_published' };
if (!event || event.module !== 'plexus' || event.version !== 1 || event.name !== names[params.kind]) return;
// Plexus v1 stores raw GitHub items. Repository identity is a notice header,
// not a body field; basal supplies both after verifying the fetched body.
if (typeof event.headers?.repo !== 'string' || event.headers.repo.toLowerCase() !== params.repo.toLowerCase()) return;
const body = event.body;
if (!body || typeof body !== 'object' || Array.isArray(body)) return;
if (params.kind !== 'release' && body.number !== params.number) return;
const merged = body.merged === true || body.merged_at != null;
const closed = body.state === 'closed';
if (params.kind !== 'release') {
  if (params.state === 'merged' && !merged) return;
  if (params.state === 'closed' && !closed) return;
}

const observed = { event: event.name };
for (const key of ['id', 'number', 'state', 'title', 'updated_at', 'tag_name', 'name', 'published_at']) {
  const value = body[key];
  if (typeof value === 'string' || (typeof value === 'number' && Number.isSafeInteger(value))) observed[key] = value;
}
if (params.kind === 'pr') observed.merged = merged;
const happened = params.kind === 'release' ? 'release published' :
  params.state === 'any_change' ? `${params.kind === 'pr' ? 'PR' : 'issue'} changed` :
  `${params.kind === 'pr' ? 'PR' : 'issue'} ${params.state}`;
await finishWatch(`GitHub ${happened}`, observed,
  typeof body.html_url === 'string' ? [{ kind: 'url', url: body.html_url }] : []);
