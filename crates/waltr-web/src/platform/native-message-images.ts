import type { invoke as TauriInvoke } from "@tauri-apps/api/core";
import {
  MAX_IMAGE_BYTES,
  MAX_MESSAGE_IMAGES,
  MAX_MESSAGE_IMAGE_BYTES,
  MAX_MESSAGE_TEXT_BYTES,
  type MessageImagePart,
  type MessageImagesBridge,
} from "./message-images";

const tags = { png: 1, jpeg: 2, webp: 3 } as const;

function packet(parts: MessageImagePart[]): Uint8Array<ArrayBuffer> {
  if (!parts.length || parts.length > 32) throw new Error("Invalid message parts.");
  const encoder = new TextEncoder();
  let imageCount = 0;
  let imageBytes = 0;
  let textBytes = 0;
  const frames = parts.map(part => {
    if (part.kind === "text") {
      if (part.text.length > MAX_MESSAGE_TEXT_BYTES) throw new Error("Message text exceeds 16 KiB.");
      const bytes = encoder.encode(part.text);
      textBytes += bytes.length;
      if (textBytes > MAX_MESSAGE_TEXT_BYTES) throw new Error("Message text exceeds 16 KiB.");
      return { tag: 0, bytes };
    }
    imageCount++;
    imageBytes += part.bytes.length;
    if (!part.bytes.length || part.bytes.length > MAX_IMAGE_BYTES
      || imageCount > MAX_MESSAGE_IMAGES || imageBytes > MAX_MESSAGE_IMAGE_BYTES) {
      throw new Error("Message image size or count limit exceeded.");
    }
    return { tag: tags[part.format], bytes: part.bytes };
  });
  const body = new Uint8Array(1 + 5 * frames.length + imageBytes + textBytes);
  const view = new DataView(body.buffer);
  body[0] = frames.length;
  let offset = 1;
  for (const frame of frames) {
    view.setUint8(offset, frame.tag);
    view.setUint32(offset + 1, frame.bytes.length, true);
    body.set(frame.bytes, offset + 5);
    offset += 5 + frame.bytes.length;
  }
  return body;
}

export function nativeMessageImages(
  invoke: typeof TauriInvoke,
  generation: number,
  retired: AbortSignal,
  nextOperationId: () => number,
): MessageImagesBridge {
  let sending: number | undefined;
  let releasing: Promise<void> | undefined;
  const current = () => {
    if (retired.aborted) throw new Error("The native session route was replaced.");
  };
  return {
    retired,
    async send(session, parts) {
      current();
      if (sending !== undefined) throw new Error("A chat image send is already in progress.");
      const body = packet(parts);
      const operationId = nextOperationId();
      sending = operationId;
      try {
        if (releasing) await releasing;
        current();
        await invoke("send_chat_images", body, { headers: {
          "x-waltr-generation": String(generation),
          "x-waltr-session": encodeURIComponent(session),
          "x-waltr-operation-id": String(operationId),
        } });
        current();
      } finally {
        sending = undefined;
      }
    },
    async read(sessionId, itemId, partIndex, format, sizeBytes) {
      current();
      if (releasing) await releasing;
      current();
      const raw = await invoke<ArrayBuffer>("read_chat_image", { generation, sessionId, itemId, partIndex, format, sizeBytes });
      current();
      if (!(raw instanceof ArrayBuffer) || raw.byteLength !== sizeBytes || raw.byteLength > MAX_IMAGE_BYTES) {
        throw new Error("Invalid message image response.");
      }
      return new Uint8Array(raw);
    },
    async cancelSend() {
      const operationId = sending;
      if (operationId === undefined) return;
      current();
      if (await invoke<boolean>("cancel_chat_images", { generation, sendOnly: true, operationId }) !== true) {
        throw new Error("Upload cancellation could not be confirmed.");
      }
    },
    async cancelAll() {
      if (retired.aborted) return;
      const operationId = sending ?? nextOperationId();
      const previous = releasing;
      const release = (async () => {
        if (previous) await previous.catch(() => {});
        await invoke("cancel_chat_images", { generation, sendOnly: false, operationId });
      })();
      releasing = release;
      try { await release; } finally { if (releasing === release) releasing = undefined; }
    },
  };
}
