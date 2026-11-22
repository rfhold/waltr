import { expect, test } from "vitest";
import { stillRaster } from "../../src/routes/sessions/still-raster";

const ascii = (text: string) => Array.from(text, char => char.charCodeAt(0));
const u32 = (value: number) => [value & 255, (value >>> 8) & 255, (value >>> 16) & 255, value >>> 24];
const chunk = (name: string, data: number[]) => [...ascii(name), ...u32(data.length), ...data, ...(data.length % 2 ? [0] : [])];
const riff = (...chunks: number[][]) => {
  const body = [...ascii("WEBP"), ...chunks.flat()];
  return new Uint8Array([...ascii("RIFF"), ...u32(body.length), ...body]);
};
const lossless = (width: number, height: number) => chunk("VP8L", [0x2f, ...u32(width - 1 | (height - 1) << 14)]);

test("bounds WebP frame and canvas dimensions and rejects animation or duplicate frames", () => {
  const vp8x = (flags: number, width: number, height: number) =>
    chunk("VP8X", [flags, 0, 0, 0, ...u32(width - 1).slice(0, 3), ...u32(height - 1).slice(0, 3)]);
  expect(stillRaster(riff(lossless(4096, 4096)))).toBe("webp");
  expect(stillRaster(riff(lossless(4097, 4096)))).toBeUndefined();
  expect(stillRaster(riff(vp8x(0, 1, 1), lossless(1, 1)))).toBe("webp");
  expect(stillRaster(riff(vp8x(0, 2, 1), lossless(1, 1)))).toBeUndefined();
  expect(stillRaster(riff(vp8x(2, 1, 1), lossless(1, 1)))).toBeUndefined();
  for (const kind of ["ANIM", "ANMF"]) {
    expect(stillRaster(riff(lossless(1, 1), chunk(kind, [])))).toBeUndefined();
  }
  expect(stillRaster(riff(lossless(1, 1), lossless(1, 1)))).toBeUndefined();
  expect(stillRaster(riff(lossless(1, 1)).subarray(0, -1))).toBeUndefined();
});

function jpeg(width: number, height: number, footer = [0xff, 0xd9]) {
  return new Uint8Array([
    0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, height >> 8, height & 255, width >> 8, width & 255, 1, 1, 0x11, 0,
    0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 42, 0xff, 0, 1, ...footer,
  ]);
}

test("checks JPEG SOF dimensions and full still framing including stuffed entropy", () => {
  expect(stillRaster(jpeg(4096, 4096))).toBe("jpeg");
  expect(stillRaster(jpeg(4096, 4097))).toBeUndefined();
  expect(stillRaster(jpeg(0, 1))).toBeUndefined();
  expect(stillRaster(jpeg(1, 1, []))).toBeUndefined();
  expect(stillRaster(jpeg(1, 1, [0xff, 0xdc, 0, 4, 0, 1, 0xff, 0xd9]))).toBeUndefined();
  expect(stillRaster(jpeg(1, 1, [0xff, 0xd9, ...jpeg(1, 1)]))).toBeUndefined();
});
