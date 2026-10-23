import { CaptureError, start, type CaptureHandle } from "../audio";
import { validSelection, type DictationHost, type DictationOperation, type DictationStatus } from "./dictation";
export type AudioRunBridge = {
  startAudioRun(metadata: string, pcm: Uint8Array): number;
  audioRunStatusJson(operationId: number): string;
  cancelAudioRun(operationId: number): void;
};
export function browserDictation(bridge: AudioRunBridge, capture: typeof start = start): DictationHost {
  let disposed = false;
  let next = 0;
  let active: DictationOperation | undefined;
  return {
    start(selection, sessionId) {
      if (disposed || active) throw new Error("Microphone is busy or unavailable.");
      if (!validSelection(selection) || !sessionId || sessionId.length > 256) throw new Error("Configure a Routine first.");
      const id = ++next;
      let runId: number | undefined;
      let cancelled = false;
      let finishing: Promise<void> | undefined;
      let state: DictationStatus = { operationId: id, phase: "starting", preview: "" };
      let handle: CaptureHandle;
      const captureFailed = (error: unknown) => {
        if (!cancelled) {
          cancelled = error instanceof CaptureError && error.code === "cancelled";
          state = { ...state, phase: cancelled ? "cancelled" : "error", preview: "" };
        }
        if (active === operation) active = undefined;
      };
      const operation: DictationOperation = {
        operationId: id,
        async status() {
          if (runId !== undefined && !cancelled) {
            const status = JSON.parse(bridge.audioRunStatusJson(runId)) as DictationStatus;
            state = { ...status, operationId: id };
            if (state.phase !== "processing" && active === operation) active = undefined;
          }
          return state;
        },
        finish() {
          if (finishing) return finishing;
          finishing = (async () => {
            if (cancelled) return;
            state = { ...state, phase: "processing" };
            try {
              const result = await handle.stop();
              if (cancelled) return;
              if (result.format.encoding !== "PCM_S16LE" || result.format.channels !== 1 || result.format.sampleRate !== 24000 || result.bytes.byteLength > 32 * 1024 * 1024 - 65536) throw new Error("Invalid audio format or size");
              if (result.bytes.byteLength === 0) {
                captureFailed(new CaptureError("cancelled"));
                return;
              }
              runId = bridge.startAudioRun(JSON.stringify({ ...selection, sessionId }), result.bytes);
            } catch (error) {
              captureFailed(error);
            }
          })();
          return finishing;
        },
        cancel() {
          if (cancelled) return;
          cancelled = true;
          handle.cancel();
          if (runId !== undefined) bridge.cancelAudioRun(runId);
          state = { operationId: id, phase: "cancelled", preview: "" };
          if (active === operation) active = undefined;
        },
      };
      // This call must remain before any await to preserve transient user activation.
      handle = capture({ sampleRate: 24000, allowEmpty: true, onStatus(status) {
        if (status === "stopped" && !finishing && !cancelled) {
          queueMicrotask(() => { if (!cancelled) void operation.finish(); });
        }
        if (status === "error" || status === "cancelled") {
          captureFailed(new CaptureError(status === "cancelled" ? "cancelled" : "device"));
        }
      } });
      if (!cancelled && state.phase !== "error") active = operation;
      void handle.ready.then(() => {
        if (!cancelled && !finishing) state = { ...state, phase: "recording" };
      }, captureFailed);
      return operation;
    },
    cancel() { active?.cancel(); },
    dispose() { disposed = true; active?.cancel(); },
  };
}
