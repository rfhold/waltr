import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { start } from "../../src/audio";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const tracks: { stop: ReturnType<typeof vi.fn>; onended: (() => void) | null; readyState: string }[] = [];
const contexts: FakeContext[] = [];
const nodes: FakeNode[] = [];
let resumePromise: Promise<void> | undefined;
class FakeContext {
  state = "running";
  sampleRate = 48000;
  destination = {};
  onstatechange: (() => void) | null = null;
  audioWorklet = { addModule: vi.fn(async () => {}) };
  resume = vi.fn(() => resumePromise ?? Promise.resolve());
  close = vi.fn(async () => {});
  source = { connect: vi.fn(), disconnect: vi.fn() };
  createMediaStreamSource = vi.fn(() => this.source);
  constructor() { contexts.push(this); }
}
class FakeNode {
  onprocessorerror: (() => void) | null = null;
  port = { onmessage: null as ((event: { data: unknown }) => void) | null, postMessage: vi.fn(), close: vi.fn() };
  connect = vi.fn();
  disconnect = vi.fn();
  constructor() { nodes.push(this); }
  emit(data: unknown) { this.port.onmessage?.({ data }); }
}
function media() {
  const track = { stop: vi.fn(), onended: null as (() => void) | null, readyState: "live" };
  tracks.push(track);
  return { getTracks: () => [track] } as unknown as MediaStream;
}
let permission: ReturnType<typeof deferred<MediaStream>>;
let getUserMedia: ReturnType<typeof vi.fn>;
beforeEach(() => {
  tracks.length = contexts.length = nodes.length = 0;
  resumePromise = undefined;
  permission = deferred<MediaStream>();
  getUserMedia = vi.fn(() => permission.promise);
  vi.stubGlobal("isSecureContext", true);
  vi.stubGlobal("navigator", { mediaDevices: { getUserMedia } });
  vi.stubGlobal("AudioContext", FakeContext);
  vi.stubGlobal("AudioWorkletNode", FakeNode);
});
afterEach(() => { vi.unstubAllGlobals(); vi.useRealTimers(); });
function released() {
  for (const track of tracks) expect(track.stop).toHaveBeenCalled();
  for (const context of contexts) expect(context.close).toHaveBeenCalledTimes(1);
  for (const node of nodes) {
    expect(node.disconnect).toHaveBeenCalled();
    expect(node.port.close).toHaveBeenCalledTimes(1);
  }
}
async function recording(options: Parameters<typeof start>[0] = {}) {
  const handle = start(options);
  permission.resolve(media());
  await handle.ready;
  return handle;
}

describe("browser capture ownership", () => {
  it("finalizes queued frames and shares concurrent/repeated stop results", async () => {
    const status = vi.fn();
    const handle = await recording({ onStatus: status });
    nodes[0].emit({ type: "samples", channels: 1, samples: new Float32Array([0, 1, 0, -1]) });
    const first = handle.stop();
    expect(handle.stop()).toBe(first);
    expect(tracks[0].stop).toHaveBeenCalled();
    nodes[0].emit({ type: "samples", channels: 1, samples: new Float32Array([0, 1]) });
    nodes[0].emit({ type: "stopped" });
    const result = await first;
    expect(result.format).toEqual({ encoding: "PCM_S16LE", sampleRate: 24000, channels: 1 });
    expect(result.bytes.length).toBe(6);
    const count = status.mock.calls.length;
    handle.cancel();
    nodes[0].emit({ type: "error", code: "overflow" });
    expect(status).toHaveBeenCalledTimes(count);
    expect(await handle.stop()).toBe(result);
    released();
  });

  it("cancels pending permission promptly and stops a late grant", async () => {
    const status = vi.fn();
    const handle = start({ onStatus: status });
    handle.cancel();
    handle.cancel();
    await expect(handle.ready).rejects.toMatchObject({ code: "cancelled" });
    await expect(handle.stop()).rejects.toMatchObject({ code: "cancelled" });
    permission.resolve(media());
    await permission.promise;
    await Promise.resolve();
    expect(nodes).toHaveLength(0);
    expect(status.mock.calls.map(([value]) => value)).toEqual(["starting", "cancelled"]);
    released();
  });

  it("cleans up permission denial and unsupported backend without a device prompt", async () => {
    const denied = start();
    permission.reject(new DOMException("denied", "NotAllowedError"));
    await expect(denied.ready).rejects.toMatchObject({ code: "permission-denied" });
    released();
    getUserMedia.mockClear();
    vi.stubGlobal("isSecureContext", false);
    await expect(start().ready).rejects.toMatchObject({ code: "unsupported" });
    expect(getUserMedia).not.toHaveBeenCalled();
  });

  it.each(["overflow", "duration", "device"])("fails closed on worklet %s", async code => {
    const status = vi.fn();
    const handle = await recording({ onStatus: status });
    const lateMessage = nodes[0].port.onmessage!;
    nodes[0].emit({ type: "error", code });
    await expect(handle.stop()).rejects.toMatchObject({ code });
    lateMessage({ data: { type: "samples", channels: 1, samples: new Float32Array([1]) } });
    expect(status.mock.calls.map(([value]) => value)).toEqual(["starting", "recording", "error"]);
    released();
  });

  it.each(["cancel", "device-ended", "processor", "suspended"])("releases every resource after %s", async reason => {
    const handle = await recording();
    if (reason === "cancel") handle.cancel();
    if (reason === "device-ended") tracks[0].onended!();
    if (reason === "processor") nodes[0].onprocessorerror!();
    if (reason === "suspended") { contexts[0].state = "suspended"; contexts[0].onstatechange!(); }
    await expect(handle.stop()).rejects.toMatchObject({ code: reason === "cancel" ? "cancelled" : reason === "suspended" ? "suspended" : "device" });
    released();
  });

  it("rejects oversized payload before permission and enforces the PCM ceiling", async () => {
    expect(() => start({ maxPayloadBytes: 32 * 1024 * 1024 })).toThrow(RangeError);
    expect(getUserMedia).not.toHaveBeenCalled();
    const handle = await recording({ maxPayloadBytes: 2 });
    nodes[0].emit({ type: "samples", channels: 1, samples: new Float32Array([0, 0, 0, 0]) });
    await expect(handle.stop()).rejects.toMatchObject({ code: "overflow" });
    released();
  });

  it("bounds elapsed duration and missing stop acknowledgement", async () => {
    vi.useFakeTimers();
    const handle = await recording({ maxDurationSeconds: 1 });
    vi.advanceTimersByTime(1000);
    await expect(handle.stop()).rejects.toMatchObject({ code: "duration" });
    released();
  });

  it("bounds a stalled stop", async () => {
    vi.useFakeTimers();
    const handle = await recording();
    const result = handle.stop();
    vi.advanceTimersByTime(2000);
    await expect(result).rejects.toMatchObject({ code: "device" });
    released();
  });

  it.each([false, true])("defines empty results with allowEmpty=%s", async allowEmpty => {
    const handle = await recording({ allowEmpty });
    const result = handle.stop();
    nodes[0].emit({ type: "stopped" });
    if (allowEmpty) expect((await result).bytes).toEqual(new Uint8Array());
    else await expect(result).rejects.toMatchObject({ code: "empty" });
    released();
  });

  it("cancel wins over an unfinished stop, including a late stopped message", async () => {
    const handle = await recording();
    const late = nodes[0].port.onmessage!;
    const result = handle.stop();
    handle.cancel();
    late({ data: { type: "stopped" } });
    await expect(result).rejects.toMatchObject({ code: "cancelled" });
    released();
  });

  it("rejects malformed worklet input", async () => {
    const handle = await recording();
    nodes[0].emit({ type: "samples", channels: 2, samples: new Float32Array([1]) });
    await expect(handle.stop()).rejects.toMatchObject({ code: "device" });
    released();
  });

  it("cleans up a module load failure", async () => {
    const handle = start();
    contexts[0].audioWorklet.addModule.mockRejectedValue(new Error("backend unavailable"));
    permission.resolve(media());
    await expect(handle.ready).rejects.toMatchObject({ code: "device" });
    released();
  });

  it("bounds a stalled module load", async () => {
    vi.useFakeTimers();
    const module = deferred<void>();
    const handle = start();
    contexts[0].audioWorklet.addModule.mockImplementation(() => module.promise);
    permission.resolve(media());
    await vi.advanceTimersByTimeAsync(5000);
    await expect(handle.ready).rejects.toMatchObject({ code: "device" });
    module.resolve();
    released();
  });

  it("bounds autoplay resume that never settles and releases a late permission grant", async () => {
    vi.useFakeTimers();
    resumePromise = new Promise(() => {});
    const handle = start();
    await vi.advanceTimersByTimeAsync(5000);
    await expect(handle.ready).rejects.toMatchObject({ code: "suspended" });
    permission.resolve(media());
    await Promise.resolve();
    released();
  });

  it("bounds pending permission even after resume succeeds with a fixed device error", async () => {
    vi.useFakeTimers();
    const handle = start();
    await vi.advanceTimersByTimeAsync(4999);
    expect(contexts[0].close).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    await expect(handle.ready).rejects.toMatchObject({ code: "device", message: "Microphone capture: device" });
    permission.resolve(media());
    await Promise.resolve();
    await Promise.resolve();
    expect(tracks[0].stop).toHaveBeenCalledTimes(1);
    expect(nodes).toHaveLength(0);
    released();
  });

  it("cleans up permission granted before a rejected resume", async () => {
    const resume = deferred<void>();
    resumePromise = resume.promise;
    const handle = start();
    permission.resolve(media());
    await Promise.resolve();
    resume.reject(new Error("autoplay"));
    await expect(handle.ready).rejects.toMatchObject({ code: "suspended" });
    released();
  });

  it("rejects suspended startup and releases granted tracks", async () => {
    const handle = start();
    contexts[0].state = "suspended";
    permission.resolve(media());
    await expect(handle.ready).rejects.toMatchObject({ code: "suspended" });
    released();
  });

  it.each([true, false])("fails suspension during module loading with state event=%s", async emitStateChange => {
    const module = deferred<void>();
    const loading = deferred<void>();
    const status = vi.fn();
    const handle = start({ onStatus: status });
    const context = contexts[0];
    context.audioWorklet.addModule.mockImplementation(() => {
      loading.resolve();
      return module.promise;
    });
    permission.resolve(media());
    await loading.promise;
    const lateStateChange = context.onstatechange!;
    expect(lateStateChange).toBeTypeOf("function");
    context.state = "suspended";
    if (emitStateChange) {
      lateStateChange();
      await expect(handle.ready).rejects.toMatchObject({ code: "suspended" });
      expect(context.onstatechange).toBeNull();
      released();
    }
    module.resolve();
    await expect(handle.ready).rejects.toMatchObject({ code: "suspended" });
    await Promise.resolve();
    lateStateChange();
    expect(nodes).toHaveLength(0);
    expect(context.createMediaStreamSource).not.toHaveBeenCalled();
    expect(status.mock.calls.map(([value]) => value)).toEqual(["starting", "error"]);
    await expect(handle.stop()).rejects.toMatchObject({ code: "suspended" });
    released();
  });

  it("cancels while addModule is pending, with no late graph creation", async () => {
    const module = deferred<void>();
    const handle = start();
    contexts[0].audioWorklet.addModule.mockImplementation(() => module.promise);
    permission.resolve(media());
    await Promise.resolve();
    await Promise.resolve();
    handle.cancel();
    module.resolve();
    await expect(handle.ready).rejects.toMatchObject({ code: "cancelled" });
    await Promise.resolve();
    expect(nodes).toHaveLength(0);
    released();
  });
});
