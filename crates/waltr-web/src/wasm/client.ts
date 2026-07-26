import init, { WaltrClient } from "../../generated/wasm-current/waltr_client";

export type {
  ClientCommand,
  ClientSnapshot,
  OperationSnapshot,
} from "../generated/waltr-client";

import type { ClientCommand, ClientSnapshot } from "../generated/waltr-client";

export type ClientSnapshotListener = (snapshot: ClientSnapshot) => void;

export type WasmClient = {
  endpoint: string;
  snapshot: () => ClientSnapshot | undefined;
  start: () => Promise<void>;
  subscribe: (listener: ClientSnapshotListener) => () => void;
  dispatch: (command: ClientCommand) => Promise<ClientSnapshot>;
};

export type WasmClientLoader = () => Promise<WasmClient>;

export async function loadWasmClient(): Promise<WasmClient> {
  await init();

  const client = WaltrClient.forCurrentOrigin();
  let currentSnapshot: ClientSnapshot | undefined;
  let currentSnapshotJson: string | undefined;
  let started = false;
  const listeners = new Set<ClientSnapshotListener>();

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
    try {
      return publishJson(await client.dispatchJson(JSON.stringify(command)));
    } catch (error) {
      await readSnapshot();
      throw error;
    }
  };

  const start = async () => {
    if (started) {
      return;
    }

    started = true;
    await readSnapshot();

    while (started) {
      publishJson(await client.nextStateJson());
    }
  };

  return {
    endpoint: client.endpoint(),
    snapshot: () => currentSnapshot,
    start,
    subscribe: (listener) => {
      listeners.add(listener);

      if (currentSnapshot) {
        listener(currentSnapshot);
      }

      return () => {
        listeners.delete(listener);
      };
    },
    dispatch,
  };
}
