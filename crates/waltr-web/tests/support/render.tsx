import { cleanup, render } from "@solidjs/testing-library";
import { createMemo, createSignal, type JSX } from "solid-js";
import { afterEach } from "vitest";

import { AppStateContextProvider } from "../../src/app/context";
import type { ClientCommand, ClientSnapshot, ConfigView, SessionView } from "../../src/generated/waltr-client";
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
  } = {},
) {
  const commands = options.commands ?? [];
  let nextOperationId = 0;
  resetTestHistory(options.routePath ?? "/config/provider");

  return render(() => {
    const [snapshot, setSnapshot] = createSignal<ClientSnapshot | undefined>(
      options.snapshot,
    );
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
          dispatchCreateSession,
          dispatchDeleteSession,
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
