// Wake the creating agent with one Magic Context proposal for its project's
// ARCHITECTURE.md and STRUCTURE.md. The event body is immutable, so whether the
// proposal is still pending comes from a separate state query; a proposal that
// went stale, was applied or was dismissed before delivery is skipped quietly
// rather than failing the run.
const BODY_MAX = 4000;
const LIST_MAX = 12;

const clip = (text, max) => (text.length > max ? text.slice(0, max - 1) + '…' : text);
const line = (text) => String(text ?? '').replace(/\s+/g, ' ').trim();

const triggerValue = typeof trigger === 'string' ? JSON.parse(trigger) : trigger;
const event = triggerValue && triggerValue.event;
if (!event || event.module !== 'magic-context' || event.name !== 'docs_proposal' || event.version !== 1) return;
const proposal = event.body;
// Magic Context numbers proposals per project; pass the id back unchanged.
const validId = Number.isSafeInteger(proposal?.id) || (typeof proposal?.id === 'string' && proposal.id !== '');
if (!proposal || proposal.project_id !== params.project_id || !validId) return;

let state;
try {
  const answer = await ops.call('magic-context', 'docs_proposals.state', { proposal_id: proposal.id });
  if (answer?.error) throw { data: answer.error };
  state = answer?.result?.state;
} catch (error) {
  // A capture-only dry run answers no query; treat the proposal as pending
  // there so the preview shows the digest a real run would send.
  if (error.data?.code !== 'captured') throw error;
  state = 'pending';
}
if (state !== 'pending') return;

const documents = (Array.isArray(proposal.documents) ? proposal.documents : [])
  .filter((doc) => Array.isArray(doc.changes) && doc.changes.length);
const changeCount = documents.reduce((n, doc) => n + doc.changes.length, 0);
const dropped = Array.isArray(proposal.dropped) ? proposal.dropped.length : 0;
const names = documents.map((doc) => line(doc.name)).filter(Boolean);

const parts = [];
for (const doc of documents) {
  const changes = doc.changes;
  parts.push(`${line(doc.name)}: ${changes.length} change${changes.length === 1 ? '' : 's'}`);
  for (const change of changes.slice(0, LIST_MAX)) {
    const reason = line(change.reason);
    parts.push(`- ${line(change.action)} ${line(change.heading)}${reason ? ` — ${reason}` : ''}`);
  }
  if (changes.length > LIST_MAX) parts.push(`- and ${changes.length - LIST_MAX} more`);
}
if (dropped) parts.push(`${dropped} candidate change${dropped === 1 ? ' was' : 's were'} left out of this proposal.`);
// The file is relative to the project's checkout and holds every diff plus
// the full new section texts, which do not fit in a digest.
const file = typeof proposal.file === 'string' && proposal.file ? line(proposal.file) : null;
const action = 'apply the changes you agree with by editing ARCHITECTURE.md and STRUCTURE.md. Magic Context marks the proposal applied when the files match; the person can dismiss it in Alfonso Desktop.';

const footer = `\n${file ? `Read ${file}, then ${action}` : `Proposal ${proposal.id}: ${action}`}`;

const item = {
  title: clip(`Docs proposal for ${params.project_id}: ${changeCount} change${changeCount === 1 ? '' : 's'}${names.length ? ` in ${names.join(', ')}` : ''}`, 200),
  // Clip the change list, never the instruction line.
  body: clip(parts.join('\n'), BODY_MAX - footer.length) + footer,
  data: { proposal_id: proposal.id, project_id: proposal.project_id, file },
};
try {
  await sink.digest(self.agent_id, item, 'wake', { replace_key: `docs-proposal:${params.project_id}` });
} catch (error) {
  // Capture-only dry runs record the wake without delivering it.
  if (error.data?.code !== 'captured') throw error;
}
