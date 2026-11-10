// Wake the creating agent when review feedback lands on its repository's PRs.
// Inline comments include the PR, file, line and finding. A person's submitted
// review also wakes; bot summaries wake only when requesting changes, because
// their inline comments already carry the findings. Skip the repository's own
// bot using Plexus bindings; if those are unavailable, skip *-alfonso[bot] only.
const BINDINGS_TTL_MS = 10 * 60 * 1000;

async function wake(item) {
  try {
    await sink.digest(self.agent_id, item, 'wake');
  } catch (error) {
    // Capture-only dry runs record the wake without delivering it.
    if (error.data?.code !== 'captured') throw error;
  }
}

async function isOwnBot(login) {
  const now = Date.now();
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
    } catch {
      // Routing feedback must work even when the bindings service is unavailable.
      snapshot = { generation: 0, bindings: [] };
    }
    // checked_at records the last attempt, including failures, to bound retries.
    cached = { generation: snapshot.generation, checked_at: now, bindings: snapshot.bindings };
    await kv.set('github-bindings', cached);
  }
  const binding = cached.bindings.find(binding =>
    binding.repository?.toLowerCase() === params.repo.toLowerCase() &&
    typeof binding.app_handle_id === 'string' && binding.app_handle_id.length);
  const normalized = String(login || '').toLowerCase();
  return binding ? normalized === `${binding.app_handle_id}[bot]`.toLowerCase() : normalized.endsWith('-alfonso[bot]');
}

if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(params.repo)) return;
const triggerValue = typeof trigger === 'string' ? JSON.parse(trigger) : trigger;
const event = triggerValue && triggerValue.event;
if (!event || event.module !== 'plexus' || event.version !== 1 || !event.body) return;
const body = event.body;
if (body.repo !== params.repo) return;
const clean = (text) => String(text || '').replace(/<[^>]+>/g, '').replace(/\n{3,}/g, '\n\n').trim();
const clip = (text, max) => (text.length > max ? text.slice(0, max - 1) + '…' : text);
if (event.name === 'github_pr_review_comment_created' && body.kind === 'pull_request_review_comment') {
  if (await isOwnBot(body.author_login)) return;
  const where = body.path ? `${body.path}${body.line != null ? ':' + body.line : body.original_line != null ? ':' + body.original_line + ' (outdated)' : ''}` : 'general';
  const who = body.author_is_bot ? body.author_login : `${body.author_login} (person)`;
  const reply = body.in_reply_to_id != null ? ' (reply)' : '';
  const links = body.comment_url ? [{ kind: 'url', url: body.comment_url }] : [];
  await wake({ title: clip(`PR #${body.pr} review comment${reply}: ${where}`, 200), body: clip(`${who} on head ${String(body.head_sha || '').slice(0, 12)}:\n\n${clean(body.body_excerpt)}${body.body_truncated ? '\n\n(truncated)' : ''}`, 4000), data: { pr: body.pr, path: body.path, line: body.line, comment_id: body.comment_id, head_sha: body.head_sha }, links });
  return;
}
if (event.name === 'github_pr_review_submitted' && body.kind === 'pull_request_review') {
  if (await isOwnBot(body.reviewer_login)) return;
  const requestsChanges = body.state === 'changes_requested';
  if (body.reviewer_is_bot && !requestsChanges) return;
  const links = body.review_url ? [{ kind: 'url', url: body.review_url }] : [];
  await wake({ title: clip(`PR #${body.pr} review ${body.state} by ${body.reviewer_login}`, 200), body: clip(`Head ${String(body.head_sha || '').slice(0, 12)}:\n\n${clean(body.body_excerpt) || '(no review body)'}${body.body_truncated ? '\n\n(truncated)' : ''}`, 4000), data: { pr: body.pr, review_id: body.review_id, state: body.state, head_sha: body.head_sha }, links });
}
