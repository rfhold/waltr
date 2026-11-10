import workletUrl from "./live.worklet.js?url&no-inline";
import { CaptureError } from "./pcm";

export type LiveCapture = { ready: Promise<void>; stop(): Promise<void>; cancel(): Promise<void> };
export function startLiveCapture(push: (bytes: Uint8Array) => Promise<void>, failed: () => void, sampleRate = 24000, stopped: () => void = () => {}): LiveCapture {
  let context: AudioContext | undefined;
  let tracks: MediaStream | undefined;
  let source: MediaStreamAudioSourceNode | undefined;
  let node: AudioWorkletNode | undefined;
  let ended = false;
  let stopping = false;
  let writing = false;
  let cleanup: Promise<void> | undefined;
  let resolve!: () => void;
  let reject!: (error: unknown) => void;
  let timer: ReturnType<typeof setTimeout>;
  let startupTimer: ReturnType<typeof setTimeout>;
  let stopTimer: ReturnType<typeof setTimeout>;
  let rejectStartup!: (error: unknown) => void;
  const startupCancelled = new Promise<never>((_, no) => { rejectStartup = no; });
  const done = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
  void done.catch(() => undefined);
  function teardown() {
    if (cleanup) return cleanup;
    clearTimeout(timer);
    clearTimeout(startupTimer); clearTimeout(stopTimer);
    tracks?.getTracks().forEach(t => { t.onended = null; t.stop(); });
    source?.disconnect();
    if (node) { node.port.onmessage = null; node.port.close(); node.disconnect(); node.onprocessorerror = null; }
    if (context) context.onstatechange = null;
    cleanup = context?.close().catch(() => undefined) ?? Promise.resolve();
    return cleanup;
  }
  function fail() {
    if (ended) return;
    ended = true; rejectStartup(new CaptureError("device")); void teardown().then(() => reject(new CaptureError("device"))); failed();
  }
  // Open/resume and request permission in the initiating gesture.
  const setup = (async () => {
    try {
      if (!globalThis.isSecureContext || !navigator.mediaDevices?.getUserMedia || typeof AudioContext === "undefined") throw new CaptureError("unsupported");
      context = new AudioContext();
      startupTimer = setTimeout(fail, 5000);
      const resumed = context.resume();
      const permission = navigator.mediaDevices.getUserMedia({ audio: { channelCount: { ideal: 1 }, echoCancellation: false, noiseSuppression: false, autoGainControl: false } }).then(stream => {
        if (ended) stream.getTracks().forEach(t => t.stop());
        else tracks = stream;
        return stream;
      });
      const [stream] = await Promise.all([permission, resumed]);
      if (ended) return;
      clearTimeout(startupTimer); startupTimer = setTimeout(fail, 5000);
      await context.audioWorklet.addModule(workletUrl);
      if (ended) return;
      clearTimeout(startupTimer);
      if (context.state !== "running") throw new CaptureError("suspended");
      context.onstatechange = () => { if (!ended && context?.state !== "running") fail(); };
      node = new AudioWorkletNode(context, "waltr-live-pcm", { numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1], channelCount: 2, channelCountMode: "clamped-max", processorOptions: { sampleRate } });
      node.onprocessorerror = fail;
      node.port.onmessage = ({ data }) => {
        if (ended) return;
        if (data.type === "stopped") { ended = true; void teardown().then(() => { resolve(); stopped(); }); return; }
        if (data.type !== "pcm" || writing || !(data.bytes instanceof Uint8Array) || !data.bytes.length || data.bytes.length > 65536 || data.bytes.length % 2) { fail(); return; }
        writing = true;
        void push(data.bytes).then(() => { writing = false; if (!ended) node?.port.postMessage({ type: "ack" }); }, fail);
      };
      for (const track of stream.getTracks()) { if (track.readyState === "ended") throw new CaptureError("device"); track.onended = fail; }
      source = context.createMediaStreamSource(stream); source.connect(node); node.connect(context.destination);
      if (stopping) node.port.postMessage({ type: "stop" });
      timer = setTimeout(() => { stopping = true; node?.port.postMessage({ type: "stop" }); }, 3600000);
    } catch (error) { fail(); throw error; }
  })();
  const ready = Promise.race([setup, startupCancelled]);
  void ready.catch(() => undefined);
  return {
    ready,
    async stop() { if (!ended) { stopping = true; await ready; if (!ended) { node?.port.postMessage({ type: "stop" }); stopTimer = setTimeout(fail, 20000); } } await done; },
    async cancel() { if (!ended) { ended = true; rejectStartup(new CaptureError("cancelled")); reject(new CaptureError("cancelled")); } await teardown(); },
  };
}
