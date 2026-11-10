export default async function (input) {
  const categorized = (await host.call('children.call', {calls: [
    {key: 'categorization', definition: 'context-categorization', entrypoint: 'child', input: null}
  ]})).result;
  if (categorized.length !== 1 || !categorized[0].succeeded || !categorized[0].cleanupComplete) throw new Error('categorization did not complete');
  const snapshot = (await host.call('context.original', {})).result;
  const checkpoint = snapshot.metadata['routine.categories.v1'];
  if (!checkpoint || checkpoint.version !== 1 || checkpoint.through !== snapshot.boundary) throw new Error('stale category checkpoint');
  const categories = Object.create(null);
  for (const category of Array.from(checkpoint.categories)) categories[category.key] = {...category, item_ids: []};
  for (const item of Array.from(snapshot.items)) {
    const key = (snapshot.itemMetadata[item.item_id] || {})['routine.category.v1'];
    if (!Object.prototype.hasOwnProperty.call(categories, key)) throw new Error('incomplete category coverage');
    categories[key].item_ids.push(item.item_id);
  }
  const coverage = {categories, checkpoint};
  const helper = (await host.call('helper.run', {
    selection: {kind: 'named', name: 'routine-context-strategy'},
    source: snapshot.source,
    task: JSON.stringify({coverage, instruction: input.instruction}),
    capture: 'context-strategy',
    maxTurns: 3,
    deadlineMs: 20000, responseBytes: 65536, completionBytes: 65536
  })).result;
  if (helper.captures.length !== 1) throw new Error('one strategy required');
  const strategy = helper.captures[0].value;
  if (strategy.kind === 'no_change') return {decisions: []};
  if (strategy.kind !== 'edit' || strategy.edits.length === 0 || strategy.edits.length > 4) throw new Error('invalid strategy');
  const keys = new Set();
  for (const edit of Array.from(strategy.edits)) {
    if (!/^[A-Za-z0-9_.-]{1,119}$/.test(edit.key) || keys.has(edit.key) || !Object.prototype.hasOwnProperty.call(coverage.categories, edit.key) || coverage.categories[edit.key].item_ids.length === 0) throw new Error('invalid category edit');
    keys.add(edit.key);
  }
  const children = (await host.call('children.call', {calls: Array.from(strategy.edits, edit => ({
    key: edit.key, definition: 'manage-category-context', entrypoint: 'child', input: {...edit, planOnly: true}
  }))})).result;
  const decisions = Array.from(children, child => ({...child, result: child.result ? {...child.result} : null}));
  if (Array.from(decisions).some(child => !child.succeeded || !child.cleanupComplete || !child.result)) throw new Error('category child failed');
  const plans = Array.from(decisions).map(child => child.result);
  const replacements = plans.filter(plan => plan.decision === 'replace');
  for (const child of decisions) if (child.result.decision === 'keep') {
    child.result.maskId = Array.from(snapshot.masks).find(mask => mask.key === 'category.' + child.result.key).id;
  }
  if (replacements.length) {
    const commit = (await host.call('context.transact', {
      source: snapshot.source, expectedRevision: snapshot.revision,
      predicates: [{key: 'routine.categories.v1', value: {state: 'present', value: checkpoint}}], updates: [],
      masks: {expected: Array.from(snapshot.masks).map(mask => [mask.key, mask.id]),
        removeKeys: replacements.map(plan => 'category.' + plan.key), removeOverlaps: [],
        add: replacements.map(plan => ({key: 'category.' + plan.key, through: snapshot.boundary,
          itemIds: categories[plan.key].item_ids, replacement: plan.summary}))}
    })).result;
    for (const child of Array.from(decisions)) if (child.result.decision === 'replace') {
      child.result.applied = true;
      child.result.maskId = Array.from(commit.addedMasks).find(mask => mask.key === 'category.' + child.result.key).id;
    }
  }
  return {decisions};
}
