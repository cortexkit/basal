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

if (params.condition !== 'exists' && params.text === '') {
  await finishWatch('File watch stopped: misconfigured empty text', { reason: 'empty_text' });
  return;
}

let stat;
let read;
try {
  stat = await fs.stat(params.path);
  if (!stat.exists) return;
  if (params.condition !== 'exists') {
    if (stat.kind !== 'file') {
      await finishWatch('File watch stopped: target is not a text file', { reason: 'not_a_file', kind: stat.kind });
      return;
    }
    read = await fs.read(params.path);
  }
} catch (error) {
  const code = error.data?.code;
  if (code === 'denied' || code === 'invalid_arguments') {
    await finishWatch(`File watch stopped: ${code}`, { reason: code });
  }
  // Contents can become readable later. Absence between stat and read,
  // oversized or non-text contents, I/O failures and captured reads all retry.
  return;
}

const contains = read ? read.text.includes(params.text) : false;
if (params.condition === 'contains' && !contains) return;
if (params.condition === 'lacks' && contains) return;
await finishWatch(`File ${params.condition === 'exists' ? 'appeared' : params.condition === 'contains' ? 'contains the watched text' : 'lacks the watched text'}`, {
  exists: true,
  kind: stat.kind,
  size: stat.size,
  mtime_ms: stat.mtime_ms,
  ...(read ? { contains } : {}),
});
