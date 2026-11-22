import type { invoke as TauriInvoke } from "@tauri-apps/api/core";
import { expect, test, vi } from "vitest";
import { nativeMessageImages } from "../../src/platform/native-message-images";

const allocator = () => {
  let id = 0;
  return () => ++id;
};

test("native chat images carry bytes only in bounded raw IPC, with exact read coordinates", async () => {
  const ipc = vi.fn().mockResolvedValueOnce(undefined).mockResolvedValueOnce(new Uint8Array([0, 255]).buffer);
  const bridge = nativeMessageImages(ipc as typeof TauriInvoke, 12, new AbortController().signal, allocator());
  await bridge.send("session/α", [
    { kind: "text", text: "x" }, { kind: "image", format: "jpeg", bytes: new Uint8Array([0, 255]) },
  ]);
  expect(ipc).toHaveBeenNthCalledWith(1, "send_chat_images", new Uint8Array([
    2, 0, 1, 0, 0, 0, 120, 2, 2, 0, 0, 0, 0, 255,
  ]), { headers: { "x-waltr-generation": "12", "x-waltr-session": "session%2F%CE%B1", "x-waltr-operation-id": "1" } });
  expect(await bridge.read("session/α", "item", 1, "jpeg", 2)).toEqual(new Uint8Array([0, 255]));
  expect(ipc).toHaveBeenNthCalledWith(2, "read_chat_image", {
    generation: 12, sessionId: "session/α", itemId: "item", partIndex: 1, format: "jpeg", sizeBytes: 2,
  });
  await bridge.cancelSend();
  expect(ipc).toHaveBeenCalledTimes(2);
});

test("native IPC rejects an old-generation byte completion and disallows further requests", async () => {
  let finish!: (bytes: ArrayBuffer) => void;
  const ipc = vi.fn(() => new Promise(resolve => { finish = resolve; }));
  const authority = new AbortController();
  const bridge = nativeMessageImages(ipc as typeof TauriInvoke, 12, authority.signal, allocator());
  const pending = bridge.read("session", "item", 0, "png", 1);
  authority.abort();
  finish(new Uint8Array([20]).buffer);
  await expect(pending).rejects.toThrow("native session route was replaced");
  await expect(bridge.send("session", [{ kind: "text", text: "x" }])).rejects.toThrow("native session route was replaced");
  expect(ipc).toHaveBeenCalledOnce();
});

test.each(["not yet polled", "admitted"])("cancels only the matching native operation when send is %s", async (state) => {
  let send!: (reason: Error) => void;
  const ipc = vi.fn((command: string) => command === "send_chat_images"
    ? new Promise((_resolve, reject) => { send = reject; })
    : Promise.resolve(true));
  const bridge = nativeMessageImages(ipc as typeof TauriInvoke, 12, new AbortController().signal, allocator());
  const pending = bridge.send("session", [{ kind: "text", text: "x" }]);
  if (state === "admitted") await Promise.resolve();
  await bridge.cancelSend();
  expect(ipc).toHaveBeenNthCalledWith(2, "cancel_chat_images", { generation: 12, sendOnly: true, operationId: 1 });
  send(new Error("Chat image send cancelled"));
  await expect(pending).rejects.toThrow("cancelled");
});

test("does not treat a native no-op cancel acknowledgement as verified", async () => {
  let complete!: () => void;
  const ipc = vi.fn((command: string) => command === "send_chat_images"
    ? new Promise<void>(resolve => { complete = resolve; })
    : Promise.resolve(false));
  const bridge = nativeMessageImages(ipc as typeof TauriInvoke, 12, new AbortController().signal, allocator());
  const pending = bridge.send("session", [{ kind: "text", text: "x" }]);
  await expect(bridge.cancelSend()).rejects.toThrow("cancellation could not be confirmed");
  complete();
  await pending;
});
