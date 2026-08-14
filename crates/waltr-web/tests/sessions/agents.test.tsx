import { fireEvent, render, screen, waitFor, within } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { AppStateProvider } from "../../src/app/state";
import type { ClientCommand, ClientSnapshot, SessionView } from "../../src/generated/waltr-client";
import type { ClientSnapshotListener, WasmClient } from "../../src/wasm/client";
import { SessionsRouteWithRouter, resetTestHistory } from "../support/routes";
import { readySessionTimeline, sessionView, snapshotWithSessions } from "../support/snapshots";

describe("session agent operations", () => {
  test("ignores stale switch success and failure until the matching operation succeeds", async () => {
    const harness = renderAgentRoute();

    fireEvent.click(await screen.findByRole("button", { name: "Switch agent" }));
    const dialog = screen.getByRole("dialog", { name: "Switch agent dialog" });
    fireEvent.click(within(dialog).getByRole("radio", { name: /reviewer/i }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Switch agent" }));

    await waitFor(() => expect(harness.updateCommands()).toEqual([
      { type: "updateSessionAgent", id: "session-agent-keyed", agentName: "reviewer" },
    ]));

    harness.publishStale("old-success", "ready");
    await Promise.resolve();
    expect(screen.getByRole("dialog", { name: "Switch agent dialog" })).toBeTruthy();

    harness.publishStale("old-failure", "failed");
    await Promise.resolve();
    expect(screen.getByRole("dialog", { name: "Switch agent dialog" })).toBeTruthy();
    expect(screen.queryByText("stale failure")).toBeNull();

    harness.settleMatching();
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Switch agent dialog" })).toBeNull());
    expect(screen.getByLabelText("Session status summary").textContent).toContain("reviewer");
  });
});

function renderAgentRoute() {
  resetTestHistory("/sessions/session-agent-keyed");
  const initial = snapshotWithSessions([sessionView({ id: "session-agent-keyed", agentName: "default" })]);
  initial.sessions.timelines["session-agent-keyed"] = readySessionTimeline();
  let current = initial;
  let listener: ClientSnapshotListener | undefined;
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
      if (command.type === "updateSessionAgent") {
        publish(agentUpdateSnapshot(current, "agent-update-current", "loading", null, null));
      }
      return current;
    },
  };

  render(() => (
    <AppStateProvider loadWasmClient={async () => client}>
      <SessionsRouteWithRouter />
    </AppStateProvider>
  ));

  return {
    updateCommands: () => commands.filter((command) => command.type === "updateSessionAgent"),
    publishStale: (key: string, status: "ready" | "failed") => {
      const stale = agentUpdateSnapshot(current, key, status, status === "failed" ? "stale failure" : null, null);
      stale.sessions.agentUpdate = current.sessions.agentUpdate;
      publish(stale);
    },
    settleMatching: () => {
      const session = { ...current.sessions.current[0]!, agentName: "reviewer" };
      publish(agentUpdateSnapshot(current, "agent-update-current", "ready", null, session));
    },
  };
}

function agentUpdateSnapshot(
  snapshot: ClientSnapshot,
  key: string,
  status: "loading" | "ready" | "failed",
  error: string | null,
  session: SessionView | null,
): ClientSnapshot {
  return {
    ...snapshot,
    operations: {
      current: {
        ...snapshot.operations.current,
        [key]: { kind: "sessionAgentUpdate", status, error, session },
      },
    },
    sessions: {
      ...snapshot.sessions,
      current: session ? snapshot.sessions.current.map((entry) => entry.id === session.id ? session : entry) : snapshot.sessions.current,
      agentUpdate: { status, error, key },
    },
  };
}
