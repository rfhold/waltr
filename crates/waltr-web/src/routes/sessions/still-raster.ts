import type { MessageImageFormatView } from "../../generated/waltr-client";
import { MAX_IMAGE_BYTES } from "../../platform/message-images";

// Bound browser decoding to 16,777,216 pixels (64 MiB RGBA) and 8,192 per axis.
const MAX_PIXELS = 16 * 1024 * 1024;
const MAX_AXIS = 8192;

function bounded(width: number, height: number) {
  return width > 0 && height > 0 && width <= MAX_AXIS && height <= MAX_AXIS && width * height <= MAX_PIXELS;
}

export function stillRaster(bytes: Uint8Array): MessageImageFormatView | undefined {
  if (bytes.length < 12 || bytes.length > MAX_IMAGE_BYTES) return;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const signature = (offset: number, expected: number[]) => expected.every((byte, i) => bytes[offset + i] === byte);
  const tag = (offset: number) => String.fromCharCode(bytes[offset]!, bytes[offset + 1]!, bytes[offset + 2]!, bytes[offset + 3]!);
  const u24 = (offset: number) => bytes[offset]! | bytes[offset + 1]! << 8 | bytes[offset + 2]! << 16;

  if (signature(0, [137, 80, 78, 71, 13, 10, 26, 10])) {
    let data = false;
    for (let offset = 8; offset + 12 <= bytes.length;) {
      const size = view.getUint32(offset);
      const end = offset + 12 + size;
      const kind = tag(offset + 4);
      if (end > bytes.length || kind === "acTL" || kind === "fcTL" || kind === "fdAT") return;
      if (offset === 8) {
        if (kind !== "IHDR" || size !== 13 || !bounded(view.getUint32(offset + 8), view.getUint32(offset + 12))) return;
      } else if (kind === "IHDR") return;
      if (kind === "IDAT" && size > 0) data = true;
      if (kind === "IEND") return size === 0 && end === bytes.length && data ? "png" : undefined;
      offset = end;
    }
    return;
  }

  if (signature(0, [255, 216])) {
    let frame = false;
    let scan = false;
    let entropy = false;
    for (let offset = 2; offset < bytes.length;) {
      if (bytes[offset++] !== 255) {
        if (entropy) continue;
        return;
      }
      while (bytes[offset] === 255) offset++;
      const marker = bytes[offset++];
      if (marker === undefined) return;
      if (entropy && (marker === 0 || (marker >= 0xd0 && marker <= 0xd7))) continue;
      entropy = false;
      if (marker === 0xd9) return frame && scan && offset === bytes.length ? "jpeg" : undefined;
      if (marker === 0 || marker === 1 || (marker >= 0xd0 && marker <= 0xd8) || marker === 0xdc || offset + 2 > bytes.length) return;
      const size = view.getUint16(offset);
      if (size < 2 || offset + size > bytes.length) return;
      if (marker >= 0xc0 && marker <= 0xcf && marker !== 0xc4 && marker !== 0xc8 && marker !== 0xcc) {
        if (frame || (marker !== 0xc0 && marker !== 0xc1 && marker !== 0xc2) || size < 11
          || size !== 8 + 3 * bytes[offset + 7]!
          || !bounded(view.getUint16(offset + 5), view.getUint16(offset + 3))) return;
        frame = true;
      }
      if (marker === 0xda) {
        if (!frame || size < 8 || size !== 6 + 2 * bytes[offset + 2]!) return;
        scan = entropy = true;
      }
      offset += size;
    }
    return;
  }

  if (tag(0) === "RIFF" && tag(8) === "WEBP" && view.getUint32(4, true) === bytes.length - 8) {
    let canvas: [number, number] | undefined;
    let frame: [number, number] | undefined;
    let offset = 12;
    while (offset + 8 <= bytes.length) {
      const kind = tag(offset);
      const size = view.getUint32(offset + 4, true);
      const data = offset + 8;
      const end = data + size + (size & 1);
      if (end > bytes.length || kind === "ANIM" || kind === "ANMF") return;
      if (kind === "VP8X") {
        if (offset !== 12 || size !== 10 || (bytes[data]! & 2) !== 0) return;
        canvas = [1 + u24(data + 4), 1 + u24(data + 7)];
        if (!bounded(...canvas)) return;
      } else if (kind === "VP8 ") {
        if (frame || size < 10 || (bytes[data]! & 1) !== 0 || !signature(data + 3, [157, 1, 42])) return;
        frame = [view.getUint16(data + 6, true) & 0x3fff, view.getUint16(data + 8, true) & 0x3fff];
      } else if (kind === "VP8L") {
        if (frame || size < 5 || bytes[data] !== 0x2f) return;
        const bits = view.getUint32(data + 1, true);
        if (bits >>> 29) return;
        frame = [1 + (bits & 0x3fff), 1 + ((bits >>> 14) & 0x3fff)];
      }
      offset = end;
    }
    if (offset === bytes.length && frame && bounded(...frame) && (!canvas || (frame[0] === canvas[0] && frame[1] === canvas[1]))) return "webp";
  }
}
