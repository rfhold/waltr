import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type { ClientCommand, ClientSnapshot } from "../generated/waltr-client";
import type {
  AccessProgress,
  ClientAdapter,
  ClientSnapshotListener,
} from "./client";

const BRIDGE_EVENT = "waltr://bridge";

type BridgeEvent = {
  generation: number;
  sequence: number;
  payload: BridgePayload;
};

type BridgeRecovery = {
  generation: number;
  events: BridgeEvent[];
};

type BridgePayload =
  | { type: "state"; snapshot: ClientSnapshot }
  | { type: "access"; progress: unknown };

type TauriApi = {
  invoke: typeof invoke;
  listen: typeof listen;
};

export async function loadTauriClient(api: TauriApi = { invoke, listen }): Promise<ClientAdapter> {
  let currentSnapshot: ClientSnapshot | undefined;
  let currentProgress: AccessProgress | undefined;
  let generation: number | undefined;
  let announcedGeneration: number | undefined;
  let sequence = -1;
  let started = false;
  let runtimeReady = false;
  let disposed = false;
  let unlisten: UnlistenFn | undefined;
  let eventQueue = Promise.resolve();
  const pendingEvents: BridgeEvent[] = [];
  const snapshotListeners = new Set<ClientSnapshotListener>();
  const progressListeners = new Set<(progress: AccessProgress) => void>();

  const publishSnapshot = (snapshot: ClientSnapshot) => {
    currentSnapshot = snapshot;
    for (const listener of snapshotListeners) listener(snapshot);
  };
  const publishProgress = (progress: AccessProgress) => {
    currentProgress = progress;
    for (const listener of progressListeners) listener(progress);
  };
  const accept = (event: BridgeEvent) => {
    sequence = event.sequence;
    if (event.payload.type === "state") {
      publishSnapshot(event.payload.snapshot);
    } else {
      publishProgress(presentationProgress(event.payload.progress));
    }
  };
  const recover = async () => {
    const requestedGeneration = generation;
    if (requestedGeneration === undefined) return false;
    const recovery = await api.invoke<BridgeRecovery>("client_state", {
      afterSequence: sequence < 0 ? null : sequence,
    });
    if (disposed
      || generation !== requestedGeneration
      || announcedGeneration !== requestedGeneration
      || recovery.generation !== requestedGeneration
      || recovery.events.some((event) => event.generation !== recovery.generation)) {
      return false;
    }
    for (const event of recovery.events) {
      if (event.sequence <= sequence) continue;
      accept(event);
    }
    return true;
  };
  const processEvent = async (event: BridgeEvent) => {
    if (disposed) return;
    if (event.generation !== generation) {
      const resumesFreshGeneration = runtimeReady
        && generation !== undefined
        && event.generation > generation
        && event.sequence === 0
        && event.payload.type === "state";
      if (!resumesFreshGeneration) return;
      generation = event.generation;
      announcedGeneration = event.generation;
      sequence = -1;
      currentProgress = undefined;
    }
    if (event.sequence <= sequence) return;

    if (event.sequence > sequence + 1) {
      if (!await recover()) return;
    }

    if (event.sequence === sequence + 1) accept(event);
    else if (event.sequence > sequence && event.payload.type === "state") accept(event);
  };
  const enqueue = (event: BridgeEvent) => {
    const queued = eventQueue.then(() => processEvent(event));
    eventQueue = queued.catch(() => undefined);
    return queued;
  };

  const start = async () => {
    if (started || disposed) return;
    started = true;
    try {
      unlisten = await api.listen<BridgeEvent>(BRIDGE_EVENT, ({ payload }) => {
        if (runtimeReady
          && payload.payload.type === "state"
          && payload.sequence === 0
          && (announcedGeneration === undefined || payload.generation > announcedGeneration)) {
          announcedGeneration = payload.generation;
        }
        if (generation === undefined) pendingEvents.push(payload);
        else void enqueue(payload).catch(() => undefined);
      });
      if (disposed) {
        unlisten();
        unlisten = undefined;
        return;
      }

      const runtime = await api.invoke<{ generation: number }>("start_runtime");
      if (disposed) {
        await api.invoke("dispose_runtime");
        return;
      }
      generation = runtime.generation;
      announcedGeneration = runtime.generation;
      for (const event of pendingEvents.splice(0)) await enqueue(event);
      await eventQueue;
      if (!currentSnapshot) {
        await recover();
      }
      runtimeReady = true;
    } catch (error) {
      started = false;
      unlisten?.();
      unlisten = undefined;
      await api.invoke("dispose_runtime").catch(() => undefined);
      throw error;
    }
  };

  const dispose = async () => {
    if (disposed) return;
    disposed = true;
    runtimeReady = false;
    announcedGeneration = undefined;
    unlisten?.();
    unlisten = undefined;
    snapshotListeners.clear();
    progressListeners.clear();
    await api.invoke("dispose_runtime");
  };

  return {
    snapshot: () => currentSnapshot,
    start,
    subscribe: (listener) => {
      snapshotListeners.add(listener);
      if (currentSnapshot) listener(currentSnapshot);
      return () => snapshotListeners.delete(listener);
    },
    dispatch: async (command: ClientCommand) => {
      const event = await api.invoke<BridgeEvent>("client_dispatch", { command });
      if (disposed || event.generation !== generation) throw new Error("native command returned stale client state");
      await enqueue(event);
      if (event.payload.type !== "state") throw new Error("native command did not return client state");
      if (!currentSnapshot) throw new Error("native command did not publish client state");
      return currentSnapshot;
    },
    dispose,
    access: {
      progress: () => currentProgress,
      subscribe: (listener) => {
        progressListeners.add(listener);
        if (currentProgress) listener(currentProgress);
        return () => progressListeners.delete(listener);
      },
      start: () => api.invoke("access_start"),
      cancel: () => api.invoke("access_cancel"),
      logout: async () => {
        const event = await api.invoke<BridgeEvent | null>("access_logout");
        if (event) await enqueue(event);
      },
    },
    openExternalUrl: (url) => api.invoke("open_external_url", { url }),
  };
}

export const loadClientAdapter = loadTauriClient;

function presentationProgress(progress: unknown): AccessProgress {
  if (isRecord(progress) && progress.type === "loggingOut") return { type: "loggingOut" };
  if (isRecord(progress) && progress.type === "unavailable" && typeof progress.message === "string") {
    return { type: "unavailable", message: progress.message };
  }
  return { type: "native" };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
