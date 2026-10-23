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
import type { ClientAdapter, ClientAdapterLoader } from "../platform/client";
import { FixtureAppStateProvider, fixtureSnapshotForPath } from "./fixtures";
import { reportApplicationError } from "../telemetry";

type AppStateProviderProps = ParentProps<{
  loadClientAdapter: ClientAdapterLoader;
}>;

export function AppStateProvider(props: AppStateProviderProps) {
  const fixtureSnapshot = typeof window === "undefined" ? undefined : fixtureSnapshotForPath(window.location.pathname);

  if (fixtureSnapshot) {
    return <FixtureAppStateProvider snapshot={fixtureSnapshot}>{props.children}</FixtureAppStateProvider>;
  }

  const [snapshot, setSnapshot] = createSignal<ClientSnapshot>();
  const [clientError, setClientError] = createSignal<unknown>();
  const [explicitAuthTarget, setExplicitAuthTarget] = createSignal<Extract<ClientCommand, { type: "beginAuth" }>["target"]>();
  const [presentedAuthAttemptId, setPresentedAuthAttemptId] = createSignal<string>();
  const [ignoredPresentedAttemptId, setIgnoredPresentedAttemptId] = createSignal<string>();
  const [authorizationDraft, setAuthorizationDraft] = createSignal<{ attemptId: string; input: string }>();
  const [ignoredTerminalAttemptId, setIgnoredTerminalAttemptId] = createSignal<string>();
  const operationWaiters = new Set<OperationWaiter>();
  const [clientAdapter] = createResource(() => props.loadClientAdapter());
  const [minimumSplashElapsed] = createResource(async () => {
    await sleep(300);
    return true;
  });
  const systemReady = createMemo(() => snapshot()?.status.ready ?? false);
  let clientLoadErrorReported = false;

  createEffect(() => {
    if (clientAdapter.error && !clientLoadErrorReported) {
      clientLoadErrorReported = true;
      reportApplicationError("startup", "load_failed");
    }
  });

  createEffect(() => {
    const explicitTarget = explicitAuthTarget();
    const presentedAttemptId = presentedAuthAttemptId();
    const attempt = snapshot()?.auth.activeAttempt;
    if (!explicitTarget
      && attempt
      && attempt.attemptId !== ignoredPresentedAttemptId()
      && ["pending", "completing"].includes(attempt.lifecycle)) {
      setExplicitAuthTarget(attempt.target);
      setPresentedAuthAttemptId(attempt.attemptId);
      setIgnoredPresentedAttemptId(undefined);
      return;
    }
    if (!explicitTarget) return;
    if (!presentedAttemptId) {
      if (attempt
        && attempt.attemptId !== ignoredTerminalAttemptId()
        && sameAuthTarget(explicitTarget, attempt.target)) {
        setPresentedAuthAttemptId(attempt.attemptId);
      }
      return;
    }
    if (!attempt
      || attempt.attemptId !== presentedAttemptId
      || !sameAuthTarget(explicitTarget, attempt.target)) {
      setExplicitAuthTarget(undefined);
      setPresentedAuthAttemptId(undefined);
      setIgnoredPresentedAttemptId(attempt?.attemptId ?? presentedAttemptId);
      setIgnoredTerminalAttemptId(undefined);
      setAuthorizationDraft(undefined);
      return;
    }
    if (attempt?.lifecycle === "succeeded"
      && attempt.attemptId !== ignoredTerminalAttemptId()
      && sameAuthTarget(explicitTarget, attempt.target)) {
      setExplicitAuthTarget(undefined);
      setPresentedAuthAttemptId(undefined);
      setIgnoredTerminalAttemptId(undefined);
      setAuthorizationDraft(undefined);
    }
  });

  createEffect(() => {
    const client = clientAdapter();

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
        reportApplicationError("startup", "start_failed");
        setClientError(error);
      }
    });

    onCleanup(() => {
      cancelled = true;
      unsubscribe();
      void client.dispose();
    });
  });

  createEffect(() => {
    const client = clientAdapter();
    if (!client) return;
    const dispose = () => { void client.dispose(); };
    const pagehide = (event: PageTransitionEvent) => {
      if (!event.persisted) dispose();
    };
    const pageshow = (event: PageTransitionEvent) => {
      if (event.persisted) void client.start().catch((error) => {
        reportApplicationError("recovery", "resume_failed");
        setClientError(error);
      });
    };
    window.addEventListener("pagehide", pagehide);
    window.addEventListener("pageshow", pageshow);
    window.addEventListener("beforeunload", dispose);
    onCleanup(() => {
      window.removeEventListener("pagehide", pagehide);
      window.removeEventListener("pageshow", pageshow);
      window.removeEventListener("beforeunload", dispose);
    });
  });

  const phase = createMemo<AppPhase>(() => {
    if (clientAdapter.error || clientError()) {
      return "splash";
    }

    return systemReady() ? "ready" : "splash";
  });

  const dispatch = async (command: ClientCommand) => {
    try {
      return await getLoadedClient(clientAdapter()).dispatch(command);
    } catch (error) {
      reportApplicationError("dispatch", "failed");
      throw error;
    }
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
    setPresentedAuthAttemptId(undefined);
    setIgnoredPresentedAttemptId(undefined);
    try {
      return await dispatchBeginAuth(target);
    } catch (error) {
      if (sameAuthTarget(explicitAuthTarget(), target)) {
        setExplicitAuthTarget(undefined);
        setPresentedAuthAttemptId(undefined);
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
      setPresentedAuthAttemptId(undefined);
      setIgnoredTerminalAttemptId(undefined);
      setAuthorizationDraft(undefined);
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
      setPresentedAuthAttemptId(undefined);
      setIgnoredTerminalAttemptId(undefined);
      setAuthorizationDraft(undefined);
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
    try {
      const immediate = await getLoadedClient(clientAdapter()).dispatch(options.command);
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

      return await new Promise<ClientSnapshot>((resolve, reject) => {
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
    } catch (error) {
      reportApplicationError("dispatch", "failed");
      throw error;
    }
  }

  return (
    <AppStateContextProvider
      value={{
        phase,
        minimumSplashElapsed,
        snapshot,
        clientError,
        clientLoadError: () => clientAdapter.error,
        clientLoaded: () => Boolean(clientAdapter()),
        systemReady,
        explicitAuthTarget,
        presentedAuthAttemptId,
        authorizationDraft,
        dictation: () => { snapshot(); return clientAdapter()?.dictation; },
        dictationPreferenceScope: () => clientAdapter()?.dictationPreferenceScope,
        accessOperations: () => clientAdapter()?.access,
        openAuthorizationAttempt: () => clientAdapter()?.openAuthorizationAttempt,
        openExternalUrl: () => clientAdapter()?.openExternalUrl,
        dispatch,
        dispatchBeginExplicitAuth,
        dispatchCompleteAuth,
        dispatchCancelAuth,
        dispatchRefreshAuth,
        dispatchLogoutAuth,
        clearExplicitAuthTarget: () => {
          setExplicitAuthTarget(undefined);
          setPresentedAuthAttemptId(undefined);
        },
        setAuthorizationDraft: (attemptId, input) => setAuthorizationDraft({ attemptId, input }),
        clearAuthorizationDraft: () => setAuthorizationDraft(undefined),
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

function getLoadedClient(client: ClientAdapter | undefined) {
  if (!client) {
    throw new Error("client is not loaded");
  }

  return client;
}

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, milliseconds));
}
