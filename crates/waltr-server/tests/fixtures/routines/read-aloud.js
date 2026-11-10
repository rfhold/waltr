export default async function (input) {
  await host.call('media.synthesize', {port: 'text', output: 'audio', speech: input.speech});
  await host.call('media.finish', {});
  return {completed: true};
}
