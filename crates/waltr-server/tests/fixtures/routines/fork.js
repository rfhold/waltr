export default async function (input) {
  if (input.direction.trim().length === 0) throw new Error('direction required');
  const source = (await host.call('context.snapshot', {})).result;
  const helper = (await host.call('helper.run', {
    selection: {kind: 'named', name: 'routine-fork'}, source: source.source,
    task: '<fork-direction>\n' + input.direction + '\n</fork-direction>',
    maxTurns: 3,
    deadlineMs: 20000, responseBytes: 24576, completionBytes: 32768
  })).result;
  const destination = (await host.call('context.create', {
    source: source.source, copyHistory: false, inheritDefinition: true, inheritInference: true,
    message: helper.response + '\n\n<fork-direction>\n' + input.direction + '\n</fork-direction>'
  })).result;
  const started = (await host.call('context.start', {destination: destination.destination})).result;
  return {sessionId: destination.sessionId, created: destination.created, started: started.started};
}
