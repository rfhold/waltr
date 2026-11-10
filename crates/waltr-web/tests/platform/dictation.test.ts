import { expect, test, vi } from "vitest";
import { browserDictation } from "../../src/platform/browser-dictation";
import { nativeDictation } from "../../src/platform/native-dictation";
import { mediaBridge, routineDefinition, routineSnapshot } from "../support/routines";
import type { LiveCapture } from "../../src/audio/live";

const selection = { routine: "transcribe", trigger: "audio", input: "audio", output: "text" };
export function browserFixture() {
  const snapshot = routineSnapshot(routineDefinition({ routineName: "transcribe", entrypoints: [{ name: "audio", trigger: "interactive", selector: null, startContexts: ["session"] }] }));
  const bridge = mediaBridge();
  let push!: (bytes: Uint8Array) => Promise<void>;
  const handle: LiveCapture = { ready: Promise.resolve(), stop: vi.fn(async () => {}), cancel: vi.fn(async () => {}) };
  const capture = vi.fn((sink: typeof push) => { push = sink; return handle; });
  const host = browserDictation(bridge, () => snapshot, capture);
  return { bridge, handle, capture, host, snapshot, push: (bytes = new Uint8Array([1, 2])) => push(bytes) };
}
test("gesture capture is incremental, Accepted gates input, preview precedes release, finish does not cancel", async () => {
  const f = browserFixture(); const op = f.host.start(selection, "session");
  expect(f.capture).toHaveBeenCalledOnce(); expect(f.bridge.openMediaInteraction).not.toHaveBeenCalled();
  expect(f.host.livePreview).toBe(true);
  await f.push();
  const metadata = JSON.parse(f.bridge.openMediaInteraction.mock.calls[0]![0]);
  expect(metadata).toMatchObject({ routine: "transcribe", revision: "revision/9007199254740993", entrypoint: "audio", sourceSession: "session", inputs: [{ format: "pcm16le", port: "audio", sampleRate: 24000, channels: 1 }] });
  expect(metadata).not.toHaveProperty("pcm");
  expect(f.bridge.pushMediaPcm).toHaveBeenCalledWith(7, "microphone-1", 0, new Uint8Array([1, 2]));
  expect((await op.status()).preview).toBe("hypothesis");
  expect(f.handle.stop).not.toHaveBeenCalled();
  await op.finish(); expect(f.bridge.closeMediaStream).toHaveBeenCalledWith(7, "microphone-1", 1); expect(f.bridge.finishMediaInput).toHaveBeenCalledWith(7);
  expect(f.bridge.cancelMediaInteraction).not.toHaveBeenCalled();
  f.bridge.mediaInteractionStatusJson.mockReturnValue(JSON.stringify({ phase: "succeeded", preview: "final", finalText: "final", cleanupVerified: true }));
  expect(await op.status()).toMatchObject({ phase: "succeeded", finalText: "final" });
});
test("empty capture is neutral and cancellation of a late admission cannot send PCM", async () => {
  const f = browserFixture(); const empty = f.host.start(selection, "session"); await empty.finish();
  expect((await empty.status()).phase).toBe("cancelled"); expect(f.bridge.openMediaInteraction).not.toHaveBeenCalled();
  let resolve!: (id: number) => void;
  f.bridge.openMediaInteraction.mockImplementation(() => new Promise(yes => { resolve = yes; }));
  const operation = f.host.start(selection, "session"); const sending = f.push(); operation.cancel(); resolve(7);
  await expect(sending).rejects.toThrow("Cancelled");
  expect(f.bridge.cancelMediaInteraction).toHaveBeenCalledWith(7); expect(f.bridge.pushMediaPcm).not.toHaveBeenCalled();
});
test("ambiguous send is not retried", async () => {
  const f = browserFixture(); f.host.start(selection, "session");
  f.bridge.pushMediaPcm.mockRejectedValue(new Error("lost ack")); await expect(f.push()).rejects.toThrow("lost ack");
  expect(f.bridge.pushMediaPcm).toHaveBeenCalledOnce(); f.host.dispose();
});
test.each(["cancel", "timeout"])("owned opening %s cancels before acceptance without PCM or admission retry", async mode => {
  vi.useFakeTimers();
  const f = browserFixture();
  try {
    f.bridge.mediaInteractionStatusJson.mockReturnValue(JSON.stringify({ phase: "opening", preview: "", cleanupVerified: false }));
    const operation = f.host.start(selection, "session"), pending = f.push();
    const rejected = expect(pending).rejects.toThrow();
    await vi.advanceTimersByTimeAsync(10);
    if (mode === "cancel") operation.cancel();
    await vi.advanceTimersByTimeAsync(mode === "cancel" ? 10 : 20000);
    await rejected;
    expect(f.bridge.cancelMediaInteraction).toHaveBeenCalledExactlyOnceWith(7);
    expect(f.handle.cancel).toHaveBeenCalledOnce();
    expect(f.bridge.openMediaInteraction).toHaveBeenCalledOnce();
    expect(f.bridge.pushMediaPcm).not.toHaveBeenCalled();
    expect(await operation.status()).toMatchObject({ phase: mode === "cancel" ? "cancelled" : "error", preview: "" });
    expect(() => f.host.start(selection, "session")).not.toThrow();
  } finally { f.host.dispose(); vi.useRealTimers(); }
});
test.each(["removed", "revision", "entrypoint", "ports", "parameters", "selection"])("%s reconciliation clears preview and cancels before further PCM", async change => {
  const f = browserFixture(), operation = f.host.start(selection, "session");
  await f.push(); expect((await operation.status()).preview).toBe("hypothesis");
  const definition = f.snapshot.routines.definitions.definitions[0];
  if (change === "removed") f.snapshot.routines.definitions.definitions = [];
  if (change === "revision") definition.routineRevision = "replacement";
  if (change === "entrypoint") definition.entrypoints[0].startContexts = ["global"];
  if (change === "ports") definition.ports.push({ name: "audio", direction: "input", media: "audio", required: true, maxBytes: "1", formats: ["pcm16le"] });
  if (change === "parameters") definition.parameters.schema = { type: "null" };
  if (change === "selection") f.snapshot.config.current!.sessionDictation = { ...selection, output: "other" };
  f.host.reconcile(f.snapshot);
  expect(f.handle.cancel).toHaveBeenCalledOnce(); expect(f.bridge.cancelMediaInteraction).toHaveBeenCalledExactlyOnceWith(7);
  await expect(f.push()).rejects.toThrow("Cancelled");
  expect(f.bridge.pushMediaPcm).toHaveBeenCalledOnce(); expect(await operation.status()).toMatchObject({ phase: "cancelled", preview: "" });
});
test("unrelated catalog and config changes preserve the pinned dictation", async () => {
  const f = browserFixture(), operation = f.host.start(selection, "session"); await f.push();
  f.snapshot.routines.definitions.definitions.push(routineDefinition({ routineRevision: "other-routine-revision" }));
  f.snapshot.routines.definitions.digest = "new-catalog"; f.snapshot.config.current!.etag = "unrelated-config";
  f.host.reconcile(f.snapshot); await f.push();
  expect(f.bridge.pushMediaPcm).toHaveBeenCalledTimes(2); expect((await operation.status()).preview).toBe("hypothesis"); expect(f.handle.cancel).not.toHaveBeenCalled();
  f.host.dispose();
});
test("catalog replacement during pending admission tombstones the old owner without sending PCM", async () => {
  const f = browserFixture();
  f.bridge.mediaInteractionStatusJson.mockReturnValue(JSON.stringify({ phase: "opening", cleanupVerified: false, preview: "" }));
  const operation = f.host.start(selection, "session"), pending = f.push();
  await vi.waitFor(() => expect(f.bridge.mediaInteractionStatusJson).toHaveBeenCalledWith(7));
  f.snapshot.routines.definitions.definitions[0].routineRevision = "replacement"; f.host.reconcile(f.snapshot);
  expect(f.handle.cancel).toHaveBeenCalledOnce();
  await expect(pending).rejects.toThrow(/cancelled/i); expect(f.bridge.cancelMediaInteraction).toHaveBeenCalledExactlyOnceWith(7);
  expect(f.bridge.pushMediaPcm).not.toHaveBeenCalled(); expect((await operation.status()).preview).toBe("");
});
test("active playback underrun cancels the captured owner without another status poll", async () => {
  const nodes: Array<{ onended: (() => void) | null; buffer: unknown; connect(): void; disconnect(): void; start(): void; stop(): void }> = [];
  class Context {
    currentTime = 0; state = "running"; destination = {};
    resume = async () => {}; close = async () => {};
    createBuffer(_channels: number, frames: number, rate: number) { return { duration: frames / rate, getChannelData: () => new Float32Array(frames) }; }
    createBufferSource() { const node = { onended: null as (() => void) | null, buffer: undefined, connect() {}, disconnect() {}, start() {}, stop() {} }; nodes.push(node); return node; }
  }
  vi.stubGlobal("AudioContext", Context);
  try {
    const f = browserFixture(); f.snapshot.routines.definitions.definitions[0].ports.push({ name: "speech", direction: "output", media: "audio", required: false, maxBytes: "1048576", formats: ["pcm16le"] });
    const operation = f.host.start(selection, "session"); await f.push();
    f.bridge.takeMediaOutputJson.mockReturnValueOnce(JSON.stringify({ type: "open", streamId: "speech", format: { type: "pcm16le", sampleRate: 24000, channels: 1 } })).mockReturnValueOnce(JSON.stringify({ type: "audio", streamId: "speech", chunkId: 1, byteLength: 2 }));
    await operation.status(); nodes[0].onended!();
    expect(f.handle.cancel).toHaveBeenCalledOnce(); expect(f.bridge.cancelMediaInteraction).toHaveBeenCalledExactlyOnceWith(7);
    expect(await operation.status()).toMatchObject({ phase: "error", preview: "" });
  } finally { vi.unstubAllGlobals(); }
});
test("native cancellation tombstones pending admission without PCM or credentials in IPC", async () => {
  let reject!: (error: Error) => void;
  const admission = new Promise<never>((_, no) => { reject = no; });
  const invoke = vi.fn((command: string) => command === "dictation_start" ? admission : Promise.resolve(undefined));
  const host = nativeDictation(invoke as import("../../src/platform/native-dictation").DictationInvoke, 12);
  const op = host.start(selection, "s"); const status = op.status(); expect(op.status()).toBe(status); host.dispose();
  expect(invoke).toHaveBeenCalledWith("dictation_start", { generation: 12, operationId: 1, selection, sessionId: "s" });
  expect(invoke).toHaveBeenCalledWith("dictation_cancel", { generation: 12, operationId: 1 }); reject(new Error("cancelled")); await expect(status).rejects.toThrow();
});
test("native finish/status echo exact identity with no PCM", async () => {
  const invoke = vi.fn(async (_command: string, payload: Record<string, unknown>) => ({ generation: payload.generation, operationId: payload.operationId, phase: "recording", preview: "" }));
  const host = nativeDictation(invoke as import("../../src/platform/native-dictation").DictationInvoke, 4); const op = host.start(selection, "s"); await op.finish(); await op.status();
  expect(invoke).toHaveBeenCalledWith("dictation_finish", { generation: 4, operationId: 1 }); expect(invoke).toHaveBeenCalledWith("dictation_status", { generation: 4, operationId: 1 }); host.dispose();
});
