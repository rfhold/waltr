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

    setSnapshot(client.snapshot());
    const unsubscribe = client.subscribe(setSnapshot);
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
      }}
    >
      {props.children}
    </AppStateContextProvider>
  );
}

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, milliseconds));
}
