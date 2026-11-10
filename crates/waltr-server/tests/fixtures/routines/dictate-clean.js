export default async function () {
  const transcript = await host.call('media.collect', {port: 'audio', cap: 1048576, snapshot: true});
  const helper = await host.call('helper.run', {
    selection: {kind: 'named', name: 'routine-cleanup'},
    task: 'Clean the attached transcript and capture the clean text in chosen.', capture: 'clean-text',
    maxTurns: 3, deadlineMs: 120000, responseBytes: 65536, completionBytes: 4194304
  }, null, null, transcript);
  const clean = await host.call('media.select', {field: '/chosen'}, null, null, helper);
  await host.call('media.publish', {name: 'clean-text'}, null, null, clean);
  await host.call('media.emit', {output: 'clean-text'}, null, null, clean);
  await host.call('media.finish', {});
  return {completed: true};
}
