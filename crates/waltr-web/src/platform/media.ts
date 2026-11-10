export type MediaStatus = { operationId: number; phase: "opening" | "live" | "succeeded" | "error"; preview: string; finalText: string | null; cleanupVerified: boolean };
export type NativeFileInput = { handle: string; port: string; name: string; contentType: string; bytes: number; purpose: "file" | "image" };
export type NativeFileOutput = { port: string; handle: string };
export type NativeOutputBinding = { port: string; kind: "playback" } | { port: string; kind: "file"; handle: string };
export type NativeFiles = {
  pickInput(request: { port: string; kind: "file" | "image"; contentTypes: string[]; maxBytes: number }): Promise<NativeFileInput | undefined>;
  releaseInput(handle: string): Promise<void>;
  pickOutput(request: { port: string; suggestedName: string }): Promise<NativeFileOutput | undefined>;
  releaseOutput(handle: string): Promise<void>;
  open(metadata: string, inputs: { port: string; handle: string }[], outputs: NativeOutputBinding[]): Promise<number>;
};
/** Metadata only; payloads cross the typed byte methods, never JSON. */
export type MediaBridge = {
  nativePlayback?: { drop(id: number): Promise<void> };
  nativeFiles?: NativeFiles;
  openMediaInteraction(metadata: string): Promise<number>;
  openMediaPcmStream(id: number, port: string, stream: string, format: string): Promise<void>;
  pushMediaPcm(id: number, stream: string, sequence: number, bytes: Uint8Array): Promise<void>;
  openMediaTextStream(id: number, port: string, stream: string): Promise<void>;
  pushMediaTextRevision(id: number, stream: string, sequence: string, revision: string): Promise<void>;
  closeMediaStream(id: number, stream: string, nextSequence: number): Promise<void>;
  finishMediaInput(id: number): Promise<void>;
  mediaInteractionStatusJson(id: number): string | Promise<string>;
  takeMediaOutputJson(id: number): string | undefined | Promise<string | undefined>;
  takeMediaOutputBytes(id: number, chunk: number): Uint8Array | Promise<Uint8Array>;
  beginMediaUpload(id: number, port: string): Promise<void>;
  appendMediaUpload(id: number, port: string, bytes: Uint8Array): Promise<void>;
  commitMediaUpload(id: number, port: string): Promise<string>;
  beginMediaDownload(id: number, port: string): Promise<string>;
  readMediaDownloadChunk(id: number, port: string): Promise<Uint8Array | undefined>;
  closeMediaDownload(id: number, port: string): Promise<void>;
  beginRoutineTextResult(invocation: string, selected: string): number | Promise<number>;
  readRoutineTextResultChunk(id: number): Promise<string | undefined>;
  closeRoutineTextResult(id: number): Promise<void>;
  cancelMediaInteraction(id: number): Promise<boolean>;
};

export async function cancelOwnedMedia(bridge: MediaBridge, id: number): Promise<boolean> {
  try { return await bridge.cancelMediaInteraction(id); }
  catch (error) {
    if (bridge.nativeFiles || bridge.nativePlayback) throw error;
    const status = JSON.parse(await bridge.mediaInteractionStatusJson(id)) as MediaStatus;
    // WASM publishes a terminal phase only after its local owner has joined.
    if (status.phase === "error" || status.phase === "succeeded") return status.cleanupVerified;
    throw error;
  }
}

export async function waitAccepted(bridge: MediaBridge, id: number, cancelled: () => boolean, deadline = performance.now() + 20000) {
  while (!cancelled() && performance.now() < deadline) {
    const status = JSON.parse(await bridge.mediaInteractionStatusJson(id)) as MediaStatus;
    if (status.phase === "live") return;
    if (status.phase === "error" || status.phase === "succeeded") throw new Error("Interaction ended before input acceptance.");
    await new Promise(resolve => setTimeout(resolve, 10));
  }
  throw new Error("Interaction acceptance timed out or was cancelled.");
}
