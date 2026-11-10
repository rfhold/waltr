import type { RoutinePort, RoutineTextResult } from "../generated/waltr-client";
import type { MediaBridge } from "./media";

export const CHUNK_BYTES = 65536;
export const FILE_BYTES = 64 * 1024 * 1024;
export const IMAGE_BYTES = 20 * 1024 * 1024;

export class MediaDownloadCleanupError extends Error {
  constructor() { super("Download reader cleanup could not be verified."); }
}

export function fileDeclaration(port: RoutinePort, file: Pick<File, "name" | "size" | "type">) {
  if (port.direction !== "input" || !["file", "image"].includes(port.media)) throw new Error("Not a file input port.");
  const max = Math.min(port.media === "image" ? IMAGE_BYTES : FILE_BYTES, Number(BigInt(port.maxBytes)));
  if (!Number.isSafeInteger(file.size) || !file.size || file.size > max) throw new Error(`File exceeds the ${max} byte input limit or is empty.`);
  const image = ({ "image/png": "png", "image/jpeg": "jpeg", "image/webp": "webp" } as Record<string, string>)[file.type];
  if (port.media === "image" && !image) throw new Error("Choose a PNG, JPEG, or WebP image. The server validates its actual content.");
  return { format: port.media, port: port.name, displayName: file.name, contentType: file.type || "application/octet-stream", declaredBytes: file.size, expectedSha256: null, ...(port.media === "image" ? { image } : {}) };
}

export async function uploadFile(bridge: MediaBridge, id: number, port: string, file: File, signal: AbortSignal) {
  signal.throwIfAborted();
  await bridge.beginMediaUpload(id, port);
  for (let offset = 0; offset < file.size; offset += CHUNK_BYTES) {
    signal.throwIfAborted();
    const bytes = new Uint8Array(await file.slice(offset, offset + CHUNK_BYTES).arrayBuffer());
    signal.throwIfAborted();
    await bridge.appendMediaUpload(id, port, bytes);
  }
  signal.throwIfAborted();
  // commit validates quarantine and sends BlobInput on this exact declared port.
  return bridge.commitMediaUpload(id, port);
}

export async function downloadBlob(bridge: MediaBridge, id: number, port: string, signal: AbortSignal): Promise<{ blob: Blob; name: string }> {
  signal.throwIfAborted();
  const checkoutAt = performance.now();
  let deadline = checkoutAt + 900000;
  let closure: Promise<void> | undefined;
  const close = () => {
    closure ??= Promise.resolve().then(() => bridge.closeMediaDownload(id, port));
    void closure.catch(() => undefined);
    return closure;
  };
  const abort = () => { void close(); };
  let timer = setTimeout(abort, 900000);
  signal.addEventListener("abort", abort, { once: true });
  let result: { blob: Blob; name: string };
  try {
    const descriptor = JSON.parse(await bridge.beginMediaDownload(id, port)) as { bytes: string; displayName: string; contentType: string; remainingValidityMs?: string | null };
    signal.throwIfAborted();
    const count = BigInt(descriptor.bytes);
    if (count < 0n || count > BigInt(FILE_BYTES)) throw new Error("This browser supports explicit Blob saves only up to 64 MiB.");
    const ttl = descriptor.remainingValidityMs;
    if (typeof ttl !== "string" || !/^\d+$/.test(ttl) || !Number.isSafeInteger(Number(ttl)) || Number(ttl) > 900000) throw new Error("Invalid nonrenewable download grant lifetime.");
    deadline = checkoutAt + Number(ttl);
    clearTimeout(timer);
    if (performance.now() >= deadline) throw new Error("The nonrenewable download grant expired.");
    timer = setTimeout(abort, Math.max(0, deadline - performance.now()));
    let size = 0;
    const chunks: BlobPart[] = [];
    for (;;) {
      signal.throwIfAborted();
      if (performance.now() >= deadline) throw new Error("The nonrenewable download grant expired.");
      const bytes = await bridge.readMediaDownloadChunk(id, port);
      signal.throwIfAborted();
      if (performance.now() >= deadline) throw new Error("The nonrenewable download grant expired.");
      if (bytes === undefined) break;
      if (!(bytes instanceof Uint8Array) || !bytes.length || bytes.length > CHUNK_BYTES) throw new Error("Invalid binary download chunk.");
      size += bytes.length;
      if (size > Number(count) || size > FILE_BYTES) throw new Error("Download size exceeded its declaration.");
      chunks.push(new Blob([bytes as Uint8Array<ArrayBuffer>]));
    }
    if (size !== Number(count)) throw new Error("Incomplete download.");
    result = { blob: new Blob(chunks, { type: descriptor.contentType }), name: descriptor.displayName };
  } finally {
    clearTimeout(timer); signal.removeEventListener("abort", abort);
    try { await close(); } catch { throw new MediaDownloadCleanupError(); }
  }
  signal.throwIfAborted();
  if (performance.now() >= deadline) throw new Error("The nonrenewable download grant expired.");
  return result;
}

export class TextResultBudget {
  private bytes = 0;
  reserve(descriptor: RoutineTextResult) {
    const count = BigInt(descriptor.utf8Bytes);
    if (count > 1048576n || count < 0n || this.bytes + Number(count) > 8 * 1048576) throw new Error("Text result limit: 1 MiB each, 8 MiB total.");
    this.bytes += Number(count);
    let released = false;
    return () => { if (!released) { released = true; this.bytes -= Number(count); } };
  }
}

export async function readTextResult(bridge: MediaBridge, invocation: string, descriptor: RoutineTextResult, signal: AbortSignal) {
  const id = await bridge.beginRoutineTextResult(invocation, descriptor.id);
  let text = "";
  let bytes = 0;
  try {
    for (;;) {
      signal.throwIfAborted();
      const chunk = await bridge.readRoutineTextResultChunk(id);
      signal.throwIfAborted();
      if (chunk === undefined) break;
      const size = new TextEncoder().encode(chunk).length;
      if (size > CHUNK_BYTES + 3) throw new Error("Text result chunk exceeded its bound.");
      bytes += size;
      if (bytes > 1048576 || BigInt(bytes) > BigInt(descriptor.utf8Bytes)) throw new Error("Text result exceeded its declaration.");
      text += chunk;
    }
    if (BigInt(bytes) !== BigInt(descriptor.utf8Bytes)) throw new Error("Incomplete text result.");
    return text;
  } finally { await bridge.closeRoutineTextResult(id).catch(() => undefined); }
}
