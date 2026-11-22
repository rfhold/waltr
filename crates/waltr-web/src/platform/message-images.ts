import type { MessageImageFormatView } from "../generated/waltr-client";

export const MAX_IMAGE_BYTES = 8 * 1024 * 1024;
export const MAX_MESSAGE_IMAGE_BYTES = 16 * 1024 * 1024;
export const MAX_MESSAGE_IMAGES = 4;
export const MAX_MESSAGE_TEXT_BYTES = 16 * 1024;

export const imageMime = {
  png: "image/png",
  jpeg: "image/jpeg",
  webp: "image/webp",
} as const satisfies Record<MessageImageFormatView, string>;

export type MessageImagePart =
  | { kind: "text"; text: string }
  | { kind: "image"; format: MessageImageFormatView; bytes: Uint8Array };

export type MessageImagesBridge = {
  retired: AbortSignal;
  send: (session: string, parts: MessageImagePart[]) => Promise<void>;
  read: (session: string, item: string, index: number, format: MessageImageFormatView, size: number) => Promise<Uint8Array>;
  cancelSend: () => Promise<void> | void;
  cancelAll: () => Promise<void> | void;
};
