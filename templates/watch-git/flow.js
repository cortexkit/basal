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

const ref = params.ref || 'HEAD';
let head;
try {
  head = await git.revParse(params.repo, ref);
} catch (error) {
  const code = error.data?.code;
  if (code === 'denied' || code === 'invalid_arguments') {
    await finishWatch(`Git watch stopped: ${code}`, { reason: code });
  }
  // A ref may appear later; unavailable Git and captured reads retry too.
  return;
}
if (params.condition === 'ref_exists') {
  await finishWatch('Git ref exists', { ref, sha: head });
  return;
}

const baseline = await kv.get('baseline');
if (baseline === null) {
  // Pin the first successful read, rather than waking for existing history.
  await kv.set('baseline', head);
  return;
}
if (baseline === head) return;

let commits;
try {
  await git.revParse(params.repo, baseline);
  const lost = await git.log(params.repo, { ref: `${head}..${baseline}`, maxCount: 1 });
  if (lost.length) {
    await finishWatch('Git watch stopped: watched history was rewritten', { reason: 'history_rewritten', baseline, head });
    return;
  }
  commits = await git.log(params.repo, { ref: `${baseline}..${head}`, maxCount: 500 });
} catch (error) {
  const code = error.data?.code;
  if (code === 'denied' || code === 'invalid_arguments' || code === 'not_found') {
    await finishWatch(`Git watch stopped: ${code}`, { reason: code, baseline, head });
  }
  return;
}
const match = commits.find(commit => commit.subject.includes(params.pattern));
if (match) {
  await finishWatch('Matching Git commit landed', { sha: match.sha, subject: match.subject, author_ms: match.author_ms, baseline, head });
} else if (commits.length === 500) {
  await finishWatch('Git watch stopped: more than 500 new commits between checks', { reason: 'history_limit', baseline, head, scan_limit: 500 });
} else {
  // Advancing only after a complete scan keeps each subsequent range small.
  await kv.set('baseline', head);
}
