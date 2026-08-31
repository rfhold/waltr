import { fireEvent, render, screen, waitFor } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { AppStateProvider } from "../../src/app/state";
import type { ClientCommand, ClientSnapshot } from "../../src/generated/waltr-client";
import type { ClientAdapter, ClientSnapshotListener } from "../../src/platform/client";
import { renderWithAppState } from "../support/render";
import { SessionsRouteWithRouter, ShellRouteWithChildren, resetTestHistory, testHistory } from "../support/routes";
import { sessionView, snapshotWithSessions } from "../support/snapshots";

describe("session data management", () => {
  test("links from Sessions to the static data route with Sessions navigation active", async () => {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions",
      snapshot: snapshotWithSessions([]),
    });

    fireEvent.click(screen.getByRole("link", { name: "Manage session data" }));

    await waitFor(() => expect(testHistory().get()).toBe("/sessions/data"));
    expect(screen.getByRole("heading", { name: "Session data" })).toBeTruthy();
    expect(screen.queryByText("Waiting for session...")).toBeNull();
    expect(screen.getAllByRole("link", { name: "Sessions" }).some((link) => link.getAttribute("aria-current") === "page")).toBe(true);
  });

  test("focuses confirmation and restores focus to the trigger on cancel", async () => {
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <SessionsRouteWithRouter />, {
      commands,
      routePath: "/sessions/data",
      snapshot: snapshotWithSessions([sessionView()]),
    });

    expect(screen.getByText(/permanently deletes all agent sessions and their conversation history/i)).toBeTruthy();
    expect(screen.getByText(/does not affect configuration, authentication, OAuth credentials, or MCP\/Waltr SQL state/i)).toBeTruthy();

    const trigger = screen.getByRole("button", { name: "Delete all sessions" });
    trigger.focus();
    expect(document.activeElement).toBe(trigger);
    fireEvent.click(trigger);
    expect(screen.getByRole("group", { name: "Delete all sessions confirmation" })).toBeTruthy();
    const confirm = screen.getByRole("button", { name: "Confirm delete all sessions" });
    await waitFor(() => expect(document.activeElement).toBe(confirm));

    fireEvent.click(screen.getByRole("button", { name: "Cancel delete all sessions" }));

    expect(screen.queryByRole("group", { name: "Delete all sessions confirmation" })).toBeNull();
    const restoredTrigger = screen.getByRole("button", { name: "Delete all sessions" });
    expect(restoredTrigger).not.toBe(trigger);
    await waitFor(() => expect(document.activeElement).toBe(restoredTrigger));
    expect(commands.filter((command) => command.type === "wipeSessions")).toHaveLength(0);
  });

  test("locks controls while pending and ignores unrelated operation success", async () => {
    const harness = renderWipeRoute();
    await confirmWipe();
    await waitFor(() => expect(harness.wipeCommands()).toEqual([{ type: "wipeSessions" }]));

    const confirm = screen.getByRole("button", { name: "Confirm delete all sessions" }) as HTMLButtonElement;
    const cancel = screen.getByRole("button", { name: "Cancel delete all sessions" }) as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    expect(cancel.disabled).toBe(true);
    fireEvent.click(confirm);
    expect(harness.wipeCommands()).toHaveLength(1);

    harness.publishForeignSuccess("wipe-foreign");
    await Promise.resolve();
    expect(screen.getByRole("group", { name: "Delete all sessions confirmation" })).toBeTruthy();
    expect(screen.queryByText("All agent sessions deleted.")).toBeNull();
  });

  test("shows matching failure, permits retry, and remains on the data page after success", async () => {
    const harness = renderWipeRoute();
    await confirmWipe();
    await waitFor(() => expect(harness.wipeCommands()).toHaveLength(1));

    harness.settle("wipe-1", "failed", "session service unavailable");
    await screen.findByText("session service unavailable");
    expect((screen.getByRole("button", { name: "Confirm delete all sessions" }) as HTMLButtonElement).disabled).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: "Confirm delete all sessions" }));
    await waitFor(() => expect(harness.wipeCommands()).toHaveLength(2));
    harness.settle("wipe-2", "ready", null);

    await screen.findByText("All agent sessions deleted.");
    expect(screen.queryByRole("group", { name: "Delete all sessions confirmation" })).toBeNull();
    expect(testHistory().get()).toBe("/sessions/data");
  });

  test("returns an open detail route to Sessions when a remote wipe removes its session", async () => {
    let publish!: (snapshot: ClientSnapshot) => void;
    const initial = snapshotWithSessions([sessionView({ id: "remote-session" })]);
    renderWithAppState(() => <SessionsRouteWithRouter />, {
      routePath: "/sessions/remote-session",
      snapshot: initial,
      onSnapshotSetter: (setSnapshot) => {
        publish = setSnapshot;
      },
    });

    expect(screen.getByLabelText("Session status summary")).toBeTruthy();
    publish(wipeSnapshot(initial, "remote-wipe", "ready", null));

    await waitFor(() => expect(testHistory().get()).toBe("/sessions"));
    expect(screen.getByText("No sessions yet.")).toBeTruthy();
  });
});

async function confirmWipe() {
  const open = await screen.findByRole("button", { name: "Delete all sessions" }) as HTMLButtonElement;
  await waitFor(() => expect(open.disabled).toBe(false));
  fireEvent.click(open);
  fireEvent.click(screen.getByRole("button", { name: "Confirm delete all sessions" }));
}

function renderWipeRoute() {
  resetTestHistory("/sessions/data");
  let current = snapshotWithSessions([sessionView({ id: "wipe-me" })]);
  let listener: ClientSnapshotListener | undefined;
  let wipeId = 0;
  const commands: ClientCommand[] = [];
  const publish = (snapshot: ClientSnapshot) => {
    current = snapshot;
    listener?.(snapshot);
  };
  const client: ClientAdapter = {
    snapshot: () => current,
    start: async () => undefined,
    subscribe: (next) => {
      listener = next;
      return () => { listener = undefined; };
    },
    dispatch: async (command) => {
      commands.push(command);
      if (command.type === "wipeSessions") {
        wipeId += 1;
        publish(wipeSnapshot(current, `wipe-${wipeId}`, "loading", null));
      }
      return current;
    },
    dispose: () => undefined,
  };

  render(() => (
    <AppStateProvider loadClientAdapter={async () => client}>
      <SessionsRouteWithRouter />
    </AppStateProvider>
  ));

  return {
    wipeCommands: () => commands.filter((command) => command.type === "wipeSessions"),
    settle: (key: string, status: "ready" | "failed", error: string | null) => {
      publish(wipeSnapshot(current, key, status, error));
    },
    publishForeignSuccess: (key: string) => {
      publish({
        ...current,
        operations: {
          current: {
            ...current.operations.current,
            [key]: { kind: "sessionWipe", status: "ready", error: null },
          },
        },
      });
    },
  };
}

function wipeSnapshot(
  snapshot: ClientSnapshot,
  key: string,
  status: "loading" | "ready" | "failed",
  error: string | null,
): ClientSnapshot {
  return {
    ...snapshot,
    operations: {
      current: {
        ...snapshot.operations.current,
        [key]: { kind: "sessionWipe", status, error },
      },
    },
    sessions: {
      ...snapshot.sessions,
      current: status === "ready" ? [] : snapshot.sessions.current,
      wipe: { status, error, key },
    },
  };
}
