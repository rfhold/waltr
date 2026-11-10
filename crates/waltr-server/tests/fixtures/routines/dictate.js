export default async function () {
  await host.call('media.transcribe', {port: 'audio', output: 'transcript'});
  await host.call('media.finish', {});
  return {completed: true};
}
