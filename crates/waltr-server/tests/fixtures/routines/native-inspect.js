export default async function (input) {
  const source = await host.call('media.upload', {port: 'source'});
  const content = [{type: 'text', text: 'Inspect the attached source. Capture the answer in chosen.'}];
  if (input.image) {
    await host.call('helper.image.bind', {name: 'source'}, null, null, source);
    content.push({type: 'image', image: {source: 'source'}});
  }
  const helperArgs = {selection: {kind: 'named', name: 'routine-inspect'}, content, capture: 'inspection', maxTurns: 3, deadlineMs: 20000, responseBytes: 65536, completionBytes: 4194304};
  const helper = input.image ? await host.call('helper.run', helperArgs) : await host.call('helper.run', helperArgs, null, null, source);
  const answer = await host.call('media.select', {field: '/chosen'}, null, null, helper);
  await host.call('media.publish', {name: 'answer'}, null, null, answer);
  await host.call('media.emit', {output: 'answer'}, null, null, answer);
  await host.call('media.finish', {});
  return {completed: true};
}
