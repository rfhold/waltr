export default async function (input) {
  const snapshot = (await host.call('context.snapshot', {})).result;
  if (snapshot.automatic) {
    const tokens = snapshot.contextTokens;
    const limit = snapshot.contextLimit;
    if (tokens === null || !(tokens >= 180000 || (limit !== null && limit > 0 && tokens >= limit * 0.8))) return false;
  }
  const helper = (await host.call('helper.run', {
    selection: {kind: 'named', name: 'routine-compaction'}, source: snapshot.source,
    task: 'Compact the pinned effective context. Preserve decisions and unfinished work.',
    maxTurns: 3,
    deadlineMs: 20000, responseBytes: 32768, completionBytes: 65536
  })).result;
  if (!snapshot.items.length || !helper.response.trim()) return false;
  const commit = (await host.call('context.transact', {
    source: snapshot.source, expectedRevision: snapshot.revision, predicates: [], updates: [],
    masks: {expected: Array.from(snapshot.masks).map(mask => [mask.key, mask.id]), removeKeys: [],
      removeOverlaps: [snapshot.boundary],
      add: [{key: 'routine-compaction', through: snapshot.boundary, replacement: helper.response}]}
  })).result;
  if (snapshot.automatic === true || (input && input.continue === true)) await host.call('context.continue', {cursor: commit.cursor});
  return true;
}
