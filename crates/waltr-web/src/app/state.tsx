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
import type { WasmClientLoader } from "../wasm/client";
import { FixtureAppStateProvider, fixtureSnapshotForPath } from "./fixtures";

type AppStateProviderProps = ParentProps<{
  loadWasmClient: WasmClientLoader;
}>;

export function AppStateProvider(props: AppStateProviderProps) {
  const fixtureSnapshot = typeof window === "undefined" ? undefined : fixtureSnapshotForPath(window.location.pathname);

  if (fixtureSnapshot) {
    return <FixtureAppStateProvider snapshot={fixtureSnapshot}>{props.children}</FixtureAppStateProvider>;
  }

  const [snapshot, setSnapshot] = createSignal<ClientSnapshot>();
  const [clientError, setClientError] = createSignal<unknown>();
  const [explicitAuthTarget, setExplicitAuthTarget] = createSignal<Extract<ClientCommand, { type: "beginAuth" }>["target"]>();
  const [ignoredTerminalAttemptId, setIgnoredTerminalAttemptId] = createSignal<string>();
  const operationWaiters = new Set<OperationWaiter>();
  const [wasmClient] = createResource(() => props.loadWasmClient());
  const [minimumSplashElapsed] = createResource(async () => {
    await sleep(300);
    return true;
  });
  const systemReady = createMemo(() => snapshot()?.status.ready ?? false);

  createEffect(() => {
    const explicitTarget = explicitAuthTarget();
    const attempt = snapshot()?.auth.activeAttempt;
    if (attempt?.lifecycle === "succeeded"
      && attempt.attemptId !== ignoredTerminalAttemptId()
      && sameAuthTarget(explicitTarget, attempt.target)) {
      setExplicitAuthTarget(undefined);
      setIgnoredTerminalAttemptId(undefined);
    }
  });

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

  const dispatchResetConfig = async () => {
    return dispatchAndWaitForOperation({
      command: { type: "resetConfig" },
      key: (snapshot) => snapshot.config.reset.key,
      kind: "configReset",
      missingKeyMessage: "config reset command did not produce an operation key",
      failureMessage: "config reset failed",
    });
  };

  const dispatchBeginAuth = (target: Extract<ClientCommand, { type: "beginAuth" }>["target"]) =>
    dispatchAndWaitForOperation({
      command: { type: "beginAuth", target },
      key: (snapshot) => snapshot.auth.begin.key,
      kind: "authBegin",
      missingKeyMessage: "begin auth command did not produce an operation key",
      failureMessage: "authorization could not be started",
    });

  const dispatchBeginExplicitAuth = async (target: Extract<ClientCommand, { type: "beginAuth" }>["target"]) => {
    const activeAttempt = snapshot()?.auth.activeAttempt;
    setIgnoredTerminalAttemptId(
      activeAttempt?.lifecycle === "succeeded" && sameAuthTarget(target, activeAttempt.target)
        ? activeAttempt.attemptId
        : undefined,
    );
    setExplicitAuthTarget(target);
    try {
      return await dispatchBeginAuth(target);
    } catch (error) {
      if (sameAuthTarget(explicitAuthTarget(), target)) {
        setExplicitAuthTarget(undefined);
        setIgnoredTerminalAttemptId(undefined);
      }
      throw error;
    }
  };

  const dispatchCompleteAuth = async (attemptId: string, input: string) => {
    const attemptTarget = snapshot()?.auth.activeAttempt?.target;
    const result = await dispatchAndWaitForOperation({
      command: { type: "completeAuth", attemptId, input },
      key: (snapshot) => snapshot.auth.complete.key,
      kind: "authComplete",
      missingKeyMessage: "complete auth command did not produce an operation key",
      failureMessage: "authorization could not be completed",
    });
    if (sameAuthTarget(explicitAuthTarget(), attemptTarget)) {
      setExplicitAuthTarget(undefined);
      setIgnoredTerminalAttemptId(undefined);
    }
    return result;
  };

  const dispatchCancelAuth = async (attemptId: string) => {
    const attemptTarget = snapshot()?.auth.activeAttempt?.target;
    const result = await dispatchAndWaitForOperation({
      command: { type: "cancelAuth", attemptId },
      key: (snapshot) => snapshot.auth.cancel.key,
      kind: "authCancel",
      missingKeyMessage: "cancel auth command did not produce an operation key",
      failureMessage: "authorization could not be canceled",
    });
    if (sameAuthTarget(explicitAuthTarget(), attemptTarget)) {
      setExplicitAuthTarget(undefined);
      setIgnoredTerminalAttemptId(undefined);
    }
    return result;
  };

  const dispatchRefreshAuth = (target: Extract<ClientCommand, { type: "refreshAuth" }>["target"]) =>
    dispatchAndWaitForOperation({
      command: { type: "refreshAuth", target },
      key: (snapshot) => snapshot.auth.refresh.key,
      kind: "authRefresh",
      missingKeyMessage: "refresh auth command did not produce an operation key",
      failureMessage: "credential refresh failed",
    });

  const dispatchLogoutAuth = (target: Extract<ClientCommand, { type: "logoutAuth" }>["target"]) =>
    dispatchAndWaitForOperation({
      command: { type: "logoutAuth", target },
      key: (snapshot) => snapshot.auth.logout.key,
      kind: "authLogout",
      missingKeyMessage: "reset auth command did not produce an operation key",
      failureMessage: "credential reset failed",
    });

  const dispatchCreateSession = async (agentName: string) => {
    return dispatchAndWaitForOperation({
      command: { type: "createSession", agentName },
      key: (snapshot) => snapshot.sessions.create.key,
      kind: "sessionCreate",
      missingKeyMessage: "session create command did not produce an operation key",
      failureMessage: "session create failed",
    });
  };

  const dispatchDeleteSession = async (id: string) => {
    return dispatchAndWaitForOperation({
      command: { type: "deleteSession", id },
      key: (snapshot) => snapshot.sessions.delete.key,
      kind: "sessionDelete",
      missingKeyMessage: "session delete command did not produce an operation key",
      failureMessage: "session delete failed",
    });
  };

  const dispatchWipeSessions = async () => {
    return dispatchAndWaitForOperation({
      command: { type: "wipeSessions" },
      key: (snapshot) => snapshot.sessions.wipe.key,
      kind: "sessionWipe",
      missingKeyMessage: "session wipe command did not produce an operation key",
      failureMessage: "session wipe failed",
    });
  };

  const dispatchUpdateSessionInferenceSettings = async (command: Extract<ClientCommand, { type: "updateSessionInferenceSettings" }>) => {
    return dispatchAndWaitForOperation({
      command,
      key: (snapshot) => snapshot.sessions.inferenceSettingsUpdate.key,
      kind: "sessionInferenceSettingsUpdate",
      missingKeyMessage: "session inference settings update command did not produce an operation key",
      failureMessage: "session inference settings update failed",
    });
  };

  const dispatchUpdateSessionAgent = async (id: string, agentName: string) => {
    return dispatchAndWaitForOperation({
      command: { type: "updateSessionAgent", id, agentName },
      key: (snapshot) => snapshot.sessions.agentUpdate.key,
      kind: "sessionAgentUpdate",
      missingKeyMessage: "session agent update command did not produce an operation key",
      failureMessage: "session agent update failed",
    });
  };

  const dispatchSubmitUserMessage = async (sessionId: string, text: string) => {
    return dispatchAndWaitForOperation({
      command: { type: "submitUserMessage", sessionId, text },
      key: (snapshot) => snapshot.sessions.submitUserMessage.key,
      kind: "sessionSubmitUserMessage",
      missingKeyMessage: "submit user message command did not produce an operation key",
      failureMessage: "submit user message failed",
    });
  };

  const dispatchRespondExternalRequest = async (sessionId: string, requestId: string, responseJson: string) => {
    return dispatchAndWaitForOperation({
      command: { type: "respondExternalRequest", sessionId, requestId, responseJson },
      key: (snapshot) => snapshot.sessions.respondExternalRequest.key,
      kind: "sessionRespondExternalRequest",
      missingKeyMessage: "respond external request command did not produce an operation key",
      failureMessage: "respond external request failed",
    });
  };

  const dispatchInterruptSession = async (sessionId: string) => {
    return dispatchAndWaitForOperation({
      command: { type: "interruptSession", sessionId },
      key: (snapshot) => snapshot.sessions.interrupt.key,
      kind: "sessionInterrupt",
      missingKeyMessage: "interrupt session command did not produce an operation key",
      failureMessage: "interrupt session failed",
    });
  };

  const dispatchContinueSession = async (command: Extract<ClientCommand, { type: "continueSessionFromTurn" | "continueSessionFromCursor" }>) => {
    return dispatchAndWaitForOperation({
      command,
      key: (snapshot) => snapshot.sessions.continueSession.key,
      kind: "sessionContinue",
      missingKeyMessage: "session continue command did not produce an operation key",
      failureMessage: "session continue failed",
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
        explicitAuthTarget,
        dispatch,
        dispatchBeginExplicitAuth,
        dispatchCompleteAuth,
        dispatchCancelAuth,
        dispatchRefreshAuth,
        dispatchLogoutAuth,
        clearExplicitAuthTarget: () => setExplicitAuthTarget(undefined),
        dispatchConfigPatch,
        dispatchResetConfig,
        dispatchCreateSession,
        dispatchDeleteSession,
        dispatchWipeSessions,
          dispatchUpdateSessionInferenceSettings,
          dispatchUpdateSessionAgent,
        dispatchSubmitUserMessage,
        dispatchRespondExternalRequest,
        dispatchInterruptSession,
        dispatchContinueSession,
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

function sameAuthTarget(
  left: Extract<ClientCommand, { type: "beginAuth" }>["target"] | undefined,
  right: Extract<ClientCommand, { type: "beginAuth" }>["target"] | undefined,
) {
  return Boolean(left && right && left.kind === right.kind && left.key === right.key);
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
