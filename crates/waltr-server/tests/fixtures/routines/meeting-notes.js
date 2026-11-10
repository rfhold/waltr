export default async function () {
  const transcript = await host.call('media.collect', {port: 'audio', cap: 262144, snapshot: true});
  const filtered = await host.call('helper.run', {
    selection: {kind: 'named', name: 'routine-conversation-filter'},
    task: 'Filter the attached conversation, capturing retained_transcript and deleted_items separately.',
    capture: 'filtered-conversation', maxTurns: 3, deadlineMs: 120000,
    responseBytes: 65536, completionBytes: 4194304
  }, null, null, transcript);
  const retained = await host.call('media.select', {field: '/retained_transcript'}, null, null, filtered);
  const audit = await host.call('media.select', {field: '/deleted_items'}, null, null, filtered);
  const conversation = await host.call('media.select', {}, null, null, filtered);
  const helper = await host.call('helper.run', {
    selection: {kind: 'named', name: 'routine-conversation-notes'},
    task: 'Write source-grounded notes from the attached retained_transcript and deleted_items. Capture final notes in chosen.',
    capture: 'conversation-notes', maxTurns: 3, deadlineMs: 120000,
    responseBytes: 65536, completionBytes: 4194304
  }, null, null, conversation);
  const notes = await host.call('media.select', {field: '/chosen'}, null, null, helper);
  await host.call('media.publish', {name: 'retained-transcript'}, null, null, retained);
  await host.call('media.publish', {name: 'notes'}, null, null, notes);
  await host.call('media.publish', {name: 'deletion-audit'}, null, null, audit);
  await host.call('media.emit', {output: 'notes'}, null, null, notes);
  await host.call('media.finish', {});
  return {completed: true};
}
