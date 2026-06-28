import { cleanup, render } from "@solidjs/testing-library";
import { createMemo, createSignal, type JSX } from "solid-js";
import { afterEach } from "vitest";

import { AppStateContextProvider } from "../../src/app/context";
import type { ClientCommand, ClientOperationView, ClientSnapshot, ConfigView, SessionView } from "../../src/generated/waltr-client";
import type { CatalogSearchResponse, CatalogSuggestResponse } from "../../src/wasm/client";
import { resetTestHistory } from "./routes";
import { emptyConfig, snapshotWithConfig } from "./snapshots";

afterEach(() => {
  cleanup();
});

export function renderWithAppState(
  ui: () => JSX.Element,
  options: {
    snapshot?: ClientSnapshot;
    commands?: ClientCommand[];
    clientLoaded?: boolean;
    clientLoadError?: unknown;
    clientError?: unknown;
    minimumSplashElapsed?: boolean;
    routePath?: string;
    configPatchError?: string;
    sessionCreateError?: string;
    sessionDeleteError?: string;
    sessionRuntimeConfigUpdateError?: string;
    submitUserMessageError?: string;
    respondExternalRequestError?: string;
    interruptSessionError?: string;
    searchCatalogModels?: (request: { query: string; providerId?: string; limit?: number }) => Promise<CatalogSearchResponse>;
    suggestCatalogMatches?: (request: { providerKey: string; modelKey: string; limit?: number }) => Promise<CatalogSuggestResponse>;
    onSnapshotSetter?: (setSnapshot: (snapshot: ClientSnapshot) => void) => void;
  } = {},
) {
  const commands = options.commands ?? [];
  let nextOperationId = 0;
  resetTestHistory(options.routePath ?? "/config/provider");

  return render(() => {
    const [snapshot, setSnapshot] = createSignal<ClientSnapshot | undefined>(
      options.snapshot,
    );
    options.onSnapshotSetter?.(setSnapshot);
    const systemReady = createMemo(() => snapshot()?.status.ready ?? false);
    const phase = createMemo(() => systemReady() ? "ready" : "splash");
    const dispatch = async (command: ClientCommand) => {
      commands.push(command);
      return snapshot() ?? snapshotWithConfig(emptyConfig(""));
    };

    const dispatchConfigPatch = async (command: ClientCommand) => {
      commands.push(command);
      const key = `op-${nextOperationId++}`;
      const current = snapshot()?.config.current ?? emptyConfig("");
      const loading = snapshotWithConfig(current);
      loading.config.patch = { status: "loading", error: null, key };
      loading.operations.current[key] = {
        kind: "configPatch",
        status: "loading",
        error: null,
        config: null,
      };
      setSnapshot(loading);

      await Promise.resolve();

      if (options.configPatchError) {
        const failed = snapshotWithConfig(current);
        failed.config.patch = { status: "failed", error: options.configPatchError, key };
        failed.operations.current[key] = {
          kind: "configPatch",
          status: "failed",
          error: options.configPatchError,
          config: null,
        };
        setSnapshot(failed);
        throw new Error(options.configPatchError);
      }

      const patchedConfig = applyConfigPatchCommand(current, command);
      const ready = snapshotWithConfig(patchedConfig);
      ready.config.patch = { status: "ready", error: null, key };
      ready.operations.current[key] = {
        kind: "configPatch",
        status: "ready",
        error: null,
        config: patchedConfig,
      };
      setSnapshot(ready);

      return ready;
    };

    const dispatchCreateSession = async () => {
      const command: ClientCommand = { type: "createSession" };
      commands.push(command);
      const key = `op-${nextOperationId++}`;
      const current = snapshot() ?? snapshotWithConfig(emptyConfig(""));
      const session: SessionView = {
        id: `session-${nextOperationId}`,
        revision: 0n,
        status: { phase: "idle", failedMessage: "" },
        currentPrompt: null,
        createdAt: { seconds: 1_700_000_000n + BigInt(nextOperationId), nanos: 0 },
        updatedAt: { seconds: 1_700_000_000n + BigInt(nextOperationId), nanos: 0 },
        lastActivityAt: { seconds: 1_700_000_000n + BigInt(nextOperationId), nanos: 0 },
        runtimeConfig: null,
        metrics: {
          inputTokens: 0n,
          outputTokens: 0n,
          cacheReadTokens: 0n,
          cacheWriteTokens: 0n,
          imageInputTokens: 0n,
          imageInputCount: 0n,
          totalTokens: 0n,
          costCurrency: null,
          totalCostMicros: 0n,
        },
      };
      const loading: ClientSnapshot = {
        ...current,
        operations: { current: { ...current.operations.current } },
        sessions: {
          ...current.sessions,
          create: { status: "loading", error: null, key },
        },
      };
      loading.operations.current[key] = {
        kind: "sessionCreate",
        status: "loading",
        error: null,
        session: null,
      };
      setSnapshot(loading);

      await Promise.resolve();

      if (options.sessionCreateError) {
        const failed: ClientSnapshot = {
          ...current,
          operations: { current: { ...current.operations.current } },
          sessions: {
            ...current.sessions,
            create: { status: "failed", error: options.sessionCreateError, key },
          },
        };
        failed.operations.current[key] = {
          kind: "sessionCreate",
          status: "failed",
          error: options.sessionCreateError,
          session: null,
        };
        setSnapshot(failed);
        throw new Error(options.sessionCreateError);
      }

      const ready: ClientSnapshot = {
        ...current,
        operations: { current: { ...current.operations.current } },
        sessions: {
          ...current.sessions,
          current: [...current.sessions.current, session],
          create: { status: "ready", error: null, key },
        },
      };
      ready.operations.current[key] = {
        kind: "sessionCreate",
        status: "ready",
        error: null,
        session,
      };
      setSnapshot(ready);

      return ready;
    };

    const dispatchDeleteSession = async (id: string) => {
      const command: ClientCommand = { type: "deleteSession", id };
      commands.push(command);
      const key = `op-${nextOperationId++}`;
      const current = snapshot() ?? snapshotWithConfig(emptyConfig(""));
      const loading: ClientSnapshot = {
        ...current,
        operations: { current: { ...current.operations.current } },
        sessions: {
          ...current.sessions,
          delete: { status: "loading", error: null, key },
        },
      };
      loading.operations.current[key] = {
        kind: "sessionDelete",
        status: "loading",
        error: null,
        id,
      };
      setSnapshot(loading);

      await Promise.resolve();

      if (options.sessionDeleteError) {
        const failed: ClientSnapshot = {
          ...current,
          operations: { current: { ...current.operations.current } },
          sessions: {
            ...current.sessions,
            delete: { status: "failed", error: options.sessionDeleteError, key },
          },
        };
        failed.operations.current[key] = {
          kind: "sessionDelete",
          status: "failed",
          error: options.sessionDeleteError,
          id,
        };
        setSnapshot(failed);
        throw new Error(options.sessionDeleteError);
      }

      const ready: ClientSnapshot = {
        ...current,
        operations: { current: { ...current.operations.current } },
        sessions: {
          ...current.sessions,
          current: current.sessions.current.filter((session) => session.id !== id),
          delete: { status: "ready", error: null, key },
        },
      };
      ready.operations.current[key] = {
        kind: "sessionDelete",
        status: "ready",
        error: null,
        id,
      };
      setSnapshot(ready);

      return ready;
    };

    const dispatchUpdateSessionRuntimeConfig = async (command: Extract<ClientCommand, { type: "updateSessionRuntimeConfig" }>) => {
      return dispatchSessionOperation({
        command,
        operationName: "runtimeConfigUpdate",
        kind: "sessionRuntimeConfigUpdate",
        error: options.sessionRuntimeConfigUpdateError,
        loadingOperation: { kind: "sessionRuntimeConfigUpdate", status: "loading", error: null, session: null },
        readyOperation: (current) => ({ kind: "sessionRuntimeConfigUpdate", status: "ready", error: null, session: current.sessions.current.find((session) => session.id === command.id) ?? null }),
        failedOperation: (error) => ({ kind: "sessionRuntimeConfigUpdate", status: "failed", error, session: null }),
      });
    };

    const dispatchSubmitUserMessage = async (sessionId: string, text: string) => {
      return dispatchSessionOperation({
        command: { type: "submitUserMessage", sessionId, text },
        operationName: "submitUserMessage",
        kind: "sessionSubmitUserMessage",
        error: options.submitUserMessageError,
        loadingOperation: { kind: "sessionSubmitUserMessage", status: "loading", error: null, sessionId },
        readyOperation: () => ({ kind: "sessionSubmitUserMessage", status: "ready", error: null, sessionId }),
        failedOperation: (error) => ({ kind: "sessionSubmitUserMessage", status: "failed", error, sessionId }),
      });
    };

    const dispatchRespondExternalRequest = async (sessionId: string, requestId: string, responseJson: string) => {
      return dispatchSessionOperation({
        command: { type: "respondExternalRequest", sessionId, requestId, responseJson },
        operationName: "respondExternalRequest",
        kind: "sessionRespondExternalRequest",
        error: options.respondExternalRequestError,
        loadingOperation: { kind: "sessionRespondExternalRequest", status: "loading", error: null, sessionId, requestId },
        readyOperation: () => ({ kind: "sessionRespondExternalRequest", status: "ready", error: null, sessionId, requestId }),
        failedOperation: (error) => ({ kind: "sessionRespondExternalRequest", status: "failed", error, sessionId, requestId }),
      });
    };

    const dispatchInterruptSession = async (sessionId: string) => {
      return dispatchSessionOperation({
        command: { type: "interruptSession", sessionId },
        operationName: "interrupt",
        kind: "sessionInterrupt",
        error: options.interruptSessionError,
        loadingOperation: { kind: "sessionInterrupt", status: "loading", error: null, sessionId },
        readyOperation: () => ({ kind: "sessionInterrupt", status: "ready", error: null, sessionId }),
        failedOperation: (error) => ({ kind: "sessionInterrupt", status: "failed", error, sessionId }),
      });
    };

    const dispatchSessionOperation = async (operation: {
      command: ClientCommand;
      operationName: "runtimeConfigUpdate" | "submitUserMessage" | "respondExternalRequest" | "interrupt";
      kind: ClientOperationView["kind"];
      error?: string;
      loadingOperation: ClientOperationView;
      readyOperation: (snapshot: ClientSnapshot) => ClientOperationView;
      failedOperation: (error: string) => ClientOperationView;
    }) => {
      commands.push(operation.command);
      const key = `op-${nextOperationId++}`;
      const current = snapshot() ?? snapshotWithConfig(emptyConfig(""));
      const loading: ClientSnapshot = {
        ...current,
        operations: { current: { ...current.operations.current, [key]: operation.loadingOperation } },
        sessions: {
          ...current.sessions,
          [operation.operationName]: { status: "loading", error: null, key },
        },
      };
      setSnapshot(loading);

      await Promise.resolve();

      if (operation.error) {
        const failed: ClientSnapshot = {
          ...current,
          operations: { current: { ...current.operations.current, [key]: operation.failedOperation(operation.error) } },
          sessions: {
            ...current.sessions,
            [operation.operationName]: { status: "failed", error: operation.error, key },
          },
        };
        setSnapshot(failed);
        throw new Error(operation.error);
      }

      const ready: ClientSnapshot = {
        ...current,
        operations: { current: { ...current.operations.current, [key]: operation.readyOperation(current) } },
        sessions: {
          ...current.sessions,
          [operation.operationName]: { status: "ready", error: null, key },
        },
      };
      setSnapshot(ready);

      return ready;
    };

    return (
      <AppStateContextProvider
        value={{
          phase,
          minimumSplashElapsed: () => options.minimumSplashElapsed ?? true,
          snapshot,
          clientError: () => options.clientError,
          clientLoadError: () => options.clientLoadError,
          clientLoaded: () => options.clientLoaded ?? true,
          systemReady,
          dispatch,
          dispatchConfigPatch,
          searchCatalogModels: options.searchCatalogModels ?? (async () => ({ models: [], stale: false })),
          suggestCatalogMatches: options.suggestCatalogMatches ?? (async () => ({ matches: [], stale: false })),
          dispatchCreateSession,
          dispatchDeleteSession,
          dispatchUpdateSessionRuntimeConfig,
          dispatchSubmitUserMessage,
          dispatchRespondExternalRequest,
          dispatchInterruptSession,
        }}
      >
        {ui()}
      </AppStateContextProvider>
    );
  });
}

function applyConfigPatchCommand(current: ConfigView, command: ClientCommand): ConfigView {
  if (command.type === "addProvider") {
    return {
      ...current,
      providers: { ...current.providers, [command.provider.name]: command.provider },
    };
  }

  if (command.type === "updateProvider") {
    const providers = { ...current.providers };
    delete providers[command.originalName];
    providers[command.provider.name] = command.provider;
    return { ...current, providers };
  }

  if (command.type === "deleteProvider") {
    const providers = { ...current.providers };
    delete providers[command.name];
    return { ...current, providers };
  }

  if (command.type === "addMcpServer") {
    return {
      ...current,
      mcpServers: { ...current.mcpServers, [command.server.name]: command.server },
    };
  }

  if (command.type === "updateMcpServer") {
    const mcpServers = { ...current.mcpServers };
    delete mcpServers[command.originalName];
    mcpServers[command.server.name] = command.server;
    return { ...current, mcpServers };
  }

  if (command.type === "deleteMcpServer") {
    const mcpServers = { ...current.mcpServers };
    delete mcpServers[command.name];
    return { ...current, mcpServers };
  }

  if (command.type === "setRuntimeDefaults") {
    return { ...current, runtimeDefaults: command.defaults };
  }

  return current;
}
