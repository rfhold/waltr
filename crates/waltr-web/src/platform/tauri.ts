import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { acceptNativeAppsBootstrap } from "../mcp-apps/policy";

import type { ClientCommand, ClientSnapshot } from "../generated/waltr-client";
import type {
  ClientAdapter,
  ClientSnapshotListener,
} from "./client";

import { nativeDictation } from "./native-dictation";
import type { DictationHost } from "./dictation";

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
  | { type: "platform"; startup: unknown };

type BridgeStartupState =
  | "starting"
  | "notificationPermissionRequired"
  | "firstCatalogDownload"
  | "ready"
  | "recoverableFailure"
  | "stopped";

type TauriApi = {
  invoke: typeof invoke;
  listen: typeof listen;
};

export type ServerProfile = { id: string; name: string; url?: string };
export type ServerRegistry = { servers: ServerProfile[]; lastConnectedId: string | null };

export const serverRegistry = {
  list: () => invoke<ServerRegistry>("list_servers"),
  save: (url: string, name?: string) => invoke<ServerProfile>("save_server", { url, name }),
  remove: (serverId: string) => invoke<void>("remove_server", { serverId }),
};

export async function loadTauriClient(api: TauriApi = { invoke, listen }, serverId?: string): Promise<ClientAdapter> {
  let currentSnapshot: ClientSnapshot | undefined;
  let dictation: DictationHost | undefined;
  let dictationSupported = false;
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

  const publishSnapshot = (snapshot: ClientSnapshot) => {
    if (!snapshot.status.ready) dictation?.cancel();
    currentSnapshot = snapshot;
    for (const listener of snapshotListeners) listener(snapshot);
  };
  const accept = (event: BridgeEvent) => {
    sequence = event.sequence;
    if (event.payload.type === "state") {
      publishSnapshot(event.payload.snapshot);
    } else if (event.payload.startup !== "ready") {
      dictation?.cancel();
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
        && canAnnounceGeneration(event);
      if (!resumesFreshGeneration) return;
      dictation?.dispose();
      generation = event.generation;
      dictation = dictationSupported ? nativeDictation(api.invoke, generation) : undefined;
      announcedGeneration = event.generation;
      sequence = -1;
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
          && canAnnounceGeneration(payload)
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

      const runtime = await api.invoke<{ generation: number; nativeApps?: unknown; dictation?: boolean }>("start_runtime", { serverId });
      if (disposed) {
        await api.invoke("dispose_runtime", { generation: runtime.generation });
        return;
      }
      generation = runtime.generation;
      dictationSupported = runtime.dictation === true;
      dictation = dictationSupported ? nativeDictation(api.invoke, generation) : undefined;
      acceptNativeAppsBootstrap(runtime.nativeApps);
      announcedGeneration = runtime.generation;
      for (const event of pendingEvents.splice(0)) await enqueue(event);
      await eventQueue;
      if (!currentSnapshot) {
        await recover();
      }
      runtimeReady = true;
    } catch (error) {
      if (!disposed) acceptNativeAppsBootstrap(undefined);
      started = false;
      unlisten?.();
      unlisten = undefined;
      if (generation !== undefined) {
        await api.invoke("dispose_runtime", { generation }).catch(() => undefined);
      }
      throw error;
    }
  };

  const dispose = async () => {
    if (disposed) return;
    disposed = true;
    dictation?.dispose();
    acceptNativeAppsBootstrap(undefined);
    runtimeReady = false;
    announcedGeneration = undefined;
    unlisten?.();
    unlisten = undefined;
    snapshotListeners.clear();
    if (generation !== undefined) await api.invoke("dispose_runtime", { generation });
  };

  return {
    snapshot: () => currentSnapshot,
    get dictation() { return dictation; },
    dictationPreferenceScope: `native:${serverId ?? "local"}`,
    start,
    subscribe: (listener) => {
      snapshotListeners.add(listener);
      if (currentSnapshot) listener(currentSnapshot);
      return () => snapshotListeners.delete(listener);
    },
    dispatch: async (command: ClientCommand) => {
      if (disposed || generation === undefined) throw new Error("Native client is not ready");
      const event = await api.invoke<BridgeEvent>("client_dispatch", { command, generation });
      if (disposed || event.generation !== generation) throw new Error("native command returned stale client state");
      await enqueue(event);
      if (event.payload.type !== "state") throw new Error("native command did not return client state");
      if (!currentSnapshot) throw new Error("native command did not publish client state");
      return currentSnapshot;
    },
    dispose,
    access: serverId && serverId !== "local" ? {
      openLogin: async () => {
        if (disposed || generation === undefined) throw new Error("Native client is not ready");
        dictation?.cancel();
        await api.invoke("start_access_login", { generation });
      },
      logout: async () => {
        if (disposed || generation === undefined) throw new Error("Native client is not ready");
        dictation?.cancel();
        await api.invoke("logout_access", { generation });
        return true;
      },
    } : undefined,
    openAuthorizationAttempt: async (attemptId) => {
      if (disposed || generation === undefined) throw new Error("Native client is not ready");
      await api.invoke("open_authorization_attempt", { attemptId, generation });
    },
    openExternalUrl: async (url) => {
      if (disposed || generation === undefined) throw new Error("Native client is not ready");
      await api.invoke("open_external_url", { url, generation });
    },
  };
}

export const loadClientAdapter = loadTauriClient;

function canAnnounceGeneration(event: BridgeEvent) {
  return event.sequence === 0
    && (event.payload.type === "state"
      || isBridgeStartupState(event.payload.startup));
}

function isBridgeStartupState(startup: unknown): startup is BridgeStartupState {
  return startup === "starting"
    || startup === "notificationPermissionRequired"
    || startup === "firstCatalogDownload"
    || startup === "ready"
    || startup === "recoverableFailure"
    || startup === "stopped";
}
