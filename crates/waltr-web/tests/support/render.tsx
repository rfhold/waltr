import { cleanup, render } from "@solidjs/testing-library";
import { createMemo, createSignal, type JSX } from "solid-js";
import { afterEach } from "vitest";

import { AppStateContextProvider } from "../../src/app/context";
import type { ClientCommand, ClientSnapshot } from "../../src/generated/waltr-client";
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
  } = {},
) {
  const commands = options.commands ?? [];
  resetTestHistory(options.routePath ?? "/config/provider");

  return render(() => {
    const [snapshot, setSnapshot] = createSignal<ClientSnapshot | undefined>(
      options.snapshot,
    );
    const systemReady = createMemo(() => snapshot()?.status.ready ?? false);
    const phase = createMemo(() => systemReady() ? "ready" : "splash");
    const dispatch = async (command: ClientCommand) => {
      commands.push(command);

      if (command.type === "addProvider") {
        const current = snapshot()?.config.current ?? emptyConfig("");
        setSnapshot(snapshotWithConfig({
          ...current,
          providers: { ...current.providers, [command.provider.name]: command.provider },
        }));
      }

      if (command.type === "updateProvider") {
        const current = snapshot()?.config.current ?? emptyConfig("");
        const providers = { ...current.providers };
        delete providers[command.originalName];
        providers[command.provider.name] = command.provider;
        setSnapshot(snapshotWithConfig({ ...current, providers }));
      }

      if (command.type === "deleteProvider") {
        const current = snapshot()?.config.current ?? emptyConfig("");
        const providers = { ...current.providers };
        delete providers[command.name];
        setSnapshot(snapshotWithConfig({ ...current, providers }));
      }

      if (command.type === "addMcpServer") {
        const current = snapshot()?.config.current ?? emptyConfig("");
        setSnapshot(snapshotWithConfig({
          ...current,
          mcpServers: { ...current.mcpServers, [command.server.name]: command.server },
        }));
      }

      if (command.type === "updateMcpServer") {
        const current = snapshot()?.config.current ?? emptyConfig("");
        const mcpServers = { ...current.mcpServers };
        delete mcpServers[command.originalName];
        mcpServers[command.server.name] = command.server;
        setSnapshot(snapshotWithConfig({ ...current, mcpServers }));
      }

      if (command.type === "deleteMcpServer") {
        const current = snapshot()?.config.current ?? emptyConfig("");
        const mcpServers = { ...current.mcpServers };
        delete mcpServers[command.name];
        setSnapshot(snapshotWithConfig({ ...current, mcpServers }));
      }

      return snapshot() ?? snapshotWithConfig(emptyConfig(""));
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
        }}
      >
        {ui()}
      </AppStateContextProvider>
    );
  });
}
