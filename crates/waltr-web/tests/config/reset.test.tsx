import { cleanup, fireEvent, render, screen, waitFor } from "@solidjs/testing-library";
import { afterEach, describe, expect, test } from "vitest";

import { AppStateProvider } from "../../src/app/state";
import type { ClientCommand, ClientSnapshot, ConfigView } from "../../src/generated/waltr-client";
import type { ClientSnapshotListener, WasmClient } from "../../src/wasm/client";
import { ConfigRouteWithChildren, resetTestHistory } from "../support/routes";
import { claudeConfig, mcpServerView, snapshotWithConfig } from "../support/snapshots";

afterEach(cleanup);

describe("config reset", () => {
  test("opens and cancels the destructive confirmation from every config tab", async () => {
    const harness = renderResetRoute();

    fireEvent.click(await screen.findByRole("button", { name: "Reset configuration" }));
    expect(screen.getByRole("group", { name: "Reset configuration confirmation" }).textContent)
      .toContain("providers, MCP servers, and inference defaults");
    expect(screen.getByText(/Credentials remain untouched/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Cancel reset configuration" }));
    expect(screen.queryByRole("group", { name: "Reset configuration confirmation" })).toBeNull();
    expect(harness.resetCommands()).toHaveLength(0);

    fireEvent.click(screen.getByRole("tab", { name: "MCP Servers" }));
    expect(screen.getByRole("button", { name: "Reset configuration" })).toBeTruthy();
  });

  test("locks confirmation actions and prevents duplicate submissions while pending", async () => {
    const harness = renderResetRoute();
    fireEvent.click(await screen.findByRole("button", { name: "Reset configuration" }));
    fireEvent.click(screen.getByRole("button", { name: "Confirm reset configuration" }));

    await waitFor(() => expect(harness.resetCommands()).toHaveLength(1));
    const confirm = screen.getByRole("button", { name: "Confirm reset configuration" }) as HTMLButtonElement;
    const cancel = screen.getByRole("button", { name: "Cancel reset configuration" }) as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    expect(cancel.disabled).toBe(true);
    fireEvent.click(confirm);
    expect(harness.resetCommands()).toHaveLength(1);
  });

  test("retains shared operation errors and permits retry or cancel after failure", async () => {
    const harness = renderResetRoute();
    fireEvent.click(await screen.findByRole("button", { name: "Reset configuration" }));
    fireEvent.click(screen.getByRole("button", { name: "Confirm reset configuration" }));
    await waitFor(() => expect(harness.resetCommands()).toHaveLength(1));

    harness.settle("reset-1", "failed", "configuration changed; reload and retry");
    await screen.findByText("configuration changed; reload and retry");
    expect(screen.getByRole("group", { name: "Reset configuration confirmation" })).toBeTruthy();
    expect((screen.getByRole("button", { name: "Confirm reset configuration" }) as HTMLButtonElement).disabled).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: "Confirm reset configuration" }));
    await waitFor(() => expect(harness.resetCommands()).toHaveLength(2));
    harness.settle("reset-2", "failed", "server unavailable");
    await screen.findByText("server unavailable");
    fireEvent.click(screen.getByRole("button", { name: "Cancel reset configuration" }));
    expect(screen.queryByText("server unavailable")).toBeNull();
  });

  test("closes only after matching-key success and renders the updated shared snapshot", async () => {
    const harness = renderResetRoute();
    fireEvent.click(await screen.findByRole("button", { name: "Reset configuration" }));
    fireEvent.click(screen.getByRole("button", { name: "Confirm reset configuration" }));
    await waitFor(() => expect(harness.resetCommands()).toEqual([{ type: "resetConfig" }]));

    harness.publishForeignSuccess("foreign-reset");
    await Promise.resolve();
    expect(screen.getByRole("group", { name: "Reset configuration confirmation" })).toBeTruthy();
    expect(screen.queryByText("Configuration reset.")).toBeNull();

    const resetConfig = serverDefaultConfig();
    harness.settle("reset-1", "ready", null, resetConfig);
    await screen.findByText("Configuration reset.");
    expect(screen.queryByRole("group", { name: "Reset configuration confirmation" })).toBeNull();
    expect(screen.getByText("anthropic")).toBeTruthy();
    expect(screen.queryByText("claude")).toBeNull();
  });
});

function renderResetRoute() {
  resetTestHistory("/config/provider");
  let current = snapshotWithConfig(claudeConfig());
  let listener: ClientSnapshotListener | undefined;
  let resetId = 0;
  const commands: ClientCommand[] = [];
  const publish = (snapshot: ClientSnapshot) => {
    current = snapshot;
    listener?.(snapshot);
  };
  const client: WasmClient = {
    endpoint: "https://waltr.example",
    snapshot: () => current,
    start: async () => undefined,
    subscribe: (next) => {
      listener = next;
      return () => { listener = undefined; };
    },
    dispatch: async (command) => {
      commands.push(command);
      if (command.type === "resetConfig") {
        resetId += 1;
        publish(resetSnapshot(current, `reset-${resetId}`, "loading", null));
      }
      return current;
    },
  };

  render(() => (
    <AppStateProvider loadWasmClient={async () => client}>
      <ConfigRouteWithChildren />
    </AppStateProvider>
  ));

  return {
    resetCommands: () => commands.filter((command) => command.type === "resetConfig"),
    settle: (key: string, status: "ready" | "failed", error: string | null, config?: ConfigView) => {
      publish(resetSnapshot(current, key, status, error, config));
    },
    publishForeignSuccess: (key: string) => {
      publish({
        ...current,
        operations: {
          current: {
            ...current.operations.current,
            [key]: { kind: "configReset", status: "ready", error: null, config: serverDefaultConfig() },
          },
        },
      });
    },
  };
}

function resetSnapshot(
  snapshot: ClientSnapshot,
  key: string,
  status: "loading" | "ready" | "failed",
  error: string | null,
  config?: ConfigView,
): ClientSnapshot {
  const resultConfig = status === "ready" ? (config ?? snapshot.config.current) : null;
  return {
    ...snapshot,
    config: {
      ...snapshot.config,
      current: resultConfig ?? snapshot.config.current,
      reset: { status, error, key },
    },
    operations: {
      current: {
        ...snapshot.operations.current,
        [key]: { kind: "configReset", status, error, config: resultConfig },
      },
    },
  };
}

function serverDefaultConfig(): ConfigView {
  const provider = claudeConfig().providers.claude!;
  return {
    etag: "server-default",
    providers: { anthropic: { ...provider, name: "anthropic" } },
    mcpServers: { waltr: mcpServerView({ name: "waltr", kind: "unknown", url: "https://waltr.example/mcp" }) },
    inferenceDefaults: null,
  };
}
