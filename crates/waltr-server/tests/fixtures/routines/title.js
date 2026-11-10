export default async function () {
  const snapshot = (await host.call('context.snapshot', {})).result;
  const key = 'title';
  if (Object.prototype.hasOwnProperty.call(snapshot.metadata, key)) return false;
  const first = Array.from(snapshot.items).find(item => item.channel === 'user_text');
  if (!first) return false;
  const helper = (await host.call('helper.run', {
    selection: {kind: 'named', name: 'routine-title'},
    task: JSON.stringify(first),
    maxTurns: 1,
    deadlineMs: 10000, responseBytes: 4096, completionBytes: 8192
  })).result;
  const title = helper.response.trim();
  if (!title || title.length > 60) throw new Error('invalid title');
  const current = (await host.call('context.snapshot', {})).result;
  if (Object.prototype.hasOwnProperty.call(current.metadata, key)) return false;
  return (await host.call('context.transact', {
    source: current.source, expectedRevision: current.revision,
    predicates: [{key, value: {state: 'absent'}}],
    updates: [{key, value: {state: 'present', value: title}}]
  })).result.committed;
}
