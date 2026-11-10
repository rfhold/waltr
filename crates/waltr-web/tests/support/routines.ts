import { vi } from "vitest";
import type { ClientManualRoutineDefinition, ClientRoutineInvocationSummary } from "../../src/generated/waltr-client";
import type { MediaBridge } from "../../src/platform/media";
import { snapshotWithConfig } from "./snapshots";

export function routineDefinition(overrides: Partial<ClientManualRoutineDefinition> = {}): ClientManualRoutineDefinition {
  const schema = { type: "object" as const, properties: { direction: { type: "nullable" as const, schema: { type: "string" as const, maxLength: 100 } }, options: { type: "array" as const, maxItems: 4, items: { type: "boolean" as const } } }, required: ["direction"] };
  return { routineName: "fork", description: "Create a branch.", routineRevision: "revision/9007199254740993", apiVersion: "waltr.routine.v2", engine: "javascript", entrypoints: [{ name: "manual", trigger: "manual", selector: null, startContexts: ["global", "session"] }], parameters: { schemaVersion: "1", schema, schemaJson: JSON.stringify(schema) }, result: { schemaVersion: "1", schema: { type: "null" }, schemaJson: '{"type":"null"}' }, ports: [], capabilities: [], budgets: { valueBytes: 65536, resultBytes: 65536, hostCalls: 100, inFlight: 4, deadlineMs: "60000" }, ...overrides };
}
export function routineSnapshot(definition = routineDefinition()) {
  const snapshot = snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, inferenceDefaults: null });
  snapshot.routines.definitions.definitions = [definition]; snapshot.routines.definitions.digest = "digest"; snapshot.routines.definitions.watch.status = "received";
  return snapshot;
}
export function routineSummary(id = "invocation/opaque", overrides: Partial<ClientRoutineInvocationSummary> = {}): ClientRoutineInvocationSummary {
  return { version: 2, admissionSequence: "9007199254740993", invocationId: id, routineName: "fork", routineRevision: "revision/9007199254740993", triggerName: "manual", triggerKind: "manual", ownership: "detached", correlationId: "correlation", snapshot: null, sourceSession: "source", status: "running", admittedAtMs: 1700000000000n, updatedAtMs: 1700000000000n, result: null, textResults: [], binaryResults: [], helpers: [], observationRevision: "9007199254740995", ...overrides };
}
export function mediaBridge(): MediaBridge & { [K in keyof Omit<MediaBridge, "nativePlayback" | "nativeFiles">]: ReturnType<typeof vi.fn<Omit<MediaBridge, "nativePlayback" | "nativeFiles">[K]>> } {
  return {
    openMediaInteraction: vi.fn(async () => 7), openMediaPcmStream: vi.fn(async () => {}), pushMediaPcm: vi.fn(async () => {}), openMediaTextStream: vi.fn(async () => {}), pushMediaTextRevision: vi.fn(async () => {}), closeMediaStream: vi.fn(async () => {}), finishMediaInput: vi.fn(async () => {}),
    mediaInteractionStatusJson: vi.fn(() => JSON.stringify({ operationId: 7, phase: "live", preview: "hypothesis", finalText: null, cleanupVerified: false })), takeMediaOutputJson: vi.fn(() => undefined), takeMediaOutputBytes: vi.fn(() => new Uint8Array([1, 2])),
    beginMediaUpload: vi.fn(async () => {}), appendMediaUpload: vi.fn(async () => {}), commitMediaUpload: vi.fn(async () => "{}"), beginMediaDownload: vi.fn(async () => "{}"), readMediaDownloadChunk: vi.fn(async () => undefined), closeMediaDownload: vi.fn(async () => {}), beginRoutineTextResult: vi.fn(() => 9), readRoutineTextResultChunk: vi.fn(async () => undefined), closeRoutineTextResult: vi.fn(async () => {}), cancelMediaInteraction: vi.fn(async () => true),
  };
}
