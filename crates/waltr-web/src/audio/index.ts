import workletUrl from "./capture.worklet.js?url&no-inline";
import { CaptureError, PcmConverter, BATCH_FRAMES, MAX_DURATION_SECONDS, MAX_PAYLOAD_BYTES, DEFAULT_PAYLOAD_BYTES } from "./pcm";
export { CaptureError } from "./pcm";
export type { CaptureErrorCode } from "./pcm";

export interface CaptureOptions {
  sampleRate?: number;
  maxDurationSeconds?: number;
  /** PCM-only ceiling; leave room for the later RPC envelope. Hard ceiling reserves 64 KiB. */
  maxPayloadBytes?: number;
  allowEmpty?: boolean;
  onStatus?: (status: CaptureStatus) => void;
}
export type CaptureStatus = "starting" | "recording" | "stopping" | "stopped" | "cancelled" | "error";
export interface CaptureResult {
  bytes: Uint8Array;
  format: { encoding: "PCM_S16LE"; sampleRate: number; channels: 1 };
}
export interface CaptureHandle {
  readonly ready: Promise<void>;
  /** Before ready, stop cancels startup. Otherwise drain already captured frames; repeated calls share one promise. */
  stop(): Promise<CaptureResult>;
  /** Synchronous resource release initiation; rejects unfinished ready/stop with code cancelled. */
  cancel(): void;
}

/** Call synchronously from a user gesture. No devices or contexts are opened on import. */
export function start(options: CaptureOptions = {}): CaptureHandle {
  const rate = options.sampleRate ?? 24000;
  const duration = options.maxDurationSeconds ?? MAX_DURATION_SECONDS;
  const payload = options.maxPayloadBytes ?? DEFAULT_PAYLOAD_BYTES;
  // Validate before any permission request, without retaining a second aggregate buffer.
  new PcmConverter(24000, rate, duration, Math.min(payload, 2));
  if (!Number.isInteger(payload) || payload < 2 || payload > MAX_PAYLOAD_BYTES) throw new RangeError("Invalid payload limit");
  let status: CaptureStatus = "starting";
  let terminal = false;
  let stream: MediaStream | undefined;
  let context: AudioContext | undefined;
  let source: MediaStreamAudioSourceNode | undefined;
  let node: AudioWorkletNode | undefined;
  let converter: PcmConverter | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let stopTimer: ReturnType<typeof setTimeout> | undefined;
  let startupTimer: ReturnType<typeof setTimeout> | undefined;
  let resolveReady!: () => void;
  let rejectReady!: (error: CaptureError) => void;
  let resolveResult!: (result: CaptureResult) => void;
  let rejectResult!: (error: CaptureError) => void;
  const ready = new Promise<void>((resolve, reject) => { resolveReady = resolve; rejectReady = reject; });
  const result = new Promise<CaptureResult>((resolve, reject) => { resolveResult = resolve; rejectResult = reject; });
  void ready.catch(() => {});
  void result.catch(() => {});
  const notify = (next: CaptureStatus) => {
    status = next;
    try { options.onStatus?.(next); } catch { /* Observers do not own capture lifetime. */ }
  };
  const stopTracks = () => {
    stream?.getTracks().forEach(track => { track.onended = null; track.stop(); });
    stream = undefined;
  };
  const cleanup = () => {
    clearTimeout(timer);
    clearTimeout(stopTimer);
    clearTimeout(startupTimer);
    stopTracks();
    source?.disconnect();
    if (node) {
      node.onprocessorerror = null;
      node.port.onmessage = null;
      node.port.close();
      node.disconnect();
    }
    if (context) {
      context.onstatechange = null;
      void context.close().catch(() => {});
    }
    stream = undefined;
    source = undefined;
    node = undefined;
    context = undefined;
    converter = undefined;
  };
  const fail = (error: CaptureError) => {
    if (terminal) return;
    terminal = true;
    cleanup();
    rejectReady(error);
    rejectResult(error);
    notify(error.code === "cancelled" ? "cancelled" : "error");
  };
  const complete = () => {
    if (terminal) return;
    try {
      const bytes = converter!.finish(options.allowEmpty);
      terminal = true;
      cleanup();
      resolveResult({ bytes, format: { encoding: "PCM_S16LE", sampleRate: rate, channels: 1 } });
      notify("stopped");
    } catch (error) { fail(error instanceof CaptureError ? error : new CaptureError("device")); }
  };
  notify("starting");
  void (async () => {
    try {
      if (!globalThis.isSecureContext || !navigator.mediaDevices?.getUserMedia ||
          typeof AudioContext === "undefined" || typeof AudioWorkletNode === "undefined") throw new CaptureError("unsupported");
      context = new AudioContext();
      if (!context.audioWorklet) throw new CaptureError("unsupported");
      // Invoke resume before the first await, preserving transient user activation.
      let resumeComplete = false;
      const resumed = context.resume().then(() => { resumeComplete = true; }, () => { throw new CaptureError("suspended"); });
      startupTimer = setTimeout(() => fail(new CaptureError(resumeComplete ? "device" : "suspended")), 5000);
      const permission = navigator.mediaDevices.getUserMedia({ audio: { channelCount: { ideal: 1 }, echoCancellation: false, noiseSuppression: false, autoGainControl: false } });
      void permission.then(late => {
        if (terminal) late.getTracks().forEach(track => track.stop());
        else stream = late;
      }, () => {});
      // Attach both rejection handlers immediately, even if permission remains pending.
      const [acquired] = await Promise.all([permission, resumed]);
      if (terminal) return;
      clearTimeout(startupTimer);
      stream = acquired;
      if (context.state !== "running") throw new CaptureError("suspended");
      context.onstatechange = () => {
        if (!terminal && context && context.state !== "running") fail(new CaptureError("suspended"));
      };
      for (const track of stream.getTracks()) {
        if (track.readyState === "ended") throw new CaptureError("device");
        track.onended = () => fail(new CaptureError("device"));
      }
      converter = new PcmConverter(context.sampleRate, rate, duration, payload);
      startupTimer = setTimeout(() => fail(new CaptureError("device")), 5000);
      await context.audioWorklet.addModule(workletUrl);
      clearTimeout(startupTimer);
      if (terminal) return;
      if (context.state !== "running") throw new CaptureError("suspended");
      node = new AudioWorkletNode(context, "waltr-pcm-capture", {
        numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1],
        channelCount: 2, channelCountMode: "clamped-max",
        processorOptions: { durationSeconds: duration },
      });
      node.onprocessorerror = () => fail(new CaptureError("device"));
      node.port.onmessage = ({ data }) => {
        if (terminal) return;
        try {
          if (data.type === "error") {
            throw new CaptureError(data.code === "overflow" || data.code === "duration" ? data.code : "device");
          } else if (data.type === "samples") {
            if (!(data.samples instanceof Float32Array) || data.samples.length > BATCH_FRAMES * 2) throw new CaptureError("device");
            converter!.push(data.samples, data.channels);
            node!.port.postMessage({ type: "ack" });
          } else if (data.type === "stopped" && status === "stopping") complete();
          else throw new CaptureError("device");
        } catch (error) { fail(error instanceof CaptureError ? error : new CaptureError("device")); }
      };
      source = context.createMediaStreamSource(stream);
      source.connect(node);
      // Processor output is always silence; connecting keeps the render graph active.
      node.connect(context.destination);
      timer = setTimeout(() => fail(new CaptureError("duration")), duration * 1000);
      notify("recording");
      resolveReady();
    } catch (error) {
      fail(error instanceof CaptureError ? error : new CaptureError(
        error instanceof DOMException && (error.name === "NotAllowedError" || error.name === "SecurityError") ? "permission-denied" : "device",
      ));
    }
  })();
  return {
    ready,
    stop() {
      if (terminal || status === "stopping") return result;
      if (status !== "recording") { fail(new CaptureError("cancelled")); return result; }
      notify("stopping");
      if (terminal) return result;
      clearTimeout(timer);
      stopTracks();
      source?.disconnect();
      source = undefined;
      node!.port.postMessage({ type: "stop" });
      stopTimer = setTimeout(() => fail(new CaptureError("device")), 2000);
      return result;
    },
    cancel() { fail(new CaptureError("cancelled")); },
  };
}
