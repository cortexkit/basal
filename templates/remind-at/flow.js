// BEGIN boundedDigest: this copy is kept in step with the other templates by
// the template_digest_helpers_are_identical test.
function boundedDigest(title, observed, links = []) {
  // Native regexp scanning skips ASCII without running JavaScript per character.
  const utf8Bytes = text => text.replace(/[\u0080-\u07ff]|[\ud800-\udbff][\udc00-\udfff]|[\u0800-\uffff]/g,
    ch => ch.length === 2 ? 'xxxx' : ch.charCodeAt(0) <= 0x7ff ? 'xx' : 'xxx').length;
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
  // Serialize data and the complete digest once. Track field-size deltas below;
  // mark() adds marker bytes, and removing a link also removes its array comma.
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
  // Never shorten the note. Drop oversized links before reducing metadata;
  // a shortened URL would no longer identify the observed item reliably.
  while (item.links.length && itemBytes > 16384) {
    const link = item.links.pop();
    itemBytes -= size(link) + (item.links.length ? 1 : 0);
    mark('link_url', link.url);
  }
  if (dataBytes > 8192 || itemBytes > 16384) {
    const fields = [];
    for (const group of ['params', 'observed']) {
      for (const key of Object.keys(data[group])) {
        const text = data[group][key];
        if (typeof text === 'string' && text.length) fields.push({ group, key, text, bytes: size(text) });
      }
    }
    while (dataBytes > 8192 || itemBytes > 16384) {
      let longest;
      for (const field of fields) {
        if (field.text.length && (!longest || field.bytes > longest.bytes)) longest = field;
      }
      if (!longest) throw new Error('Digest metadata cannot fit');
      const { group, key, text } = longest;
      mark(`${group}.${key}`, text);
      const chars = Array.from(text);
      const shortened = chars.slice(0, Math.floor(chars.length / 2)).join('');
      const next = size(shortened);
      data[group][key] = shortened;
      dataBytes += next - longest.bytes;
      itemBytes += next - longest.bytes;
      longest.text = shortened;
      longest.bytes = next;
    }
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

// The one-shot scheduler, not the script's wall clock, decides when to run.
await finishWatch('Reminder time reached', { due: trigger?.due ?? params.at });
