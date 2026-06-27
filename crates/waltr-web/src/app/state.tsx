import {
  createEffect,
  createMemo,
  createResource,
  createSignal,
  onCleanup,
  type ParentProps,
} from "solid-js";

import { AppStateContextProvider, type AppPhase } from "./context";
import type { ClientCommand, ClientOperationView, ClientSnapshot } from "../generated/waltr-client";
import {
  loadWasmClient,
  type CatalogSearchRequest,
  type CatalogSuggestRequest,
  type WasmClientLoader,
} from "../wasm/client";

type AppStateProviderProps = ParentProps<{
  loadWasmClient?: WasmClientLoader;
}>;

export function AppStateProvider(props: AppStateProviderProps) {
  const [snapshot, setSnapshot] = createSignal<ClientSnapshot>();
  const [clientError, setClientError] = createSignal<unknown>();
  const operationWaiters = new Set<OperationWaiter>();
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
      publishSnapshot(initialSnapshot, setSnapshot, operationWaiters);
    }

    const unsubscribe = client.subscribe((snapshot) => {
      publishSnapshot(snapshot, setSnapshot, operationWaiters);
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
    return getLoadedClient(wasmClient()).dispatch(command);
  };

  const dispatchConfigPatch = async (command: ClientCommand) => {
    return dispatchAndWaitForOperation({
      command,
      key: (snapshot) => snapshot.config.patch.key,
      kind: "configPatch",
      missingKeyMessage: "config patch command did not produce an operation key",
      failureMessage: "config patch failed",
    });
  };

  const dispatchCreateSession = async () => {
    return dispatchAndWaitForOperation({
      command: { type: "createSession" },
      key: (snapshot) => snapshot.sessions.create.key,
      kind: "sessionCreate",
      missingKeyMessage: "session create command did not produce an operation key",
      failureMessage: "session create failed",
    });
  };

  const searchCatalogModels = async (request: CatalogSearchRequest) =>
    getLoadedClient(wasmClient()).searchCatalogModels(request);

  const suggestCatalogMatches = async (request: CatalogSuggestRequest) =>
    getLoadedClient(wasmClient()).suggestCatalogMatches(request);

  const dispatchDeleteSession = async (id: string) => {
    return dispatchAndWaitForOperation({
      command: { type: "deleteSession", id },
      key: (snapshot) => snapshot.sessions.delete.key,
      kind: "sessionDelete",
      missingKeyMessage: "session delete command did not produce an operation key",
      failureMessage: "session delete failed",
    });
  };

  async function dispatchAndWaitForOperation(options: {
    command: ClientCommand;
    key: (snapshot: ClientSnapshot) => string | null;
    kind: OperationKind;
    missingKeyMessage: string;
    failureMessage: string;
  }) {
    const immediate = await getLoadedClient(wasmClient()).dispatch(options.command);
    const key = options.key(immediate);

    if (!key) {
      throw new Error(options.missingKeyMessage);
    }

    const current = snapshot() ?? immediate;
    const result = operationResult(current, key, options.kind);
    if (result?.status === "ready") {
      return current;
    }
    if (result?.status === "failed") {
      throw new Error(result.error ?? options.failureMessage);
    }

    return new Promise<ClientSnapshot>((resolve, reject) => {
      const waiter: OperationWaiter = {
        key,
        kind: options.kind,
        failureMessage: options.failureMessage,
        resolve,
        reject,
      };
      operationWaiters.add(waiter);
      settleOperationWaiter(waiter, snapshot() ?? immediate, operationWaiters);
    });
  }

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
        searchCatalogModels,
        suggestCatalogMatches,
        dispatchCreateSession,
        dispatchDeleteSession,
      }}
    >
      {props.children}
    </AppStateContextProvider>
  );
}

type OperationKind = ClientOperationView["kind"];

type OperationWaiter = {
  key: string;
  kind: OperationKind;
  failureMessage: string;
  resolve: (snapshot: ClientSnapshot) => void;
  reject: (error: Error) => void;
};

type LoadedWasmClient = NonNullable<ReturnType<WasmClientLoader> extends Promise<infer Client> ? Client : never>;

function publishSnapshot(
  snapshot: ClientSnapshot,
  setSnapshot: (snapshot: ClientSnapshot) => void,
  operationWaiters: Set<OperationWaiter>,
) {
  setSnapshot(snapshot);
  settleOperationWaiters(snapshot, operationWaiters);
}

function settleOperationWaiters(
  snapshot: ClientSnapshot,
  operationWaiters: Set<OperationWaiter>,
) {
  for (const waiter of operationWaiters) {
    settleOperationWaiter(waiter, snapshot, operationWaiters);
  }
}

function settleOperationWaiter(
  waiter: OperationWaiter,
  snapshot: ClientSnapshot,
  operationWaiters: Set<OperationWaiter>,
) {
  const result = operationResult(snapshot, waiter.key, waiter.kind);

  if (result?.status === "ready") {
    operationWaiters.delete(waiter);
    waiter.resolve(snapshot);
  } else if (result?.status === "failed") {
    operationWaiters.delete(waiter);
    waiter.reject(new Error(result.error ?? waiter.failureMessage));
  }
}

function operationResult(snapshot: ClientSnapshot, key: string, kind: OperationKind) {
  const result = snapshot.operations.current[key];

  return result?.kind === kind ? result : undefined;
}

function getLoadedClient(client: LoadedWasmClient | undefined) {
  if (!client) {
    throw new Error("client is not loaded");
  }

  return client;
}

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, milliseconds));
}
