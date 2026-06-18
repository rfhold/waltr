import {
  createEffect,
  createMemo,
  createResource,
  createSignal,
  onCleanup,
  type ParentProps,
} from "solid-js";

import { AppStateContextProvider, type AppPhase } from "./context";
import type { ClientCommand, ClientSnapshot } from "../generated/waltr-client";
import {
  loadWasmClient,
  type WasmClientLoader,
} from "../wasm/client";

type AppStateProviderProps = ParentProps<{
  loadWasmClient?: WasmClientLoader;
}>;

export function AppStateProvider(props: AppStateProviderProps) {
  const [snapshot, setSnapshot] = createSignal<ClientSnapshot>();
  const [clientError, setClientError] = createSignal<unknown>();
  const configPatchWaiters = new Set<ConfigPatchWaiter>();
  const [wasmClient] = createResource(() =>
    (props.loadWasmClient ?? loadWasmClient)(),
  );
  const [minimumSplashElapsed] = createResource(async () => {
    await sleep(300);
    return true;
  });
  const systemReady = createMemo(() => snapshot()?.status.ready ?? false);

  createEffect(() => {
    const client = wasmClient();

    if (!client) {
      return;
    }

    const initialSnapshot = client.snapshot();
    if (initialSnapshot) {
      publishSnapshot(initialSnapshot, setSnapshot, configPatchWaiters);
    }

    const unsubscribe = client.subscribe((snapshot) => {
      publishSnapshot(snapshot, setSnapshot, configPatchWaiters);
    });
    let cancelled = false;

    void client.start().catch((error) => {
      if (!cancelled) {
        setClientError(error);
      }
    });

    onCleanup(() => {
      cancelled = true;
      unsubscribe();
    });
  });

  const phase = createMemo<AppPhase>(() => {
    if (wasmClient.error || clientError()) {
      return "splash";
    }

    return systemReady() ? "ready" : "splash";
  });

  const dispatch = async (command: ClientCommand) => {
    const client = wasmClient();

    if (!client) {
      throw new Error("client is not loaded");
    }

    return client.dispatch(command);
  };

  const dispatchConfigPatch = async (command: ClientCommand) => {
    const client = wasmClient();

    if (!client) {
      throw new Error("client is not loaded");
    }

    const immediate = await client.dispatch(command);
    const key = immediate.config.patch.key;

    if (!key) {
      throw new Error("config patch command did not produce an operation key");
    }

    const current = snapshot() ?? immediate;
    const currentResult = configPatchResult(current, key);
    if (currentResult?.status === "ready") {
      return current;
    }
    if (currentResult?.status === "failed") {
      throw new Error(currentResult.error ?? "config patch failed");
    }

    return new Promise<ClientSnapshot>((resolve, reject) => {
      const waiter: ConfigPatchWaiter = { key, resolve, reject };
      configPatchWaiters.add(waiter);
      settleConfigPatchWaiter(waiter, snapshot() ?? immediate, configPatchWaiters);
    });
  };

  return (
    <AppStateContextProvider
      value={{
        phase,
        minimumSplashElapsed,
        snapshot,
        clientError,
        clientLoadError: () => wasmClient.error,
        clientLoaded: () => Boolean(wasmClient()),
        systemReady,
        dispatch,
        dispatchConfigPatch,
      }}
    >
      {props.children}
    </AppStateContextProvider>
  );
}

type ConfigPatchWaiter = {
  key: string;
  resolve: (snapshot: ClientSnapshot) => void;
  reject: (error: Error) => void;
};

function publishSnapshot(
  snapshot: ClientSnapshot,
  setSnapshot: (snapshot: ClientSnapshot) => void,
  configPatchWaiters: Set<ConfigPatchWaiter>,
) {
  setSnapshot(snapshot);
  settleConfigPatchWaiters(snapshot, configPatchWaiters);
}

function settleConfigPatchWaiters(
  snapshot: ClientSnapshot,
  configPatchWaiters: Set<ConfigPatchWaiter>,
) {
  for (const waiter of configPatchWaiters) {
    settleConfigPatchWaiter(waiter, snapshot, configPatchWaiters);
  }
}

function settleConfigPatchWaiter(
  waiter: ConfigPatchWaiter,
  snapshot: ClientSnapshot,
  configPatchWaiters: Set<ConfigPatchWaiter>,
) {
  const result = configPatchResult(snapshot, waiter.key);

  if (result?.status === "ready") {
    configPatchWaiters.delete(waiter);
    waiter.resolve(snapshot);
  } else if (result?.status === "failed") {
    configPatchWaiters.delete(waiter);
    waiter.reject(new Error(result.error ?? "config patch failed"));
  }
}

function configPatchResult(snapshot: ClientSnapshot, key: string) {
  const result = snapshot.operations.current[key];

  return result?.kind === "configPatch" ? result : undefined;
}

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, milliseconds));
}
