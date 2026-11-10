import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { startLiveCapture } from "../../src/audio/live";

let resolvePermission!: (stream: MediaStream) => void;
let resolveModule!: () => void;
let node: FakeNode;
let context: FakeContext;
const track = { readyState: "live", stop: vi.fn(), onended: null as (() => void) | null };
const stream = { getTracks: () => [track] } as unknown as MediaStream;
class FakeContext {
  state = "running"; destination = {}; onstatechange: (() => void) | null = null;
  audioWorklet = { addModule: vi.fn(() => new Promise<void>(yes => { resolveModule = yes; })) };
  resume = vi.fn(async () => {}); close = vi.fn(async () => {});
  source = { connect: vi.fn(), disconnect: vi.fn() }; createMediaStreamSource = () => this.source;
  constructor() { context = this; }
}
class FakeNode {
  onprocessorerror = null; port = { onmessage: null as ((e: { data: unknown }) => void) | null, postMessage: vi.fn(), close: vi.fn() }; connect = vi.fn(); disconnect = vi.fn();
  constructor() { node = this; }
  emit(data: unknown) { this.port.onmessage?.({ data }); }
}
const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
beforeEach(() => {
  vi.useFakeTimers(); track.stop.mockClear();
  vi.stubGlobal("isSecureContext", true); vi.stubGlobal("AudioContext", FakeContext); vi.stubGlobal("AudioWorkletNode", FakeNode);
  vi.stubGlobal("navigator", { mediaDevices: { getUserMedia: vi.fn(() => new Promise<MediaStream>(yes => { resolvePermission = yes; })) } });
});
afterEach(() => { vi.unstubAllGlobals(); vi.useRealTimers(); });
test("one worklet credit waits for send acknowledgment; normal stop drains tail and joins devices", async () => {
  let acknowledge!: () => void;
  const push = vi.fn(() => new Promise<void>(yes => { acknowledge = yes; })); const capture = startLiveCapture(push, vi.fn());
  expect(context.resume).toHaveBeenCalledOnce(); expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledOnce();
  resolvePermission(stream); await settle(); resolveModule(); await capture.ready;
  node.emit({ type: "pcm", bytes: new Uint8Array([1, 2]) });
  expect(push).toHaveBeenCalledOnce(); expect(node.port.postMessage).not.toHaveBeenCalledWith({ type: "ack" });
  acknowledge(); await settle(); expect(node.port.postMessage).toHaveBeenCalledWith({ type: "ack" });
  const stopped = capture.stop(); await settle(); node.emit({ type: "stopped" }); await stopped;
  expect(track.stop).toHaveBeenCalledOnce(); expect(context.close).toHaveBeenCalledOnce(); expect(node.port.close).toHaveBeenCalledOnce();
});
test("release before startup completes normally drains after permission, rather than unqualified cancellation", async () => {
  const capture = startLiveCapture(vi.fn(async () => {}), vi.fn()); const stopped = capture.stop();
  resolvePermission(stream); await settle(); resolveModule(); await capture.ready; await settle();
  expect(node.port.postMessage).toHaveBeenCalledWith({ type: "stop" }); node.emit({ type: "stopped" }); await stopped;
  expect(track.stop).toHaveBeenCalledOnce();
});
test("acquisition timeout and explicit cancel stop a late permission result", async () => {
  const failed = vi.fn(); const capture = startLiveCapture(vi.fn(async () => {}), failed);
  await vi.advanceTimersByTimeAsync(5000); await expect(capture.ready).rejects.toThrow(); expect(failed).toHaveBeenCalledOnce();
  resolvePermission(stream); await settle(); expect(track.stop).toHaveBeenCalledOnce(); expect(context.close).toHaveBeenCalledOnce();
});
test("permission tracks have an owner even while AudioContext resume is stalled", async () => {
  class SuspendedContext extends FakeContext { resume = vi.fn(() => new Promise<void>(() => {})); }
  vi.stubGlobal("AudioContext", SuspendedContext);
  const capture = startLiveCapture(vi.fn(async () => {}), vi.fn()); resolvePermission(stream); await settle();
  await vi.advanceTimersByTimeAsync(5000); await expect(capture.ready).rejects.toThrow();
  expect(track.stop).toHaveBeenCalledOnce(); expect(context.close).toHaveBeenCalledOnce();
});
