import { fireEvent, screen, waitFor, within } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import type { ClientCommand, SessionStatusView, TimelineEventView } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { SessionsRouteWithRouter, testHistory } from "../support/routes";
import { emptyConfig, readySessionTimeline, sessionView, snapshotWithConfig, snapshotWithSessions, timestamp } from "../support/snapshots";

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
    expect(screen.getByLabelText("Session status summary").textContent).not.toContain("runningTool");
    expect(screen.queryByLabelText("Session breadcrumb")).toBeNull();
    expect(screen.queryByLabelText("Session list")).toBeNull();
    expect(commands.map((command) => command.type)).toEqual([
      "registerSessionWatchInterest",
      "registerSessionTimelineWatchInterest",
    ]);
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
    expect(screen.getByLabelText("Session status summary").textContent).not.toContain("idle");
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

  test("session detail composer dispatches submitted user text", async () => {
    const commands: ClientCommand[] = [];

    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-compose"),
    ]);
    snapshot.sessions.timelines["session-compose"] = readySessionTimeline();

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-compose",
      snapshot,
    });

    fireEvent.input(screen.getByLabelText("Session message"), {
      target: { value: "hello from web" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));

    await waitFor(() => expect(commands).toContainEqual({
      type: "submitUserMessage",
      sessionId: "session-compose",
      text: "hello from web",
    }));
  });

  test("session detail shows assistant pending state while active", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-active", { phase: "streamingProvider", failedMessage: "" }),
    ]);
    snapshot.sessions.timelines["session-active"] = readySessionTimeline();

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-active",
      snapshot,
    });

    expect(screen.getByText("Working...")).toBeTruthy();
  });

  test("session detail shows failed status in important status area", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-failed", { phase: "failed", failedMessage: "provider unavailable" }),
    ]);
    snapshot.sessions.timelines["session-failed"] = readySessionTimeline();

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-failed",
      snapshot,
    });

    expect(screen.getByLabelText("Session activity status").textContent).toContain("provider unavailable");
    expect(screen.getByLabelText("Session status summary").textContent).not.toContain("failed");
  });

  test("session detail shows timeline watch errors in the important status area", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-timeline-error", { phase: "failed", failedMessage: "provider unavailable" }),
    ]);
    const timeline = readySessionTimeline();
    timeline.watch = { status: "failed", error: "timeline disconnected", key: null };
    snapshot.sessions.timelines["session-timeline-error"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-timeline-error",
      snapshot,
    });

    expect(screen.getByLabelText("Session activity status").textContent).toContain("timeline disconnected");
    expect(screen.getByLabelText("Session activity status").textContent).not.toContain("provider unavailable");
    expect(screen.getByLabelText("Session timeline").textContent).not.toContain("timeline disconnected");
  });

  test("session detail renders shell tools with terminal output", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-tools"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 1n,
        itemId: "tool-call-item",
        content: { kind: "toolCall", id: "call-1", name: "shell", arguments_json: JSON.stringify({ command: "pwd", description: "show cwd" }) },
      }),
      timelineEvent({
        cursor: 2n,
        itemId: "tool-progress-info-item",
        parentId: "tool-call-item",
        channel: "tool_progress_info",
        content: { kind: "textDelta", delta: "/workspace\n" },
      }),
      timelineEvent({
        cursor: 3n,
        itemId: "tool-progress-error-item",
        parentId: "tool-call-item",
        channel: "tool_progress_error",
        content: { kind: "textDelta", delta: "warning\n" },
      }),
      timelineEvent({
        cursor: 4n,
        itemId: "tool-result-item",
        content: { kind: "toolResult", id: "call-1", name: "shell", content: "ok", error: false },
      }),
    ];
    snapshot.sessions.timelines["session-tools"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-tools",
      snapshot,
    });

    const shellTool = screen.getByLabelText("Shell tool");
    expect(shellTool.textContent).toContain("show cwd");
    expect(shellTool.textContent).toContain("$ pwd");
    expect(shellTool.textContent).toContain("/workspace");
    expect(shellTool.textContent).toContain("warning");
    expect(screen.queryByText('shell(command: "pwd", description: "show cwd")')).toBeNull();
    expect(screen.queryByText(/loading/)).toBeNull();
  });

  test("session detail shows loading shell tools", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-tool-loading"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 1n,
        itemId: "tool-call-item",
        content: { kind: "toolCall", id: "call-1", name: "shell", arguments_json: JSON.stringify({ command: "pwd" }) },
      }),
    ];
    snapshot.sessions.timelines["session-tool-loading"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-tool-loading",
      snapshot,
    });

    const shellTool = screen.getByLabelText("Shell tool");
    expect(shellTool.textContent).toContain("Shell command");
    expect(shellTool.textContent).toContain("$ pwd");
    expect(shellTool.textContent).toContain("loading...");
  });

  test("session detail renders apply_patch input as a diff", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-patch"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 1n,
        itemId: "patch-call-item",
        content: {
          kind: "toolCall",
          id: "patch-call",
          name: "apply_patch",
          arguments_json: JSON.stringify({
            patchText: "*** Begin Patch\n*** Update File: src/app.ts\n@@\n-old\n+new\n*** End Patch",
          }),
        },
      }),
      timelineEvent({
        cursor: 2n,
        itemId: "patch-result-item",
        content: { kind: "toolResult", id: "patch-call", name: "apply_patch", content: "Success. Updated the following files:\nM src/app.ts", error: false },
      }),
    ];
    snapshot.sessions.timelines["session-patch"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-patch",
      snapshot,
    });

    const patchTool = screen.getByLabelText("Patch tool");
    expect(within(patchTool).getByLabelText("Files modified").textContent).toContain("M src/app.ts");
    expect(within(patchTool).getByLabelText("Patch diff").textContent).toContain("*** Update File: src/app.ts");
    expect(within(patchTool).getByLabelText("Patch diff").textContent).toContain("-old");
    expect(within(patchTool).getByLabelText("Patch diff").textContent).toContain("+new");
    expect(patchTool.textContent).toContain("Success. Updated the following files:");
  });

  test("session detail renders edit input as a before and after diff", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-edit"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 1n,
        itemId: "edit-call-item",
        content: {
          kind: "toolCall",
          id: "edit-call",
          name: "edit",
          arguments_json: JSON.stringify({
            filePath: "src/app.ts",
            oldString: "const label = 'old';",
            newString: "const label = 'new';",
          }),
        },
      }),
      timelineEvent({
        cursor: 2n,
        itemId: "edit-result-item",
        content: { kind: "toolResult", id: "edit-call", name: "edit", content: "Success. Updated src/app.ts", error: false },
      }),
    ];
    snapshot.sessions.timelines["session-edit"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-edit",
      snapshot,
    });

    const editTool = screen.getByLabelText("Edit tool");
    expect(within(editTool).getByLabelText("Files modified").textContent).toContain("M src/app.ts");
    expect(within(editTool).getByLabelText("Diff for src/app.ts").textContent).toContain("- const label = 'old';");
    expect(within(editTool).getByLabelText("Diff for src/app.ts").textContent).toContain("+ const label = 'new';");
    expect(editTool.textContent).not.toContain("Success. Updated src/app.ts");
  });

  test("session detail renders resolved external requests as compact results", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-permission"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 1n,
        itemId: "external-request:permission-1",
        kind: "externalRequestOpened",
        content: {
          kind: "externalRequest",
          request: {
            id: "permission-1",
            kind: 1,
            payloadJson: JSON.stringify({ tool_call: { name: "read", arguments: { filePath: "../outside/file.txt" } } }),
          },
        },
      }),
      timelineEvent({
        cursor: 2n,
        itemId: "external-request:permission-1",
        kind: "externalRequestResolved",
        status: "resolved",
        content: {
          kind: "externalRequest",
          request: {
            id: "permission-1",
            kind: 1,
            payloadJson: JSON.stringify({ tool_call: { name: "read", arguments: { filePath: "../outside/file.txt" } } }),
          },
        },
      }),
    ];
    snapshot.sessions.timelines["session-permission"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-permission",
      snapshot,
    });

    expect(screen.getByText("permission: answered")).toBeTruthy();
    expect(screen.queryByLabelText("Permission prompt")).toBeNull();
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

function sessionWithRuntime(id: string, status: SessionStatusView = { phase: "idle", failedMessage: "" }) {
  return sessionView({
    id,
    status,
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

function timelineEvent(overrides: Partial<TimelineEventView> = {}): TimelineEventView {
  return {
    cursor: 1n,
    sessionId: "session-tools",
    turn: 1n,
    itemId: "item-1",
    parentId: null,
    kind: "itemUpdated",
    channel: null,
    content: { kind: "empty" },
    status: null,
    metadataJson: "{}",
    ...overrides,
  };
}
