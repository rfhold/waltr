import init, { WaltrClient } from "../../generated/wasm-current/waltr_client";

export type {
  ClientCommand,
  ClientSnapshot,
  OperationSnapshot,
} from "../generated/waltr-client";

import type { ClientCommand, ClientSnapshot } from "../generated/waltr-client";
import type { ClientAdapter, ClientSnapshotListener } from "../platform/client";

export async function loadBrowserClient(): Promise<ClientAdapter> {
  await init();

  const client = WaltrClient.forCurrentOrigin();
  let currentSnapshot: ClientSnapshot | undefined;
  let currentSnapshotJson: string | undefined;
  let started = false;
  let disposed = false;
  const listeners = new Set<ClientSnapshotListener>();
  const disposedError = () => new Error("client adapter disposed");

  const publish = (snapshot: ClientSnapshot) => {
    currentSnapshot = snapshot;

    for (const listener of listeners) {
      listener(snapshot);
    }

    return snapshot;
  };
  const publishJson = (snapshotJson: string) => {
    if (snapshotJson === currentSnapshotJson && currentSnapshot) {
      return currentSnapshot;
    }

    currentSnapshotJson = snapshotJson;

    return publish(JSON.parse(snapshotJson) as ClientSnapshot);
  };

  const readSnapshot = async () =>
    publishJson(await client.snapshotJson());

  const dispatch = async (command: ClientCommand) => {
    if (disposed) {
      throw disposedError();
    }

    try {
      const snapshotJson = await client.dispatchJson(JSON.stringify(command));

      if (disposed) {
        throw disposedError();
      }

      return publishJson(snapshotJson);
    } catch (error) {
      if (disposed) {
        throw disposedError();
      }

      await readSnapshot();
      throw error;
    }
  };

  const start = async () => {
    if (disposed) {
      throw disposedError();
    }

    if (started) {
      return;
    }

    started = true;
    await readSnapshot();

    while (started) {
      try {
        const snapshotJson = await client.nextStateJson();

        if (!started) {
          return;
        }

        publishJson(snapshotJson);
      } catch (error) {
        if (disposed) {
          return;
        }

        throw error;
      }
    }
  };

  return {
    snapshot: () => currentSnapshot,
    start,
    subscribe: (listener) => {
      if (disposed) {
        return () => {};
      }

      listeners.add(listener);

      if (currentSnapshot) {
        listener(currentSnapshot);
      }

      return () => {
        listeners.delete(listener);
      };
    },
    dispatch,
    access: {
      openLogin: (url) => window.location.assign(url),
      logout: async () => {
        const response = await fetch("/oidc/logout", { method: "POST", credentials: "same-origin" });
        return response.ok;
      },
    },
    dispose: () => {
      if (disposed) {
        return;
      }

      disposed = true;
      started = false;
      client.dispose();
      listeners.clear();
    },
  };
}

export const loadClientAdapter = loadBrowserClient;
