import { afterEach, expect, test, vi } from "vitest";
import { PcmPlayback } from "../../src/audio/playback";

afterEach(() => { vi.unstubAllGlobals(); vi.useRealTimers(); });
function context() {
  const samples: Float32Array[] = [], nodes: any[] = [];
  const close = vi.fn(async () => {});
  const createBuffer = vi.fn((_channels: number, frames: number, rate: number) => ({ duration: frames / rate, getChannelData: () => { const data = new Float32Array(frames); samples.push(data); return data; } }));
  let instance!: Context;
  class Context {
    constructor() { instance = this; }
    state = "running"; currentTime = 0; destination = {}; close = close; createBuffer = createBuffer;
    resume = vi.fn(async () => {});
    createBufferSource() { const node = { onended: null as (() => void) | null, connect: vi.fn(), disconnect: vi.fn(), start: vi.fn(), stop: vi.fn(), buffer: undefined }; nodes.push(node); return node; }
  }
  vi.stubGlobal("AudioContext", Context); return { samples, nodes, close, createBuffer, clock: (time: number) => { instance.currentTime = time; } };
}
test("PCM uses its actual channel/rate and little-endian samples, then joins playback", async () => {
  const fake = context(), playback = new PcmPlayback();
  playback.open("s", { type: "pcm16le", channels: 2, sampleRate: 16000 }); playback.push("s", new Uint8Array([0, 128, 0, 64]));
  expect(fake.createBuffer).toHaveBeenCalledWith(2, 1, 16000); expect(fake.samples.map(data => data[0])).toEqual([-1, 0.5]);
  playback.close("s"); fake.nodes[0].onended(); await playback.finish(); expect(fake.close).toHaveBeenCalledOnce();
});
test("priming is idle, but the last node ending on an open active stream fails without a status poll", async () => {
  const fake = context(), failed = vi.fn(), playback = new PcmPlayback(failed);
  playback.open("s", { type: "pcm16le", channels: 1, sampleRate: 24000 });
  fake.clock(10); playback.check(); expect(failed).not.toHaveBeenCalled();
  playback.push("s", new Uint8Array(4800));
  fake.clock(10.1); fake.nodes[0].onended();
  expect(failed).toHaveBeenCalledOnce(); expect(() => playback.push("s", new Uint8Array(2))).toThrow("underrun");
  await playback.dispose(); expect(fake.close).toHaveBeenCalledOnce();
});
test("a delayed next chunk detects starvation even when the ended callback has not run", async () => {
  const fake = context(), failed = vi.fn(), playback = new PcmPlayback(failed);
  playback.open("s", { type: "pcm16le", channels: 1, sampleRate: 24000 }); playback.push("s", new Uint8Array(4800));
  fake.clock(0.2);
  expect(() => playback.push("s", new Uint8Array(2))).toThrow("underrun"); expect(failed).toHaveBeenCalledOnce();
  await playback.dispose();
});
test("explicit close allows the final scheduled tail and finish joins context closure", async () => {
  vi.useFakeTimers();
  const fake = context(), failed = vi.fn(), playback = new PcmPlayback(failed);
  let closed!: () => void; fake.close.mockImplementationOnce(() => new Promise<void>(resolve => { closed = resolve; }));
  playback.open("s", { type: "pcm16le", channels: 1, sampleRate: 24000 }); playback.push("s", new Uint8Array(4800)); playback.close("s");
  let done = false; const finishing = playback.finish().then(() => { done = true; });
  await vi.advanceTimersByTimeAsync(20); expect(done).toBe(false); expect(fake.close).not.toHaveBeenCalled();
  fake.nodes[0].onended(); await vi.advanceTimersByTimeAsync(10);
  expect(done).toBe(false); closed(); await finishing; expect(failed).not.toHaveBeenCalled();
});
test("intentional drop never reports an underrun", async () => {
  const fake = context(), failed = vi.fn(), playback = new PcmPlayback(failed);
  playback.open("s", { type: "pcm16le", channels: 1, sampleRate: 24000 }); playback.push("s", new Uint8Array(4800));
  await playback.dispose(); expect(fake.nodes[0].onended).toBeNull(); expect(failed).not.toHaveBeenCalled();
});
test("queue is bounded at 256 KiB; dropping output stops and disconnects every source", async () => {
  const fake = context(), playback = new PcmPlayback(); playback.open("s", { type: "pcm16le", channels: 1, sampleRate: 24000 });
  for (let i = 0; i < 4; i++) playback.push("s", new Uint8Array(65536));
  expect(() => playback.push("s", new Uint8Array(2))).toThrow("queue"); await playback.dispose();
  for (const node of fake.nodes) { expect(node.stop).toHaveBeenCalledOnce(); expect(node.disconnect).toHaveBeenCalledOnce(); }
  expect(fake.close).toHaveBeenCalledOnce(); expect(() => playback.push("s", new Uint8Array(2))).toThrow("Invalid");
});
