export default async function () {
  const source = await host.call('media.upload', {port: 'source'});
  const helper = await host.call('helper.run', {selection: {kind: 'named', name: 'routine-inspect'}, task: 'Inspect the attached source. Capture the answer in chosen.', capture: 'inspection', maxTurns: 3, deadlineMs: 20000, responseBytes: 65536, completionBytes: 4194304}, null, null, source);
  const answer = await host.call('media.select', {field: '/chosen'}, null, null, helper);
  await host.call('media.publish', {name: 'answer'}, null, null, answer);
  await host.call('media.emit', {output: 'answer'}, null, null, answer);
  await host.call('media.finish', {});
  return {completed: true};
}
