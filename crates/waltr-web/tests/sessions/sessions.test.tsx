import { fireEvent, screen, waitFor, within } from "@solidjs/testing-library";
import { describe, expect, test, vi } from "vitest";

import type { ClientCommand, ClientSnapshot, SessionTemplateOptionView, SessionStatusView, TimelineEventView } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { SessionsRouteWithRouter, testHistory } from "../support/routes";
import { emptyConfig, readySessionTimeline, sessionMetricsView, sessionView, snapshotWithConfig, snapshotWithSessions, timestamp } from "../support/snapshots";

const RESOURCE_TEST_DEBOUNCE_MS = 130;

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

  test("session detail metrics use full context tokens with cache", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-cache"),
    ]);
    snapshot.sessions.current[0]!.metrics = {
      ...sessionMetricsView(),
      inputTokens: 100n,
      outputTokens: 10n,
      cacheReadTokens: 100n,
      cacheWriteTokens: 20n,
      totalTokens: 110n,
      totalInputTokens: 220n,
      contextTokens: 230n,
    };
    snapshot.sessions.timelines["session-cache"] = readySessionTimeline();
    snapshot.sessions.timelines["session-cache"].events = [
      timelineEvent({
        cursor: 1n,
        sessionId: "session-cache",
        itemId: "metrics:1",
        channel: "metrics",
        content: {
          kind: "json",
          value_json: JSON.stringify({
            kind: "turn_metrics",
            usage: {
              inputTokens: 100,
              outputTokens: 10,
              cacheReadTokens: 100,
              cacheWriteTokens: 20,
              totalTokens: 110,
              totalInputTokens: 220,
              contextTokens: 230,
            },
          }),
        },
      }),
    ];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-cache",
      snapshot,
    });

    expect(screen.getByLabelText("Session token and cost totals").textContent).toContain("Ctx 230");
    expect(screen.getByLabelText("Session token and cost totals").textContent).toContain("Cache 45.5% / 9.1%");
    expect(screen.getByLabelText("Turn token metrics").textContent).toContain("230 tok");
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

  test("session detail resource autocomplete searches only valid active tokens", async () => {
    vi.useFakeTimers();
    try {
      const commands: ClientCommand[] = [];
      const snapshot = snapshotWithSessions([
        sessionWithRuntime("session-resource-rules"),
      ]);
      snapshot.sessions.timelines["session-resource-rules"] = readySessionTimeline();

      renderWithAppState(() => <SessionsRouteWithRouter />, {
        commands,
        routePath: "/sessions/session-resource-rules",
        snapshot,
      });

      const input = screen.getByLabelText("Session message") as HTMLTextAreaElement;
      await typeComposerText(input, "@");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      expect(templateSearchCommands(commands)).toEqual([]);

      await typeComposerText(input, "foo@src");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      expect(templateSearchCommands(commands)).toEqual([]);

      await typeComposerText(input, "@ src");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      expect(templateSearchCommands(commands)).toEqual([]);

      await typeComposerText(input, "read @src");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);

      await waitFor(() => expect(templateSearchCommands(commands)).toHaveLength(1));
      expect(templateSearchCommands(commands)[0]).toMatchObject({
        sessionId: "session-resource-rules",
        query: "src",
        limit: 12,
        kinds: ["file", "tool"],
      });
    } finally {
      vi.useRealTimers();
    }
  });

  test("session detail prompt autocomplete searches only first-line slash tokens", async () => {
    vi.useFakeTimers();
    try {
      const commands: ClientCommand[] = [];
      const snapshot = snapshotWithSessions([
        sessionWithRuntime("session-prompt-rules"),
      ]);
      snapshot.sessions.timelines["session-prompt-rules"] = readySessionTimeline();

      renderWithAppState(() => <SessionsRouteWithRouter />, {
        commands,
        routePath: "/sessions/session-prompt-rules",
        snapshot,
      });

      const input = screen.getByLabelText("Session message") as HTMLTextAreaElement;
      await typeComposerText(input, "/");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      expect(templateSearchCommands(commands)).toEqual([]);

      await typeComposerText(input, "note/skill:doc");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      expect(templateSearchCommands(commands)).toEqual([]);

      await typeComposerText(input, "first\n/skill:doc");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      expect(templateSearchCommands(commands)).toEqual([]);

      await typeComposerText(input, "/skill:doc");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);

      await waitFor(() => expect(templateSearchCommands(commands)).toHaveLength(1));
      expect(templateSearchCommands(commands)[0]).toMatchObject({
        sessionId: "session-prompt-rules",
        query: "skill:doc",
        limit: 12,
        kinds: ["prompt"],
        target: "prompts",
      });
    } finally {
      vi.useRealTimers();
    }
  });

  test("session detail resource autocomplete renders delayed unary results", async () => {
    vi.useFakeTimers();
    try {
      const commands: ClientCommand[] = [];
      let setSnapshot!: (snapshot: ClientSnapshot) => void;
      const snapshot = snapshotWithSessions([
        sessionWithRuntime("session-resource-delayed"),
      ]);
      snapshot.sessions.timelines["session-resource-delayed"] = readySessionTimeline();

      renderWithAppState(() => <SessionsRouteWithRouter />, {
        commands,
        routePath: "/sessions/session-resource-delayed",
        snapshot,
        onSnapshotSetter: (setter) => {
          setSnapshot = setter;
        },
      });

      await typeComposerText(screen.getByLabelText("Session message") as HTMLTextAreaElement, "read @src");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      await waitFor(() => expect(templateSearchCommands(commands)).toHaveLength(1));
      expect(screen.queryByRole("listbox", { name: "Resource suggestions" })).toBeNull();

      publishTemplateSearch(snapshot, setSnapshot, latestTemplateSearchCommand(commands), "ready", [
        { kind: "file", label: "src/lib.rs", detail: "file", template: "@src/lib.rs" },
      ]);

      await waitFor(() => expect(screen.getByRole("option", { name: /src\/lib\.rs/ })).toBeTruthy());
    } finally {
      vi.useRealTimers();
    }
  });

  test("session detail resource autocomplete accepts keyboard selection", async () => {
    vi.useFakeTimers();
    try {
      const commands: ClientCommand[] = [];
      let setSnapshot!: (snapshot: ClientSnapshot) => void;
      const snapshot = snapshotWithSessions([
        sessionWithRuntime("session-resource-keyboard"),
      ]);
      snapshot.sessions.timelines["session-resource-keyboard"] = readySessionTimeline();

      renderWithAppState(() => <SessionsRouteWithRouter />, {
        commands,
        routePath: "/sessions/session-resource-keyboard",
        snapshot,
        onSnapshotSetter: (setter) => {
          setSnapshot = setter;
        },
      });

      const input = screen.getByLabelText("Session message") as HTMLTextAreaElement;
      await typeComposerText(input, "inspect @sr please");
      input.setSelectionRange("inspect @sr".length, "inspect @sr".length);
      fireEvent.keyUp(input, { key: "r" });
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      await waitFor(() => expect(templateSearchCommands(commands)).toHaveLength(1));
      publishTemplateSearch(snapshot, setSnapshot, latestTemplateSearchCommand(commands), "ready", [
        { kind: "file", label: "src/lib.rs", detail: "file", template: "@src/lib.rs" },
        { kind: "tool", label: "read", detail: "Read files", template: '@tool(read, {"filePath":""})' },
      ]);

      await waitFor(() => expect(screen.getByRole("listbox", { name: "Resource suggestions" })).toBeTruthy());
      expect(screen.getByRole("option", { name: /src\/lib\.rs/ }).getAttribute("aria-selected")).toBe("true");

      fireEvent.keyDown(input, { key: "ArrowDown" });
      expect(screen.getByRole("option", { name: /read/ }).getAttribute("aria-selected")).toBe("true");

      const selectedOption = screen.getByRole("option", { name: /read/ }) as HTMLButtonElement;
      selectedOption.focus();
      fireEvent.keyDown(selectedOption, { key: "Enter" });

      await waitFor(() => expect(input.value).toBe('inspect @tool(read, {"filePath":""}) please'));
      expect(screen.queryByRole("listbox", { name: "Resource suggestions" })).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  test("session detail resource autocomplete does not reuse trailing tokens after caret moves", async () => {
    vi.useFakeTimers();
    try {
      const commands: ClientCommand[] = [];
      let setSnapshot!: (snapshot: ClientSnapshot) => void;
      const snapshot = snapshotWithSessions([
        sessionWithRuntime("session-resource-caret"),
      ]);
      snapshot.sessions.timelines["session-resource-caret"] = readySessionTimeline();

      renderWithAppState(() => <SessionsRouteWithRouter />, {
        commands,
        routePath: "/sessions/session-resource-caret",
        snapshot,
        onSnapshotSetter: (setter) => {
          setSnapshot = setter;
        },
      });

      const input = screen.getByLabelText("Session message") as HTMLTextAreaElement;
      await typeComposerText(input, "read @src");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      await waitFor(() => expect(templateSearchCommands(commands)).toHaveLength(1));
      publishTemplateSearch(snapshot, setSnapshot, latestTemplateSearchCommand(commands), "ready", [
        { kind: "file", label: "src/lib.rs", detail: "file", template: "@src/lib.rs" },
      ]);
      await waitFor(() => expect(screen.getByRole("listbox", { name: "Resource suggestions" })).toBeTruthy());

      input.setSelectionRange(0, 0);
      fireEvent.keyUp(input, { key: "ArrowLeft" });
      await waitFor(() => expect(screen.queryByRole("listbox", { name: "Resource suggestions" })).toBeNull());

      fireEvent.keyDown(input, { key: "Enter" });
      expect(input.value).toBe("read @src");
    } finally {
      vi.useRealTimers();
    }
  });

  test("session detail resource autocomplete accepts clicked options and ignores stale responses", async () => {
    vi.useFakeTimers();
    try {
      const commands: ClientCommand[] = [];
      let setSnapshot!: (snapshot: ClientSnapshot) => void;
      const snapshot = snapshotWithSessions([
        sessionWithRuntime("session-resource-click"),
      ]);
      snapshot.sessions.timelines["session-resource-click"] = readySessionTimeline();

      renderWithAppState(() => <SessionsRouteWithRouter />, {
        commands,
        routePath: "/sessions/session-resource-click",
        snapshot,
        onSnapshotSetter: (setter) => {
          setSnapshot = setter;
        },
      });

      const input = screen.getByLabelText("Session message") as HTMLTextAreaElement;
      await typeComposerText(input, "open @s");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      await waitFor(() => expect(templateSearchCommands(commands)).toHaveLength(1));
      const staleCommand = latestTemplateSearchCommand(commands);

      await typeComposerText(input, "open @src");
      await vi.advanceTimersByTimeAsync(RESOURCE_TEST_DEBOUNCE_MS);
      await waitFor(() => expect(templateSearchCommands(commands)).toHaveLength(2));
      publishTemplateSearch(snapshot, setSnapshot, staleCommand, "ready", [
        { kind: "file", label: "stale", detail: "old", template: "@stale" },
      ]);
      expect(screen.queryByText("stale")).toBeNull();

      publishTemplateSearch(snapshot, setSnapshot, latestTemplateSearchCommand(commands), "ready", [
        { kind: "file", label: "src/main.rs", detail: "file", template: "@src/main.rs" },
      ]);

      fireEvent.click(await screen.findByRole("option", { name: /src\/main\.rs/ }));
      await waitFor(() => expect(input.value).toBe("open @src/main.rs"));
    } finally {
      vi.useRealTimers();
    }
  });

  test("session detail composer highlights resource-looking text", async () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-resource-highlight"),
    ]);
    snapshot.sessions.timelines["session-resource-highlight"] = readySessionTimeline();

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-resource-highlight",
      snapshot,
    });

    await typeComposerText(screen.getByLabelText("Session message") as HTMLTextAreaElement, "read @src/lib.rs");

    const token = document.querySelector(".sessions__composer-resource-token");
    expect(token?.textContent).toBe("@src/lib.rs");
  });

  test("session detail composer highlights first-line prompt text", async () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-prompt-highlight"),
    ]);
    snapshot.sessions.timelines["session-prompt-highlight"] = readySessionTimeline();

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-prompt-highlight",
      snapshot,
    });

    await typeComposerText(screen.getByLabelText("Session message") as HTMLTextAreaElement, "/skill:documenting\n/skill:later");

    const promptTokens = Array.from(document.querySelectorAll(".sessions__composer-prompt-token"));
    expect(promptTokens.map((token) => token.textContent)).toEqual(["/skill:documenting"]);
  });

  test("session detail composer keeps draft and shows operation failures", async () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-compose-fail"),
    ]);
    snapshot.sessions.timelines["session-compose-fail"] = readySessionTimeline();

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-compose-fail",
      snapshot,
      submitUserMessageError: "submit rejected",
    });

    const input = screen.getByLabelText("Session message") as HTMLTextAreaElement;
    fireEvent.input(input, { target: { value: "keep me" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));

    await waitFor(() => expect(screen.getByText("submit rejected")).toBeTruthy());
    expect(input.value).toBe("keep me");
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

  test("session detail treats suspended sessions as waiting", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-suspended", { phase: "suspended", failedMessage: "" }),
    ]);
    snapshot.sessions.timelines["session-suspended"] = readySessionTimeline();

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-suspended",
      snapshot,
    });

    expect(screen.queryByText("Working...")).toBeNull();
    expect(screen.queryByRole("button", { name: "Stop" })).toBeNull();
    const stopButton = screen.getByText("Stop").closest("button");
    expect(stopButton?.hasAttribute("disabled")).toBe(true);
    expect(stopButton?.getAttribute("aria-hidden")).toBe("true");
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

  test("session detail keeps initial timeline replay pinned to the bottom", async () => {
    let publishSnapshot!: (snapshot: ClientSnapshot) => void;
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-scroll"),
    ]);
    snapshot.sessions.timelines["session-scroll"] = readySessionTimeline();

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-scroll",
      snapshot,
      onSnapshotSetter: (setSnapshot) => {
        publishSnapshot = setSnapshot;
      },
    });

    const timelineRegion = screen.getByLabelText("Session timeline");
    setScrollGeometry(timelineRegion, { scrollTop: 0, scrollHeight: 500, clientHeight: 100 });
    publishSnapshot(snapshotWithTimelineEvents(snapshot, "session-scroll", [
      timelineEvent({ cursor: 1n, sessionId: "session-scroll", itemId: "first", channel: "assistant_text", content: { kind: "textSnapshot", text: "first" } }),
    ]));

    await waitForTimelineScroll();
    expect(timelineRegion.scrollTop).toBe(500);

    setScrollGeometry(timelineRegion, { scrollTop: timelineRegion.scrollTop, scrollHeight: 900, clientHeight: 100 });
    publishSnapshot(snapshotWithTimelineEvents(snapshot, "session-scroll", [
      timelineEvent({ cursor: 1n, sessionId: "session-scroll", itemId: "first", channel: "assistant_text", content: { kind: "textSnapshot", text: "first" } }),
      timelineEvent({ cursor: 2n, sessionId: "session-scroll", itemId: "second", channel: "assistant_text", content: { kind: "textSnapshot", text: "second" } }),
    ]));

    await waitForTimelineScroll();
    expect(timelineRegion.scrollTop).toBe(900);
  });

  test("session detail sticks to bottom during streaming only when the user is at the bottom", async () => {
    let publishSnapshot!: (snapshot: ClientSnapshot) => void;
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-stream"),
    ]);
    snapshot.sessions.timelines["session-stream"] = readySessionTimeline();
    snapshot.sessions.timelines["session-stream"].events = [
      timelineEvent({ cursor: 1n, sessionId: "session-stream", itemId: "first", channel: "assistant_text", content: { kind: "textSnapshot", text: "first" } }),
    ];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-stream",
      snapshot,
      onSnapshotSetter: (setSnapshot) => {
        publishSnapshot = setSnapshot;
      },
    });

    const timelineRegion = screen.getByLabelText("Session timeline");
    setScrollGeometry(timelineRegion, { scrollTop: 500, scrollHeight: 900, clientHeight: 100 });
    fireEvent.wheel(timelineRegion);
    fireEvent.scroll(timelineRegion);
    setScrollGeometry(timelineRegion, { scrollTop: 500, scrollHeight: 1100, clientHeight: 100 });
    publishSnapshot(snapshotWithTimelineEvents(snapshot, "session-stream", [
      ...snapshot.sessions.timelines["session-stream"].events,
      timelineEvent({ cursor: 2n, sessionId: "session-stream", itemId: "second", channel: "assistant_text", content: { kind: "textSnapshot", text: "second" } }),
    ]));

    await waitForTimelineScroll();
    expect(timelineRegion.scrollTop).toBe(500);

    setScrollGeometry(timelineRegion, { scrollTop: 1000, scrollHeight: 1100, clientHeight: 100 });
    fireEvent.wheel(timelineRegion);
    fireEvent.scroll(timelineRegion);
    setScrollGeometry(timelineRegion, { scrollTop: 1000, scrollHeight: 1400, clientHeight: 100 });
    fireEvent.scroll(timelineRegion);
    publishSnapshot(snapshotWithTimelineEvents(snapshot, "session-stream", [
      ...snapshot.sessions.timelines["session-stream"].events,
      timelineEvent({ cursor: 2n, sessionId: "session-stream", itemId: "second", channel: "assistant_text", content: { kind: "textSnapshot", text: "second" } }),
      timelineEvent({ cursor: 3n, sessionId: "session-stream", itemId: "third", channel: "assistant_text", content: { kind: "textSnapshot", text: "third" } }),
    ]));

    await waitForTimelineScroll();
    expect(timelineRegion.scrollTop).toBe(1400);
  });

  test("session detail loads older history when scrolling near the top", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-history"),
    ]);
    snapshot.sessions.timelines["session-history"] = readySessionTimeline();
    snapshot.sessions.timelines["session-history"].events = [
      timelineEvent({ cursor: 10n, sessionId: "session-history", itemId: "first", channel: "assistant_text", content: { kind: "textSnapshot", text: "first" } }),
    ];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-history",
      snapshot,
    });
    await waitFor(() => expect(commands.some((command) => command.type === "registerSessionTimelineWatchInterest")).toBe(true));
    commands.length = 0;

    const timelineRegion = screen.getByLabelText("Session timeline");
    setScrollGeometry(timelineRegion, { scrollTop: 280, scrollHeight: 1500, clientHeight: 400 });
    fireEvent.wheel(timelineRegion);
    fireEvent.scroll(timelineRegion);

    await waitFor(() => expect(commands).toContainEqual(expect.objectContaining({
      type: "requestSessionTimeline",
      sessionId: "session-history",
      direction: "backward",
      cursor: 10n,
      limit: 64,
    })));
  });

  test("session detail loads older history after a fast wheel to the top", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-fast-top"),
    ]);
    snapshot.sessions.timelines["session-fast-top"] = readySessionTimeline();
    snapshot.sessions.timelines["session-fast-top"].events = [
      timelineEvent({ cursor: 10n, sessionId: "session-fast-top", itemId: "first", channel: "assistant_text", content: { kind: "textSnapshot", text: "first" } }),
    ];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-fast-top",
      snapshot,
    });
    await waitFor(() => expect(commands.some((command) => command.type === "registerSessionTimelineWatchInterest")).toBe(true));
    commands.length = 0;

    const timelineRegion = screen.getByLabelText("Session timeline");
    setScrollGeometry(timelineRegion, { scrollTop: 0, scrollHeight: 1500, clientHeight: 400 });
    fireEvent.wheel(timelineRegion);

    await waitForTimelineScroll();
    await waitFor(() => expect(commands).toContainEqual(expect.objectContaining({
      type: "requestSessionTimeline",
      sessionId: "session-fast-top",
      direction: "backward",
      cursor: 10n,
      limit: 64,
    })));
  });

  test("session detail auto-loads older history when initial content is not scrollable", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-short-history"),
    ]);
    snapshot.sessions.timelines["session-short-history"] = readySessionTimeline();
    snapshot.sessions.timelines["session-short-history"].events = [
      timelineEvent({ cursor: 10n, sessionId: "session-short-history", itemId: "first", channel: "assistant_text", content: { kind: "textSnapshot", text: "first" } }),
    ];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-short-history",
      snapshot,
    });

    const timelineRegion = screen.getByLabelText("Session timeline");
    setScrollGeometry(timelineRegion, { scrollTop: 0, scrollHeight: 180, clientHeight: 300 });
    await waitForTimelineScroll();

    await waitFor(() => expect(commands).toContainEqual(expect.objectContaining({
      type: "requestSessionTimeline",
      sessionId: "session-short-history",
      direction: "backward",
      cursor: 10n,
      limit: 64,
    })));
  });

  test("session detail preserves the visible item when older history loads above", async () => {
    const commands: ClientCommand[] = [];
    let publishSnapshot!: (snapshot: ClientSnapshot) => void;
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-prepend"),
    ]);
    snapshot.sessions.timelines["session-prepend"] = readySessionTimeline();
    snapshot.sessions.timelines["session-prepend"].events = [
      timelineEvent({ cursor: 10n, sessionId: "session-prepend", itemId: "anchor", channel: "assistant_text", content: { kind: "textSnapshot", text: "anchor" } }),
      timelineEvent({ cursor: 11n, sessionId: "session-prepend", itemId: "below", channel: "assistant_text", content: { kind: "textSnapshot", text: "below" } }),
    ];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-prepend",
      snapshot,
      onSnapshotSetter: (setSnapshot) => {
        publishSnapshot = setSnapshot;
      },
    });
    await waitFor(() => expect(commands.some((command) => command.type === "registerSessionTimelineWatchInterest")).toBe(true));
    commands.length = 0;

    const timelineRegion = screen.getByLabelText("Session timeline");
    setScrollGeometry(timelineRegion, { scrollTop: 80, scrollHeight: 1000, clientHeight: 300 });
    setElementRect(timelineRegion, { top: 0, bottom: 300 });
    setElementRect(timelineItemElement("anchor"), { top: 40, bottom: 100 });
    fireEvent.wheel(timelineRegion);
    fireEvent.scroll(timelineRegion);

    await waitFor(() => expect(commands.filter((command) => command.type === "requestSessionTimeline")).toHaveLength(1));
    setScrollGeometry(timelineRegion, { scrollTop: 80, scrollHeight: 1300, clientHeight: 300 });
    publishSnapshot(snapshotWithTimelineEvents(snapshot, "session-prepend", [
      timelineEvent({ cursor: 1n, sessionId: "session-prepend", itemId: "older", channel: "assistant_text", content: { kind: "textSnapshot", text: "older" } }),
      ...snapshot.sessions.timelines["session-prepend"].events,
    ]));
    setElementRect(timelineItemElement("anchor"), { top: 240, bottom: 300 });

    await waitForTimelineScroll();
    expect(timelineRegion.scrollTop).toBe(120);
    expect(commands.filter((command) => command.type === "requestSessionTimeline")).toHaveLength(1);
  });

  test("session detail lets virtua preserve prepended history without chaining loads", async () => {
    const commands: ClientCommand[] = [];
    let publishSnapshot!: (snapshot: ClientSnapshot) => void;
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-top-chain"),
    ]);
    snapshot.sessions.timelines["session-top-chain"] = readySessionTimeline();
    snapshot.sessions.timelines["session-top-chain"].events = [
      timelineEvent({ cursor: 10n, sessionId: "session-top-chain", itemId: "anchor", channel: "assistant_text", content: { kind: "textSnapshot", text: "anchor" } }),
      timelineEvent({ cursor: 11n, sessionId: "session-top-chain", itemId: "below", channel: "assistant_text", content: { kind: "textSnapshot", text: "below" } }),
    ];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-top-chain",
      snapshot,
      onSnapshotSetter: (setSnapshot) => {
        publishSnapshot = setSnapshot;
      },
    });
    await waitFor(() => expect(commands.some((command) => command.type === "registerSessionTimelineWatchInterest")).toBe(true));
    commands.length = 0;

    const timelineRegion = screen.getByLabelText("Session timeline");
    setScrollGeometry(timelineRegion, { scrollTop: 40, scrollHeight: 1000, clientHeight: 300 });
    setElementRect(timelineRegion, { top: 0, bottom: 300 });
    setElementRect(timelineItemElement("anchor"), { top: 30, bottom: 90 });
    fireEvent.wheel(timelineRegion);
    fireEvent.scroll(timelineRegion);

    await waitFor(() => expect(commands.filter((command) => command.type === "requestSessionTimeline")).toHaveLength(1));
    expect(commands[0]).toEqual(expect.objectContaining({
      type: "requestSessionTimeline",
      cursor: 10n,
    }));

    setScrollGeometry(timelineRegion, { scrollTop: 40, scrollHeight: 1100, clientHeight: 300 });
    publishSnapshot(snapshotWithTimelineEvents(snapshot, "session-top-chain", [
      timelineEvent({ cursor: 9n, sessionId: "session-top-chain", itemId: "older", channel: "assistant_text", content: { kind: "textSnapshot", text: "older" } }),
      ...snapshot.sessions.timelines["session-top-chain"].events,
    ]));
    setElementRect(timelineItemElement("anchor"), { top: 70, bottom: 130 });

    await waitForTimelineScroll();
    expect(timelineRegion.scrollTop).toBe(80);
    expect(commands.filter((command) => command.type === "requestSessionTimeline")).toHaveLength(1);
  });

  test("session detail renders assistant markdown", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-markdown"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 1n,
        sessionId: "session-markdown",
        itemId: "assistant-item",
        channel: "assistant_text",
        content: {
          kind: "textSnapshot",
          text: "## Summary\n\n- Run `pnpm test`\n\n```bash\npnpm test\n```\n\n```src/app.ts\nconst label = 'ok';\n```",
        },
      }),
    ];
    snapshot.sessions.timelines["session-markdown"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-markdown",
      snapshot,
    });

    const timelineRegion = screen.getByLabelText("Session timeline");
    expect(within(timelineRegion).getByRole("heading", { name: "Summary" })).toBeTruthy();
    expect(timelineRegion.querySelector(".sessions__code-block")?.textContent).toContain("pnpm test");
    expect(timelineRegion.querySelectorAll(".sessions__code-block")[1]?.textContent).toContain("typescript");
    expect(timelineRegion.querySelectorAll(".sessions__code-block")[1]?.textContent).toContain("const label = 'ok';");
  });

  test("session detail renders template results like composer resource text", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-template-render"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 1n,
        sessionId: "session-template-render",
        itemId: "user-item",
        channel: "user_text",
        content: {
          kind: "textSnapshot",
          text: [
            "Before",
            "<template-result kind=\"tool\" tool=\"shell\">",
            "<arguments>{&quot;command&quot;:&quot;pwd&quot;,&quot;description&quot;:&quot;Show the current working directory&quot;}</arguments>",
            "<content>",
            "Exit code: 0\n\n/home/rfhold/repos/rfhold/waltr",
            "</content>",
            "</template-result>",
            "<template-result kind=\"file\" tool=\"read\" source=\"docs/two words.md\">",
            "<path>docs/two words.md</path>\n<type>file</type>\n<content>\n1: file body\n</content>",
            "</template-result>",
            "<template-result kind=\"skill\" tool=\"load\" source=\"documenting\">skill body</template-result>",
            "<template-error>tool fail: boom</template-error>",
            "After",
          ].join("\n"),
        },
      }),
    ];
    snapshot.sessions.timelines["session-template-render"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-template-render",
      snapshot,
    });

    const timelineRegion = screen.getByLabelText("Session timeline");
    const resourceTokens = Array.from(timelineRegion.querySelectorAll(".sessions__composer-resource-token"));
    expect(resourceTokens.map((token) => token.textContent)).toEqual([
      '@tool(shell, {"command":"pwd","description":"Show the current working directory"})',
      '@"docs/two words.md"',
    ]);
    const promptTokens = Array.from(timelineRegion.querySelectorAll(".sessions__composer-prompt-token"));
    expect(promptTokens.map((token) => token.textContent)).toEqual(["/skill:documenting"]);
    expect(timelineRegion.textContent).toContain('@tool(shell, {"command":"pwd","description":"Show the current working directory"})');
    expect(timelineRegion.textContent).toContain('@"docs/two words.md"');
    expect(timelineRegion.textContent).toContain("/skill:documenting");
    expect(timelineRegion.textContent).not.toContain("@documenting");
    expect(timelineRegion.textContent).toContain(
      'Before\n@tool(shell, {"command":"pwd","description":"Show the current working directory"})\n@"docs/two words.md"\n/skill:documenting\ntool fail: boom\nAfter',
    );
    const errorToken = timelineRegion.querySelector(".sessions__template-error-token");
    expect(errorToken?.textContent).toBe("tool fail: boom");
    expect(timelineRegion.textContent).not.toContain("<template-error>");
    expect(timelineRegion.textContent).toContain("Before");
    expect(timelineRegion.textContent).toContain("After");
  });

  test("session detail edit user message rolls back before cursor and pre-fills composer", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-edit-user"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 7n,
        sessionId: "session-edit-user",
        itemId: "user-item",
        channel: "user_text",
        content: { kind: "textSnapshot", text: "old prompt" },
      }),
    ];
    snapshot.sessions.timelines["session-edit-user"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-edit-user",
      snapshot,
    });

    fireEvent.click(screen.getByRole("button", { name: "Edit" }));

    await waitFor(() => expect(commands).toContainEqual({
      type: "rollbackSessionBeforeCursor",
      sessionId: "session-edit-user",
      cursor: 7n,
    }));
    expect((screen.getByLabelText("Session message") as HTMLTextAreaElement).value).toBe("old prompt");
    expect(commands).not.toContainEqual({
      type: "rollbackSessionToCursor",
      sessionId: "session-edit-user",
      cursor: 7n,
    });
  });

  test("session detail edit user message re-templatizes rendered template results", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-edit-template-user"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 11n,
        sessionId: "session-edit-template-user",
        itemId: "user-item",
        channel: "user_text",
        content: {
          kind: "textSnapshot",
          text: [
            "Summarize ",
            "<template-result kind=\"file\" tool=\"read\" source=\"docs/two words.md\">expanded file</template-result>",
            " and ",
            "<template-result kind=\"tool\" tool=\"shell\">",
            "<arguments>{&quot;command&quot;:&quot;pwd&quot;,&quot;description&quot;:&quot;Show cwd&quot;}</arguments>",
            "<content>Exit code: 0</content>",
            "</template-result>",
          ].join(""),
        },
      }),
    ];
    snapshot.sessions.timelines["session-edit-template-user"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-edit-template-user",
      snapshot,
    });

    fireEvent.click(screen.getByRole("button", { name: "Edit" }));

    await waitFor(() => expect(commands).toContainEqual({
      type: "rollbackSessionBeforeCursor",
      sessionId: "session-edit-template-user",
      cursor: 11n,
    }));
    expect((screen.getByLabelText("Session message") as HTMLTextAreaElement).value).toBe(
      'Summarize @"docs/two words.md" and @tool(shell, {"command":"pwd","description":"Show cwd"})',
    );
  });

  test("session detail rerun user message continues from cursor", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-rerun-user"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 9n,
        sessionId: "session-rerun-user",
        itemId: "user-item",
        channel: "user_text",
        content: { kind: "textSnapshot", text: "rerun prompt" },
      }),
    ];
    snapshot.sessions.timelines["session-rerun-user"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-rerun-user",
      snapshot,
    });

    fireEvent.click(screen.getByRole("button", { name: "Rerun" }));

    await waitFor(() => expect(commands).toContainEqual({
      type: "continueSessionFromCursor",
      sessionId: "session-rerun-user",
      cursor: 9n,
    }));
  });

  test("session detail renders thinking markdown", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-thinking-markdown"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 1n,
        sessionId: "session-thinking-markdown",
        itemId: "thinking-item",
        channel: "thinking",
        content: {
          kind: "textSnapshot",
          text: "**FOO** check `status` then **bar**\n\n**BAZ** inspect **qux**\n\n```bash\ngit status\n```",
        },
      }),
    ];
    snapshot.sessions.timelines["session-thinking-markdown"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-thinking-markdown",
      snapshot,
    });

    const timelineRegion = screen.getByLabelText("Session timeline");
    const strongTokens = timelineRegion.querySelectorAll(".sessions__markdown--thinking strong");
    expect(strongTokens).toHaveLength(4);
    expect(strongTokens[0]?.textContent).toBe("FOO");
    expect(strongTokens[0]?.classList.contains("sessions__markdown-strong--primary")).toBe(true);
    expect(strongTokens[1]?.textContent).toBe("bar");
    expect(strongTokens[1]?.classList.contains("sessions__markdown-strong--secondary")).toBe(true);
    expect(strongTokens[2]?.textContent).toBe("BAZ");
    expect(strongTokens[2]?.classList.contains("sessions__markdown-strong--primary")).toBe(true);
    expect(strongTokens[3]?.textContent).toBe("qux");
    expect(strongTokens[3]?.classList.contains("sessions__markdown-strong--secondary")).toBe(true);
    expect(timelineRegion.querySelector(".sessions__markdown--thinking")?.textContent).toContain("check status");
    expect(timelineRegion.querySelector(".sessions__code-block")?.textContent).toContain("git status");
  });

  test("session detail sanitizes assistant markdown html", () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-sanitized"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 1n,
        sessionId: "session-sanitized",
        itemId: "assistant-item",
        channel: "assistant_text",
        content: { kind: "textSnapshot", text: "Safe text\n\n<script>alert(1)</script>" },
      }),
    ];
    snapshot.sessions.timelines["session-sanitized"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-sanitized",
      snapshot,
    });

    const timelineRegion = screen.getByLabelText("Session timeline");
    expect(timelineRegion.textContent).toContain("Safe text");
    expect(timelineRegion.querySelector("script")).toBeNull();
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

  test("session detail collapses long shell output to recent lines", async () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-long-shell-output"),
    ]);
    let setSnapshot: (snapshot: ClientSnapshot) => void = () => {};
    const timeline = readySessionTimeline();
    const output = Array.from({ length: 45 }, (_, index) => `line-${String(index + 1).padStart(2, "0")}`).join("\n");
    timeline.events = [
      timelineEvent({
        cursor: 1n,
        itemId: "tool-call-item",
        content: { kind: "toolCall", id: "call-1", name: "shell", arguments_json: JSON.stringify({ command: "pnpm test" }) },
      }),
      timelineEvent({
        cursor: 2n,
        itemId: "tool-progress-info-item",
        parentId: "tool-call-item",
        channel: "tool_progress_info",
        content: { kind: "textDelta", delta: output },
      }),
    ];
    snapshot.sessions.timelines["session-long-shell-output"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-long-shell-output",
      snapshot,
      onSnapshotSetter: (setter) => {
        setSnapshot = setter;
      },
    });

    const shellTool = () => screen.getByLabelText("Shell tool");
    expect(shellTool().textContent).toContain("... click to expand");
    expect(shellTool().textContent).not.toContain("line-01");
    expect(shellTool().textContent).not.toContain("line-30");
    expect(shellTool().textContent).toContain("line-31");
    expect(shellTool().textContent).toContain("line-45");

    fireEvent.click(screen.getByRole("button", { name: "Shell output truncated to last 15 of 45 lines. Click to expand." }));

    expect(shellTool().textContent).not.toContain("... click to expand");
    expect(shellTool().textContent).toContain("line-01");

    const updatedTimeline = {
      ...timeline,
      events: [
        ...timeline.events,
        timelineEvent({
          cursor: 3n,
          itemId: "tool-progress-info-item-next",
          parentId: "tool-call-item",
          channel: "tool_progress_info",
          content: { kind: "textDelta", delta: `\n${Array.from({ length: 5 }, (_, index) => `line-${String(index + 46).padStart(2, "0")}`).join("\n")}` },
        }),
      ],
    };
    setSnapshot({
      ...snapshot,
      sessions: {
        ...snapshot.sessions,
        timelines: {
          ...snapshot.sessions.timelines,
          "session-long-shell-output": updatedTimeline,
        },
      },
    });

    await waitFor(() => expect(shellTool().textContent).toContain("line-50"));
    expect(shellTool().textContent).not.toContain("... click to expand");
    expect(shellTool().textContent).toContain("line-01");

    fireEvent.click(screen.getByRole("button", { name: "Shell output expanded. Click to collapse." }));

    expect(shellTool().textContent).toContain("... click to expand");
    expect(shellTool().textContent).not.toContain("line-01");
    expect(shellTool().textContent).not.toContain("line-35");
    expect(shellTool().textContent).toContain("line-36");
    expect(shellTool().textContent).toContain("line-50");
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

  test("session detail renders apply_patch input as a diff", async () => {
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
    expect(patchTool.querySelector(".sessions__tool-diff-line--remove")?.textContent).toContain("-old");
    expect(patchTool.querySelector(".sessions__tool-diff-line--add")?.textContent).toContain("+new");
    expect(patchTool.textContent).toContain("Success. Updated the following files:");
    await waitFor(() => expect(patchTool.querySelector(".sessions__highlighted-code")).toBeTruthy());
  });

  test("session detail renders edit input as a before and after diff", async () => {
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
    expect(editTool.querySelector(".sessions__tool-diff-line--remove")?.textContent).toContain("- const label = 'old';");
    expect(editTool.querySelector(".sessions__tool-diff-line--add")?.textContent).toContain("+ const label = 'new';");
    expect(editTool.textContent).not.toContain("Success. Updated src/app.ts");
    await waitFor(() => expect(editTool.querySelector(".sessions__highlighted-code")).toBeTruthy());
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

  test("session detail re-answer permission rolls back to opened prompt cursor", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-permission-reanswer"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [
      timelineEvent({
        cursor: 3n,
        sessionId: "session-permission-reanswer",
        itemId: "external-request:permission-1",
        kind: "externalRequestOpened",
        content: {
          kind: "externalRequest",
          request: {
            id: "permission-1",
            kind: 1,
            payloadJson: JSON.stringify({ tool_call: { name: "read", arguments: { filePath: "README.md" } } }),
          },
        },
      }),
      timelineEvent({
        cursor: 4n,
        sessionId: "session-permission-reanswer",
        itemId: "external-request:permission-1",
        kind: "externalRequestResolved",
        status: "resolved",
        content: {
          kind: "externalRequest",
          request: {
            id: "permission-1",
            kind: 1,
            payloadJson: JSON.stringify({ tool_call: { name: "read", arguments: { filePath: "README.md" } } }),
          },
        },
      }),
    ];
    snapshot.sessions.timelines["session-permission-reanswer"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-permission-reanswer",
      snapshot,
    });

    fireEvent.click(screen.getByRole("button", { name: "Re-answer" }));

    await waitFor(() => expect(commands).toContainEqual({
      type: "rollbackSessionToCursor",
      sessionId: "session-permission-reanswer",
      cursor: 3n,
    }));
    expect(commands).not.toContainEqual({
      type: "rollbackSessionToCursor",
      sessionId: "session-permission-reanswer",
      cursor: 4n,
    });
  });

  test("session detail re-answer question rolls back to opened prompt cursor", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-question-reanswer"),
    ]);
    const timeline = readySessionTimeline();
    const payload = {
      questions: [{
        header: "Direction",
        question: "Which path?",
        multiple: false,
        custom: false,
        options: [{ label: "A", description: "Path A" }],
      }],
    };
    timeline.events = [
      timelineEvent({
        cursor: 5n,
        sessionId: "session-question-reanswer",
        itemId: "external-request:question-1",
        kind: "externalRequestOpened",
        content: {
          kind: "externalRequest",
          request: { id: "question-1", kind: 2, payloadJson: JSON.stringify(payload) },
        },
      }),
      timelineEvent({
        cursor: 6n,
        sessionId: "session-question-reanswer",
        itemId: "external-request:question-1",
        kind: "externalRequestResolved",
        status: "resolved",
        content: {
          kind: "externalRequest",
          request: { id: "question-1", kind: 2, payloadJson: JSON.stringify(payload) },
        },
      }),
    ];
    snapshot.sessions.timelines["session-question-reanswer"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-question-reanswer",
      snapshot,
    });

    fireEvent.click(screen.getByRole("button", { name: "Re-answer" }));

    await waitFor(() => expect(commands).toContainEqual({
      type: "rollbackSessionToCursor",
      sessionId: "session-question-reanswer",
      cursor: 5n,
    }));
    expect(commands).not.toContainEqual({
      type: "rollbackSessionToCursor",
      sessionId: "session-question-reanswer",
      cursor: 6n,
    });
  });

  test("session detail renders unresolved permission prompts and submits decisions", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-permission"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [externalRequestOpenEvent({
      sessionId: "session-permission",
      requestId: "permission-1",
      kind: 1,
      payload: {
        tool_call: { name: "read", arguments: { filePath: "../outside/file.txt" } },
        patterns: [{ tool: "read", arguments: { filePath: "../outside/**" } }],
      },
    })];
    snapshot.sessions.timelines["session-permission"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-permission",
      snapshot,
    });

    const prompt = screen.getByLabelText("Permission prompt");
    expect(within(prompt).getByText("Permission required")).toBeTruthy();
    expect(within(prompt).getByText("permission-1")).toBeTruthy();
    expect(within(prompt).getByText("read")).toBeTruthy();
    expect(prompt.textContent).toContain("../outside/file.txt");
    expect(within(prompt).getByText("Always approval pattern")).toBeTruthy();
    expect(within(prompt).getByText("Allow once")).toBeTruthy();
    expect(within(prompt).getByText("Allow always")).toBeTruthy();
    expect(within(prompt).getByText("Deny and resume")).toBeTruthy();
    expect(within(prompt).getByText("Deny with interrupt")).toBeTruthy();

    fireEvent.click(within(prompt).getByText("Allow once"));

    await waitFor(() => expect(commands).toContainEqual({
      type: "respondExternalRequest",
      sessionId: "session-permission",
      requestId: "permission-1",
      responseJson: JSON.stringify({ decision: "allow_once" }),
    }));
  });

  test("session detail shows external response operation failures", async () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-permission-fail"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [externalRequestOpenEvent({
      sessionId: "session-permission-fail",
      requestId: "permission-1",
      kind: 1,
      payload: {
        tool_call: { name: "read", arguments: { filePath: "README.md" } },
        patterns: [],
      },
    })];
    snapshot.sessions.timelines["session-permission-fail"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-permission-fail",
      snapshot,
      respondExternalRequestError: "response rejected",
    });

    fireEvent.click(within(screen.getByLabelText("Permission prompt")).getByText("Allow once"));

    await waitFor(() => expect(screen.getByText("response rejected")).toBeTruthy());
  });

  test("session detail shows interrupt operation failures", async () => {
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-interrupt-fail", { phase: "streamingProvider", failedMessage: "" }),
    ]);
    snapshot.sessions.timelines["session-interrupt-fail"] = readySessionTimeline();

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/session-interrupt-fail",
      snapshot,
      interruptSessionError: "interrupt rejected",
    });

    fireEvent.click(screen.getByRole("button", { name: "Stop" }));

    await waitFor(() => expect(screen.getByText("interrupt rejected")).toBeTruthy());
  });

  test("session detail renders unresolved question prompts and submits answers", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithSessions([
      sessionWithRuntime("session-question"),
    ]);
    const timeline = readySessionTimeline();
    timeline.events = [externalRequestOpenEvent({
      sessionId: "session-question",
      requestId: "question-1",
      kind: 2,
      payload: {
        questions: [
          {
            header: "Direction",
            question: "Which visual direction should the prompt take?",
            multiple: false,
            custom: true,
            options: [
              { label: "Compact", description: "Keep the prompt dense and fast." },
              { label: "Guided", description: "Use more explanatory copy." },
            ],
          },
          {
            header: "Details",
            question: "What should stay visible while answering?",
            multiple: true,
            custom: false,
            options: [
              { label: "Tool name", description: "Show the requested action." },
              { label: "Payload", description: "Show the full request payload." },
            ],
          },
        ],
      },
    })];
    snapshot.sessions.timelines["session-question"] = timeline;

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/session-question",
      snapshot,
    });

    const prompt = screen.getByLabelText("Question prompt");
    expect(within(prompt).getByText("Question")).toBeTruthy();
    expect(within(prompt).getByText("Direction")).toBeTruthy();
    expect(within(prompt).getByText("Which visual direction should the prompt take?")).toBeTruthy();
    expect(within(prompt).getByText("Compact")).toBeTruthy();
    expect(within(prompt).getByText("Keep the prompt dense and fast.")).toBeTruthy();
    expect(within(prompt).getByText("Details")).toBeTruthy();
    expect(within(prompt).getByText("Payload")).toBeTruthy();
    expect(within(prompt).getByText("Custom answer")).toBeTruthy();

    fireEvent.click(inputForOption(prompt, "Compact"));
    fireEvent.click(inputForOption(prompt, "Payload"));
    fireEvent.input(within(prompt).getByLabelText("Custom answer"), { target: { value: "Make it calmer." } });
    fireEvent.click(within(prompt).getByText("Resume session"));

    await waitFor(() => expect(commands).toContainEqual({
      type: "respondExternalRequest",
      sessionId: "session-question",
      requestId: "question-1",
      responseJson: JSON.stringify({
        answers: [
          { header: "Direction", selections: ["Compact"], custom: "Make it calmer." },
          { header: "Details", selections: ["Payload"] },
        ],
      }),
    }));
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
      reasoningEffort: null,
      enabledMcpServerKeys: ["git"],
    }));
  });

  test("provider editor keeps dialog open and shows runtime update failures", async () => {
    const snapshot = snapshotWithConfig({
      ...emptyConfig("v1"),
      providers: {
        openai: provider("OpenAI", ["gpt-4o"]),
        anthropic: provider("Anthropic", ["claude-4"]),
      },
    });
    snapshot.sessions.current = [sessionWithRuntime("runtime-session-fail")];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/runtime-session-fail",
      snapshot,
      sessionRuntimeConfigUpdateError: "runtime update rejected",
    });

    fireEvent.click(screen.getByLabelText("Edit provider/model"));
    fireEvent.click(screen.getByText("anthropic"));
    fireEvent.click(screen.getByText("Save runtime"));

    await waitFor(() => expect(screen.getByText("runtime update rejected")).toBeTruthy());
    expect(screen.getByRole("dialog", { name: "Edit provider/model dialog" })).toBeTruthy();
  });

  test("bottom bar thinking dropdown dispatches session runtime metadata update", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = snapshotWithConfig({
      ...emptyConfig("v1"),
      providers: { openai: provider("OpenAI", ["gpt-4o"], ["low", "high"]) },
    });
    snapshot.sessions.current = [sessionWithRuntime("thinking-runtime-session")];

    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/thinking-runtime-session",
      snapshot,
    });

    fireEvent.change(screen.getByLabelText("Session thinking level"), { target: { value: "high" } });

    await waitFor(() => expect(commands).toContainEqual({
      type: "updateSessionRuntimeConfig",
      id: "thinking-runtime-session",
      providerKey: "openai",
      modelId: "gpt-4o",
      reasoningEffort: "high",
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
      reasoningEffort: null,
      enabledMcpServerKeys: ["git", "files"],
    }));
  });
});

function snapshotWithTimelineEvents(
  snapshot: ClientSnapshot,
  sessionId: string,
  events: TimelineEventView[],
): ClientSnapshot {
  const timeline = snapshot.sessions.timelines[sessionId];
  if (!timeline) {
    throw new Error(`missing timeline for ${sessionId}`);
  }

  return {
    ...snapshot,
    sessions: {
      ...snapshot.sessions,
      timelines: {
        ...snapshot.sessions.timelines,
        [sessionId]: {
          ...timeline,
          events,
        },
      },
    },
  };
}

function setScrollGeometry(
  element: HTMLElement,
  geometry: { scrollTop: number; scrollHeight: number; clientHeight: number },
) {
  Object.defineProperty(element, "scrollHeight", {
    configurable: true,
    value: geometry.scrollHeight,
  });
  Object.defineProperty(element, "clientHeight", {
    configurable: true,
    value: geometry.clientHeight,
  });
  element.scrollTop = geometry.scrollTop;
}

function timelineItemElement(key: string) {
  const item = Array.from(document.querySelectorAll<HTMLElement>("[data-timeline-item-key]")).find((element) => element.dataset.timelineItemKey === key);
  if (!item) {
    throw new Error(`missing timeline item ${key}`);
  }
  return item;
}

function setElementRect(element: HTMLElement, rect: { top: number; bottom: number }) {
  element.getBoundingClientRect = () => ({
    x: 0,
    y: rect.top,
    top: rect.top,
    bottom: rect.bottom,
    left: 0,
    right: 0,
    width: 0,
    height: rect.bottom - rect.top,
    toJSON: () => ({}),
  });
}

async function waitForTimelineScroll() {
  await new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
  await Promise.resolve();
}

type TemplateSearchCommand = Extract<ClientCommand, { type: "searchSessionTemplates" }>;

async function typeComposerText(input: HTMLTextAreaElement, value: string) {
  fireEvent.input(input, { target: { value } });
  input.setSelectionRange(value.length, value.length);
  fireEvent.keyUp(input, { key: value.at(-1) ?? "" });
  await Promise.resolve();
}

function templateSearchCommands(commands: ClientCommand[]): TemplateSearchCommand[] {
  return commands.filter((command): command is TemplateSearchCommand => command.type === "searchSessionTemplates");
}

function latestTemplateSearchCommand(commands: ClientCommand[]): TemplateSearchCommand {
  const searches = templateSearchCommands(commands);
  const command = searches.at(-1);
  if (!command) {
    throw new Error("expected a session resource search command");
  }

  return command;
}

function publishTemplateSearch(
  snapshot: ClientSnapshot,
  setSnapshot: (snapshot: ClientSnapshot) => void,
  command: TemplateSearchCommand,
  status: string,
  options: SessionTemplateOptionView[],
  error: string | null = null,
) {
  setSnapshot({
    ...snapshot,
    sessions: {
      ...snapshot.sessions,
      templateSearches: {
        ...snapshot.sessions.templateSearches,
        [command.sessionId]: {
          requestId: command.requestId,
          sessionId: command.sessionId,
          query: command.query,
          status,
          error,
          options,
        },
      },
    },
  });
}

function provider(name: string, modelIds: string[], reasoningEfforts: string[] = []) {
  return {
    kind: "openAiChatCompletions" as const,
    name,
    url: "https://example.invalid/v1",
    headers: {},
    oauthCompatible: false,
    oauthSettings: null,
    models: Object.fromEntries(modelIds.map((id) => [id, {
      name: id,
      kind: "llm" as const,
      options: { temperature: null, topP: null, topK: null, minP: null, presencePenalty: null, repetitionPenalty: null, reasoningEfforts },
      limits: { context: null, output: null },
      capabilities: { tools: true, image: false },
      catalogMatch: null,
      cost: null,
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
        kind: "openAiChatCompletions",
        url: "https://example.invalid/v1",
        modelId: "gpt-4o",
        reasoningEffort: null,
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

function externalRequestOpenEvent(options: {
  sessionId: string;
  requestId: string;
  kind: 1 | 2;
  payload: unknown;
}): TimelineEventView {
  return timelineEvent({
    sessionId: options.sessionId,
    itemId: `external-request:${options.requestId}`,
    kind: "externalRequestOpened",
    channel: "external_request",
    content: {
      kind: "externalRequest",
      request: {
        id: options.requestId,
        kind: options.kind,
        payloadJson: JSON.stringify(options.payload),
      },
    },
    status: "awaitingExternalInput",
  });
}

function inputForOption(root: HTMLElement, text: string) {
  const label = within(root).getByText(text).closest("label");
  const input = label?.querySelector("input");
  if (!input) {
    throw new Error(`missing input for option ${text}`);
  }

  return input;
}
