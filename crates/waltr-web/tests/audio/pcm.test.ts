import { describe, expect, it } from "vitest";
import { BATCH_FRAMES, MAX_PAYLOAD_BYTES, PcmConverter } from "../../src/audio/pcm";

function convert(input: Float32Array, inputRate: number, channels = 1, chunk = BATCH_FRAMES) {
  const converter = new PcmConverter(inputRate);
  for (let offset = 0; offset < input.length; offset += chunk * channels) {
    converter.push(input.subarray(offset, offset + chunk * channels), channels);
  }
  return converter.finish();
}
function shorts(bytes: Uint8Array) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return Array.from({ length: bytes.length / 2 }, (_, index) => view.getInt16(index * 2, true));
}

describe("bounded streaming PCM", () => {
  it("clips, mixes interleaved channels in order and writes headerless little endian", () => {
    const bytes = convert(new Float32Array([-2, -2, 1, 1, 1, -1, 0.5, 0.5]), 24000, 2);
    expect(shorts(bytes)).toEqual([-32768, 32767, 0, 16384]);
    expect([...bytes.slice(0, 4)]).toEqual([0, 128, 255, 127]);
  });

  it.each([44100, 48000, 16000, 24000, 96000])("preserves fractional continuity at %i Hz across arbitrary chunks", rate => {
    const input = Float32Array.from({ length: rate + 17 }, (_, index) => Math.sin(index * 0.031) * 0.7);
    const whole = convert(input, rate);
    expect(whole.length / 2).toBe(Math.ceil(input.length * 24000 / rate));
    expect(convert(input, rate, 1, 1)).toEqual(whole);
    expect(convert(input, rate, 1, 37)).toEqual(whole);
  });

  it("uses output timestamps and holds the last sample on final flush", () => {
    expect(shorts(convert(new Float32Array([0, 1]), 16000))).toEqual([0, 21845, 32767]);
    expect(shorts(convert(new Float32Array([0, 0.5, 1, -1]), 48000))).toEqual([0, 32767]);
  });

  it("bounds duration and payload including final flush", () => {
    const duration = new PcmConverter(24000, 24000, 1 / 24000);
    duration.push(new Float32Array([1]), 1);
    expect(() => duration.push(new Float32Array([1]), 1)).toThrow(/duration/);
    const payload = new PcmConverter(8000, 24000, 1, 2);
    payload.push(new Float32Array([1]), 1);
    expect(() => payload.finish()).toThrow(/overflow/);
    expect(() => new PcmConverter(24000, 24000, 301)).toThrow(RangeError);
    expect(() => new PcmConverter(24000, 24000, 300, MAX_PAYLOAD_BYTES + 1)).toThrow(RangeError);
  });

  it("defines empty, malformed, finite and terminal semantics", () => {
    expect(() => new PcmConverter(24000).finish()).toThrow(/empty/);
    expect(new PcmConverter(24000).finish(true)).toEqual(new Uint8Array());
    for (const [samples, channels] of [[new Float32Array([0]), 2], [new Float32Array([NaN]), 1], [new Float32Array(2049), 1]] as const) {
      expect(() => new PcmConverter(24000).push(samples, channels)).toThrow(/device/);
    }
    const converter = new PcmConverter(24000);
    converter.finish(true);
    expect(() => converter.push(new Float32Array([0]), 1)).toThrow(/device/);
    expect(() => converter.finish()).toThrow(/device/);
  });
});
