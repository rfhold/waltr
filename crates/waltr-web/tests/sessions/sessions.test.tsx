import { fireEvent, screen, waitFor, within } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import type { ClientCommand } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { SessionsRouteWithRouter, testHistory } from "../support/routes";
import { emptyConfig, sessionView, snapshotWithConfig, snapshotWithSessions, timestamp } from "../support/snapshots";

describe("sessions route", () => {
  test("lists sessions sorted by most recently updated", () => {
    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions",
      snapshot: snapshotWithSessions([
        sessionView({ id: "older", updatedAt: timestamp(1_700_000_001) }),
        sessionView({ id: "newer", updatedAt: timestamp(1_700_000_010) }),
        sessionView({ id: "missing-time", updatedAt: null }),
      ]),
    });

    const rows = screen.getAllByRole("button", { name: /Open session/ });
    expect(rows.map((row) => row.textContent)).toEqual([
      expect.stringContaining("newer"),
      expect.stringContaining("older"),
      expect.stringContaining("missing-time"),
    ]);
    expect(screen.queryByLabelText("Session breadcrumb")).toBeNull();
  });

  test("selecting a session routes to its detail view", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions",
      snapshot: snapshotWithSessions([
        sessionView({ id: "session-b", status: { phase: "runningTool", failedMessage: "" } }),
      ]),
    });

    fireEvent.click(screen.getByLabelText("Open session session-b"));

    await waitFor(() => expect(testHistory().get()).toBe("/sessions/session-b"));
    expect(screen.getByLabelText("Session status summary").textContent).toContain("Status runningTool");
    expect(screen.queryByLabelText("Session breadcrumb")).toBeNull();
    expect(screen.queryByLabelText("Session list")).toBeNull();
    expect(commands.map((command) => command.type)).toEqual(["registerSessionWatchInterest"]);
  });

  test("creating a session routes to the created session", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions",
      snapshot: snapshotWithSessions([]),
    });

    fireEvent.click(screen.getByText("Create session"));

    await waitFor(() => expect(testHistory().get()).toBe("/sessions/session-1"));
    expect(screen.getByLabelText("Session status summary").textContent).toContain("Status idle");
    expect(commands.map((command) => command.type)).toContain("createSession");
  });

  test("shows session create failures without routing", async () => {
    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions",
      sessionCreateError: "session service unavailable",
      snapshot: snapshotWithSessions([]),
    });

    fireEvent.click(screen.getByText("Create session"));

    await waitFor(() => expect(screen.getByText("session service unavailable")).toBeTruthy());
    expect(testHistory().get()).toBe("/sessions");
  });

  test("deleting a session from the list dispatches deleteSession and removes it", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions",
      snapshot: snapshotWithSessions([
        sessionView({ id: "session-delete" }),
        sessionView({ id: "session-keep" }),
      ]),
    });

    fireEvent.click(screen.getByLabelText("Delete session session-delete"));
    fireEvent.click(screen.getByLabelText("Confirm delete session session-delete"));

    await waitFor(() => expect(screen.queryByText("session-delete")).toBeNull());
    expect(screen.getByText("session-keep")).toBeTruthy();
    expect(commands).toContainEqual({ type: "deleteSession", id: "session-delete" });
  });

  test("deleting the current detail session routes back to the sessions list", async () => {
    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-current",
      snapshot: snapshotWithSessions([
        sessionView({ id: "session-current" }),
      ]),
    });

    const detail = screen.getByRole("region", { name: "Session detail" });
    fireEvent.click(within(detail).getByLabelText("Delete session session-current"));
    fireEvent.click(within(detail).getByLabelText("Confirm delete session session-current"));

    await waitFor(() => expect(testHistory().get()).toBe("/sessions"));
    expect(screen.queryByText("session-current")).toBeNull();
  });

  test("delete failure keeps the session visible and shows the error", async () => {
    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions",
      sessionDeleteError: "session delete unavailable",
      snapshot: snapshotWithSessions([
        sessionView({ id: "session-fail" }),
      ]),
    });

    fireEvent.click(screen.getByLabelText("Delete session session-fail"));
    fireEvent.click(screen.getByLabelText("Confirm delete session session-fail"));

    await waitFor(() => expect(screen.getByText("session delete unavailable")).toBeTruthy());
    expect(screen.getByText("session-fail")).toBeTruthy();
  });

  test("delete button click does not navigate through the row action", async () => {
    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions",
      snapshot: snapshotWithSessions([
        sessionView({ id: "session-stay" }),
      ]),
    });

    fireEvent.click(screen.getByLabelText("Delete session session-stay"));

    expect(testHistory().get()).toBe("/sessions");
    expect(screen.getByLabelText("Confirm delete session session-stay")).toBeTruthy();
  });

  test("provider editor dispatches session runtime metadata update", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithConfig({
      ...emptyConfig("v1"),
      providers: {
        openai: provider("OpenAI", ["gpt-4o"]),
        anthropic: provider("Anthropic", ["claude-4"]),
      },
    });
    snapshot.sessions.current = [sessionWithRuntime("runtime-session")];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/runtime-session",
      snapshot,
    });

    fireEvent.click(screen.getByLabelText("Edit provider/model"));
    fireEvent.click(screen.getByText("anthropic"));
    fireEvent.click(screen.getByText("Save runtime"));

    await waitFor(() => expect(commands).toContainEqual({
      type: "updateSessionRuntimeConfig",
      id: "runtime-session",
      providerKey: "anthropic",
      modelId: "claude-4",
      enabledMcpServerKeys: ["git"],
    }));
  });

  test("mcp editor dispatches selected session runtime servers", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithConfig({
      ...emptyConfig("v1"),
      providers: { openai: provider("OpenAI", ["gpt-4o"]) },
      mcpServers: {
        git: { name: "git", kind: "stdio", command: "git-mcp", url: "" },
        files: { name: "files", kind: "stdio", command: "files-mcp", url: "" },
      },
    });
    snapshot.sessions.current = [sessionWithRuntime("runtime-session")];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/runtime-session",
      snapshot,
    });

    fireEvent.click(screen.getByLabelText("Edit MCP"));
    fireEvent.click(screen.getByText("files"));
    fireEvent.click(screen.getByText("Save MCP"));

    await waitFor(() => expect(commands).toContainEqual({
      type: "updateSessionRuntimeConfig",
      id: "runtime-session",
      providerKey: "openai",
      modelId: "gpt-4o",
      enabledMcpServerKeys: ["git", "files"],
    }));
  });
});

function provider(name: string, modelIds: string[]) {
  return {
    kind: "openAi" as const,
    name,
    url: "https://example.invalid/v1",
    headers: {},
    models: Object.fromEntries(modelIds.map((id) => [id, {
      name: id,
      kind: "llm" as const,
      options: { temperature: null, topP: null, topK: null, minP: null, presencePenalty: null, repetitionPenalty: null },
      limits: { context: null, output: null },
      capabilities: { tools: true, image: false },
    }])),
  };
}

function sessionWithRuntime(id: string) {
  return sessionView({
    id,
    runtimeConfig: {
      provider: {
        key: "openai",
        kind: "openAi",
        url: "https://example.invalid/v1",
        modelId: "gpt-4o",
        matchStatus: "configured",
      },
      mcpServers: [{
        key: "git",
        kind: "stdio",
        command: "git-mcp",
        url: "",
        matchStatus: "configured",
      }],
    },
  });
}
