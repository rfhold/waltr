import { expect, test, vi } from "vitest";
import type { invoke as Invoke } from "@tauri-apps/api/core";
import { nativeMedia } from "../../src/platform/native-media";

test("native media metadata is generation-qualified; typed IPC never embeds PCM in JSON", async () => {
  const invoke = vi.fn(async (command: string) => command === "routine_media_bytes" ? new Uint8Array([1, 2]).buffer : { generation: 12, value: 7 });
  const bridge = nativeMedia(invoke as typeof Invoke, 12, () => true);
  await bridge.openMediaInteraction('{"routine":"public"}');
  expect(invoke).toHaveBeenCalledWith("routine_media", { generation: 12, operation: "open", metadata: '{"routine":"public"}' });
  const input = new Uint8Array([1, 2]); await bridge.appendMediaUpload(7, "input/file", input);
  expect(invoke).toHaveBeenCalledWith("routine_media_append", input, { headers: { "x-waltr-generation": "12", "x-waltr-operation": "upload", "x-waltr-owner": "7", "x-waltr-port": "input%2Ffile" } });
  expect(await bridge.readMediaDownloadChunk(7, "output")).toEqual(input);
  expect(JSON.stringify(invoke.mock.calls.filter(call => call[0] === "routine_media"))).not.toMatch(/endpoint|bearer|secret|pcm|base64|path/i);
});
test("native media rejects stale generation before dispatch and after late completion", async () => {
  let current = true, resolve!: (value: unknown) => void;
  const invoke = vi.fn(() => new Promise(yes => { resolve = yes; }));
  const bridge = nativeMedia(invoke as typeof Invoke, 12, () => current);
  const pending = bridge.mediaInteractionStatusJson(7); current = false; resolve({ generation: 12, value: "{}" });
  await expect(pending).rejects.toThrow("Stale");
  await expect(bridge.cancelMediaInteraction(7)).rejects.toThrow("Stale"); expect(invoke).toHaveBeenCalledOnce();
});
test("native output must use raw ArrayBuffer chunks with the same 64 KiB bound", async () => {
  const invoke = vi.fn(async () => [1, 2]); const bridge = nativeMedia(invoke as typeof Invoke, 1, () => true);
  await expect(bridge.readMediaDownloadChunk(7, "output")).rejects.toThrow("Invalid native byte");
  await expect(bridge.appendMediaUpload(7, "input", new Uint8Array(65537))).rejects.toThrow("limit"); expect(invoke).toHaveBeenCalledOnce();
});
test("native PCM stays in Rust; playback drop is an opaque generation-qualified operation", async () => {
  const invoke = vi.fn(async () => ({ generation: 4, value: true }));
  const bridge = nativeMedia(invoke as typeof Invoke, 4, () => true);
  await expect(bridge.pushMediaPcm(7, "stream", 0, new Uint8Array([1, 2]))).rejects.toThrow("never crosses IPC");
  await expect(bridge.takeMediaOutputBytes(7, 1)).rejects.toThrow("belongs to Rust");
  await expect(bridge.openMediaPcmStream(7, "audio", "stream", "{}")).rejects.toThrow("belongs to Rust");
  expect(invoke).not.toHaveBeenCalled();
  await bridge.nativePlayback!.drop(7);
  expect(invoke).toHaveBeenCalledExactlyOnceWith("routine_media", { generation: 4, operation: "dropPlayback", id: 7 });
});
test("native file capabilities use the exact root picker/save/opaque binding contract", async () => {
  const invoke = vi.fn(async () => ({ generation: 8, value: null }));
  const files = nativeMedia(invoke as typeof Invoke, 8, () => true).nativeFiles!;
  const input = { port: "image", kind: "image" as const, contentTypes: ["image/png", "image/jpeg", "image/webp"], maxBytes: 20971520 };
  await expect(files.pickInput(input)).resolves.toBeUndefined();
  expect(invoke).toHaveBeenLastCalledWith("routine_media", { generation: 8, operation: "pickInput", request: input });
  await files.releaseInput("input-handle"); expect(invoke).toHaveBeenLastCalledWith("routine_media", { generation: 8, operation: "releaseInput", handle: "input-handle" });
  await files.pickOutput({ port: "out", suggestedName: "result.pdf" }); expect(invoke).toHaveBeenLastCalledWith("routine_media", { generation: 8, operation: "pickOutput", request: { port: "out", suggestedName: "result.pdf" } });
  await files.releaseOutput("output-handle"); expect(invoke).toHaveBeenLastCalledWith("routine_media", { generation: 8, operation: "releaseOutput", handle: "output-handle" });
  await files.open("{}", [{ port: "image", handle: "input-handle" }], [{ port: "speech", kind: "playback" }, { port: "out", kind: "file", handle: "output-handle" }]);
  expect(invoke).toHaveBeenLastCalledWith("routine_media", { generation: 8, operation: "openNative", metadata: "{}", inputs: [{ port: "image", handle: "input-handle" }], outputs: [{ port: "speech", kind: "playback" }, { port: "out", kind: "file", handle: "output-handle" }] });
});
test("Rust nullable EOF does not become text or output metadata; false cleanup is not success", async () => {
  const invoke = vi.fn(async (_command: string, args: unknown) => ({ generation: 9, value: (args as { operation: string }).operation === "dropPlayback" ? false : null }));
  const bridge = nativeMedia(invoke as typeof Invoke, 9, () => true);
  await expect(bridge.readRoutineTextResultChunk(7)).resolves.toBeUndefined();
  await expect(bridge.takeMediaOutputJson(7)).resolves.toBeUndefined();
  await expect(bridge.nativeFiles!.pickOutput({ port: "out", suggestedName: "result" })).resolves.toBeUndefined();
  await expect(bridge.nativePlayback!.drop(7)).rejects.toThrow("cleanup unverified");
});
