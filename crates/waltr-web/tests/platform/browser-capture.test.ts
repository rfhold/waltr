import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { browserDictation } from "../../src/platform/browser-dictation";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const selection = { routine: "transcribe", trigger: "audio", input: "audio", output: "text" };
let permission: ReturnType<typeof deferred<MediaStream>>;
let resume: ReturnType<typeof deferred<void>>;
let module: ReturnType<typeof deferred<void>>;
let context: FakeContext;
const nodes: FakeNode[] = [];
const track = { stop: vi.fn(), onended: null as (() => void) | null, readyState: "live" };
const stream = { getTracks: () => [track] } as unknown as MediaStream;
class FakeContext {
  state = "running";
  sampleRate = 48000;
  destination = {};
  onstatechange: (() => void) | null = null;
  audioWorklet = { addModule: vi.fn(() => module.promise) };
  resume = vi.fn(() => resume.promise);
  close = vi.fn(async () => {});
  source = { connect: vi.fn(), disconnect: vi.fn() };
  createMediaStreamSource = vi.fn(() => this.source);
  constructor() { context = this; }
}
class FakeNode {
  onprocessorerror: (() => void) | null = null;
  port = { onmessage: null as ((event: { data: unknown }) => void) | null, postMessage: vi.fn(), close: vi.fn() };
  connect = vi.fn();
  disconnect = vi.fn();
  constructor() { nodes.push(this); }
  emit(data: unknown) { this.port.onmessage?.({ data }); }
}
const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
function fixture() {
  const bridge = {
    startAudioRun: vi.fn(() => 7),
    audioRunStatusJson: vi.fn(() => JSON.stringify({ phase: "succeeded", preview: "clean", finalText: "clean" })),
    cancelAudioRun: vi.fn(),
  };
  // No capture injection: adapter, capture ownership, and PCM conversion are real.
  const host = browserDictation(bridge);
  const operation = host.start(selection, "session");
  return { bridge, host, operation };
}
function released() {
  expect(context.close).toHaveBeenCalledTimes(1);
  if (track.stop.mock.calls.length) expect(track.stop).toHaveBeenCalledTimes(1);
  for (const node of nodes) {
    expect(node.port.close).toHaveBeenCalledTimes(1);
    expect(node.disconnect).toHaveBeenCalledTimes(1);
  }
}
beforeEach(() => {
  vi.useFakeTimers();
  permission = deferred<MediaStream>(); resume = deferred<void>(); module = deferred<void>();
  nodes.length = 0; track.stop.mockClear(); track.onended = null;
  vi.stubGlobal("isSecureContext", true);
  vi.stubGlobal("navigator", { mediaDevices: { getUserMedia: vi.fn(() => permission.promise) } });
  vi.stubGlobal("AudioContext", FakeContext);
  vi.stubGlobal("AudioWorkletNode", FakeNode);
});
afterEach(() => { vi.unstubAllGlobals(); vi.useRealTimers(); });

describe("real browser dictation capture composition", () => {
  it.each(["permission", "resume", "worklet"])("release during pending %s remains cancelled after late completion", async pending => {
    const f = fixture();
    expect(context.resume).toHaveBeenCalledOnce();
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledOnce();
    if (pending !== "permission") permission.resolve(stream);
    if (pending !== "resume") resume.resolve();
    if (pending !== "worklet") module.resolve();
    await settle();
    expect((await f.operation.status()).phase).toBe("starting");
    await f.operation.finish();
    expect(context.close).toHaveBeenCalledTimes(1);
    expect((await f.operation.status()).phase).toBe("cancelled");
    permission.resolve(stream); resume.resolve(); module.resolve();
    await settle(); await vi.advanceTimersByTimeAsync(10000);
    expect((await f.operation.status()).phase).toBe("cancelled");
    expect(nodes).toHaveLength(0);
    expect(context.createMediaStreamSource).not.toHaveBeenCalled();
    expect(track.stop).toHaveBeenCalledTimes(1);
    expect(f.bridge.startAudioRun).not.toHaveBeenCalled();
    released();
    f.host.start(selection, "next").cancel();
  });

  it("bounds the whole acquisition when resume completes but permission stays pending", async () => {
    const f = fixture(); resume.resolve(); module.resolve(); await settle();
    await vi.advanceTimersByTimeAsync(4999);
    expect((await f.operation.status()).phase).toBe("starting");
    await vi.advanceTimersByTimeAsync(1);
    expect((await f.operation.status()).phase).toBe("error");
    expect(context.close).toHaveBeenCalledTimes(1);
    permission.resolve(stream); await settle();
    await f.operation.finish();
    expect((await f.operation.status()).phase).toBe("error");
    expect(track.stop).toHaveBeenCalledTimes(1);
    expect(nodes).toHaveLength(0);
    expect(f.bridge.startAudioRun).not.toHaveBeenCalled(); released();
  });

  it("keeps the separate five-second worklet startup bound", async () => {
    const f = fixture(); permission.resolve(stream); resume.resolve(); await settle();
    await vi.advanceTimersByTimeAsync(5000);
    expect((await f.operation.status()).phase).toBe("error");
    module.resolve(); await settle();
    expect(nodes).toHaveLength(0);
    expect(f.bridge.startAudioRun).not.toHaveBeenCalled(); released();
  });

  it("post-ready release without samples is neutral and never admits a Run", async () => {
    const f = fixture(); permission.resolve(stream); resume.resolve(); module.resolve(); await settle();
    expect((await f.operation.status()).phase).toBe("recording");
    const finish = f.operation.finish(); nodes[0].emit({ type: "stopped" }); await finish; await settle();
    expect((await f.operation.status()).phase).toBe("cancelled");
    expect(f.bridge.startAudioRun).not.toHaveBeenCalled(); released();
  });

  it("drains queued PCM on release and admits exactly one Run with the selected final", async () => {
    const f = fixture(); permission.resolve(stream); resume.resolve(); module.resolve(); await settle();
    nodes[0].emit({ type: "samples", channels: 1, samples: new Float32Array([0, 1, 0, -1]) });
    const finish = f.operation.finish(); expect(f.operation.finish()).toBe(finish);
    nodes[0].emit({ type: "samples", channels: 1, samples: new Float32Array([0, 1]) });
    nodes[0].emit({ type: "stopped" }); await finish; await settle();
    expect(f.bridge.startAudioRun).toHaveBeenCalledExactlyOnceWith(JSON.stringify({ ...selection, sessionId: "session" }), new Uint8Array([0, 0, 0, 0, 0, 0]));
    expect(await f.operation.status()).toMatchObject({ phase: "succeeded", finalText: "clean" });
    expect(context.source.disconnect).toHaveBeenCalledTimes(1);
    await f.operation.finish(); expect(f.bridge.startAudioRun).toHaveBeenCalledOnce(); released();
  });

  it.each(["NotAllowedError", "NotFoundError"])("preserves actual %s as failure even after release", async name => {
    const f = fixture(); resume.resolve();
    permission.reject(new DOMException("private device detail", name)); await settle();
    expect((await f.operation.status()).phase).toBe("error");
    await f.operation.finish(); await settle();
    expect((await f.operation.status()).phase).toBe("error");
    expect(f.bridge.startAudioRun).not.toHaveBeenCalled(); released();
  });

  it.each(["reject", "timeout", "dispose"])("cleans late permission after resume %s without restoring capture", async reason => {
    const f = fixture();
    if (reason === "reject") resume.reject(new Error("private autoplay detail"));
    if (reason === "timeout") await vi.advanceTimersByTimeAsync(5000);
    if (reason === "dispose") f.host.dispose();
    await settle(); permission.resolve(stream); resume.resolve(); module.resolve(); await settle();
    expect((await f.operation.status()).phase).toBe(reason === "dispose" ? "cancelled" : "error");
    expect(track.stop).toHaveBeenCalledTimes(1);
    expect(nodes).toHaveLength(0);
    expect(f.bridge.startAudioRun).not.toHaveBeenCalled(); released();
  });
});
