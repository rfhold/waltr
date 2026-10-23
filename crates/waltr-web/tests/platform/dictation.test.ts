import { describe, expect, test, vi } from "vitest";
import { browserDictation } from "../../src/platform/browser-dictation";
import { nativeDictation } from "../../src/platform/native-dictation";
import type { CaptureOptions, CaptureResult } from "../../src/audio";
const selection = { routine: "transcribe", trigger: "audio", input: "audio", output: "text" };
function deferred<T>() { let resolve!: (value: T) => void; let reject!: (reason: unknown) => void; const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; }
function fixture() {
  const ready = deferred<void>();
  const stopped = deferred<CaptureResult>();
  const handle = { ready: ready.promise, stop: vi.fn(() => stopped.promise), cancel: vi.fn() };
  const capture = vi.fn((_options?: CaptureOptions) => handle);
  const bridge = { startAudioRun: vi.fn(() => 7), audioRunStatusJson: vi.fn(() => JSON.stringify({ operationId: 7, phase: "processing", preview: "hypothesis", finalText: null })), cancelAudioRun: vi.fn() };
  return { ready, stopped, handle, capture, bridge, host: browserDictation(bridge, capture) };
}
const pcm: CaptureResult = { bytes: new Uint8Array([1, 2]), format: { encoding: "PCM_S16LE", sampleRate: 24000, channels: 1 } };
describe("dictation hosts", () => {
  test("gesture starts capture synchronously, binary complete Run and coalesced pull", async () => {
    const f = fixture();
    expect(f.host.livePreview).not.toBe(true);
    const op = f.host.start(selection, "session");
    expect(f.capture).toHaveBeenCalledOnce();
    expect(() => f.host.start(selection, "other")).toThrow("busy");
    f.ready.resolve(); await Promise.resolve();
    expect((await op.status()).phase).toBe("recording");
    const finish = op.finish(); f.stopped.resolve(pcm); await finish;
    expect(f.bridge.startAudioRun).toHaveBeenCalledWith(JSON.stringify({ ...selection, sessionId: "session" }), pcm.bytes);
    expect((await op.status()).preview).toBe("hypothesis");
    f.bridge.audioRunStatusJson.mockReturnValue(JSON.stringify({ operationId: 7, phase: "succeeded", preview: "final", finalText: "final" }));
    expect((await op.status()).finalText).toBe("final");
    f.host.start(selection, "second"); f.host.dispose();
    expect(f.handle.cancel).toHaveBeenCalledOnce();
    expect(() => f.host.start(selection, "third")).toThrow("unavailable");
  });
  test("cancel during startup or stop admission cannot dispatch late PCM", async () => {
    const f = fixture(); const op = f.host.start(selection, "session");
    const finish = op.finish(); f.host.dispose();
    f.ready.resolve(); f.stopped.resolve(pcm); await finish;
    expect(f.bridge.startAudioRun).not.toHaveBeenCalled();
    expect((await op.status()).phase).toBe("cancelled");
  });
  test("active Run is cancelled on host disposal", async () => {
    const f = fixture(); const op = f.host.start(selection, "session");
    f.stopped.resolve(pcm); await op.finish(); f.host.dispose();
    expect(f.bridge.cancelAudioRun).toHaveBeenCalledWith(7);
  });
  test("permission denial and malformed payload never admit Run", async () => {
    const f = fixture(); const op = f.host.start(selection, "session");
    f.ready.reject(new Error("denied")); await Promise.resolve();
    expect((await op.status()).phase).toBe("error");
    expect(f.bridge.startAudioRun).not.toHaveBeenCalled();
    const next = f.host.start(selection, "next");
    const finish = next.finish(); f.stopped.resolve({ ...pcm, format: { ...pcm.format, sampleRate: 48000 } }); await finish;
    expect((await next.status()).phase).toBe("error");
    expect(f.bridge.startAudioRun).not.toHaveBeenCalled();
  });
  test("native contract carries only generation, operation, selection and session; cancellation tombstones pending admission", async () => {
    const admission = deferred<never>();
    const invoke = vi.fn((command: string) => command === "dictation_start" ? admission.promise : Promise.resolve(undefined));
    const host = nativeDictation(invoke as import("../../src/platform/native-dictation").DictationInvoke, 12);
    const op = host.start(selection, "s");
    expect(invoke).toHaveBeenCalledWith("dictation_start", { generation: 12, operationId: 1, selection, sessionId: "s" });
    const pending = op.status();
    expect(op.status()).toBe(pending);
    host.dispose();
    expect(invoke).toHaveBeenCalledWith("dictation_cancel", { generation: 12, operationId: 1 });
    admission.reject(new Error("cancelled")); await expect(pending).rejects.toThrow();
    expect(invoke).toHaveBeenCalledTimes(2);
  });
  test("native finish/status echo exact identity with no PCM", async () => {
    const invoke = vi.fn(async (_command: string, payload: Record<string, unknown>) => ({ generation: payload.generation, operationId: payload.operationId, phase: "recording", preview: "" }));
    const host = nativeDictation(invoke as import("../../src/platform/native-dictation").DictationInvoke, 4);
    expect(host.livePreview).toBe(true);
    const op = host.start(selection, "s");
    await op.finish(); await op.status();
    expect(invoke).toHaveBeenCalledWith("dictation_finish", { generation: 4, operationId: 1 });
    expect(invoke).toHaveBeenCalledWith("dictation_status", { generation: 4, operationId: 1 });
    host.dispose();
  });
});
