import type { invoke as TauriInvoke } from "@tauri-apps/api/core";
import type { MediaBridge, NativeFileInput, NativeFileOutput } from "./media";

/** Opt-in IPC contract. Rust advertises routineMedia only after implementing these owners. */
export function nativeMedia(invoke: typeof TauriInvoke, generation: number, current: () => boolean): MediaBridge {
  const stale = () => { if (!current()) throw new Error("Stale native media capability."); };
  async function call<T>(operation: string, args: Record<string, unknown> = {}): Promise<T> {
    stale();
    const response = await invoke<{ generation: number; value: T }>("routine_media", { generation, operation, ...args });
    stale(); if (response.generation !== generation) throw new Error("Stale native media response.");
    return response.value;
  }
  async function bytes(operation: string, args: Record<string, unknown>): Promise<Uint8Array | undefined> {
    stale();
    const result = await invoke<ArrayBuffer>("routine_media_bytes", { generation, operation, ...args });
    stale();
    if (!(result instanceof ArrayBuffer) || result.byteLength > 65536) throw new Error("Invalid native byte chunk.");
    return result.byteLength ? new Uint8Array(result) : undefined;
  }
  async function optional<T>(operation: string, args: Record<string, unknown>): Promise<T | undefined> {
    return (await call<T | null>(operation, args)) ?? undefined;
  }
  async function append(operation: string, id: number, port: string, data: Uint8Array) {
    stale();
    if (!data.length || data.length > 65536) throw new Error("Native chunk limit.");
    await invoke("routine_media_append", data, { headers: {
      "x-waltr-generation": String(generation), "x-waltr-operation": operation,
      "x-waltr-owner": String(id), "x-waltr-port": encodeURIComponent(port),
    } });
    stale();
  }
  return {
    nativeFiles: {
      pickInput: request => optional<NativeFileInput>("pickInput", { request }),
      releaseInput: handle => call("releaseInput", { handle }),
      pickOutput: request => optional<NativeFileOutput>("pickOutput", { request }),
      releaseOutput: handle => call("releaseOutput", { handle }),
      open: (metadata, inputs, outputs) => call("openNative", { metadata, inputs, outputs }),
    },
    nativePlayback: { drop: async id => {
      if (await call<boolean>("dropPlayback", { id }) !== true) throw new Error("Native playback cleanup unverified.");
    } },
    openMediaInteraction: metadata => call("open", { metadata }),
    openMediaPcmStream: async () => { throw new Error("Native microphone capture belongs to Rust dictation."); },
    pushMediaPcm: async () => { throw new Error("Native PCM never crosses IPC."); },
    openMediaTextStream: (id, port, stream) => call("openText", { id, port, stream }),
    pushMediaTextRevision: (id, stream, sequence, revision) => call("text", { id, stream, sequence, revision }),
    closeMediaStream: (id, stream, nextSequence) => call("closeStream", { id, stream, nextSequence }),
    finishMediaInput: id => call("finishInput", { id }),
    mediaInteractionStatusJson: id => call("status", { id }),
    takeMediaOutputJson: id => optional<string>("outputMetadata", { id }),
    takeMediaOutputBytes: async () => { throw new Error("Native playback belongs to Rust."); },
    beginMediaUpload: (id, port) => call("beginUpload", { id, port }),
    appendMediaUpload: (id, port, data) => append("upload", id, port, data),
    commitMediaUpload: (id, port) => call("commitUpload", { id, port }),
    beginMediaDownload: (id, port) => call("beginDownload", { id, port }),
    readMediaDownloadChunk: (id, port) => bytes("download", { id, port }),
    closeMediaDownload: (id, port) => call("closeDownload", { id, port }),
    beginRoutineTextResult: (invocation, selected) => call("beginTextResult", { invocation, selected }),
    readRoutineTextResultChunk: id => optional<string>("readTextResult", { id }),
    closeRoutineTextResult: id => call("closeTextResult", { id }),
    cancelMediaInteraction: id => call("cancel", { id }),
  };
}
