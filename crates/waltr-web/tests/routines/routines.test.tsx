import { fireEvent, screen, waitFor, within } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import type { ClientCommand, ClientRoutineInvocationDetail, ClientRoutineInvocationSummary, ClientSnapshot } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { RoutinesRouteWithRouter, testHistory } from "../support/routes";
import { operationSnapshot, snapshotWithConfig } from "../support/snapshots";

describe("Routines route", () => {
  test("owns both watch interests and renders loading, live definitions, reconnecting, and empty states", async () => {
    const commands: ClientCommand[] = [];
    let publish!: (snapshot: ClientSnapshot) => void;
    const loading = baseSnapshot();
    const result = renderWithAppState(() => <RoutinesRouteWithRouter />, {
      routePath: "/routines", snapshot: loading, commands, onSnapshotSetter: (setter) => { publish = setter; },
    });

    expect(screen.getByText("Loading Routine definitions...")).toBeTruthy();
    expect(screen.getByText("Loading Routine history...")).toBeTruthy();
    await waitFor(() => expect(types(commands)).toContain("registerRoutineDefinitionWatchInterest"));
    expect(types(commands)).toContain("registerRoutineInvocationWatchInterest");

    const ready = baseSnapshot();
    ready.routines.definitions = {
      watch: operationSnapshot("received"), digest: "digest-1",
      definitions: [{
        routineName: "fork", triggerName: "manual", displayName: "Fork session", description: "Create a branch.",
        scope: "session", routineRevision: "rev-1",
        inputs: [{ name: "Direction", type: "string", required: true, minimumStringLength: 3n, stringPattern: null }],
        results: [{ name: "session-id", type: "string" }],
      }],
    };
    ready.routines.history.watch = operationSnapshot("received");
    publish(ready);
    await waitFor(() => expect(screen.getByText("Fork session")).toBeTruthy());
    expect(screen.getByText("fork")).toBeTruthy();
    expect(screen.getByText("string, required")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Fork session/ })).toBeNull();
    expect(screen.queryByText(/source selector/i)).toBeNull();
    expect(screen.getByText("No Routine invocations yet.")).toBeTruthy();

    ready.routines.definitions.watch = { status: "failed", error: "offline", key: null };
    publish({ ...ready, routines: { ...ready.routines, definitions: { ...ready.routines.definitions } } });
    await waitFor(() => expect(screen.getByText("Reconnecting...")).toBeTruthy());
    result.unmount();
    await waitFor(() => expect(types(commands)).toContain("releaseRoutineDefinitionWatchInterest"));
    expect(types(commands)).toContain("releaseRoutineInvocationWatchInterest");
  });

  test("renders newest-first history, paginates, and navigates to a separate detail page", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = baseSnapshot();
    const newest = summary("newest", "event", "running", 20n);
    const older = summary("older", "manual", "succeeded", 10n);
    snapshot.routines.definitions.watch = operationSnapshot("received");
    snapshot.routines.history = {
      watch: operationSnapshot("received"), recent: [newest, older], throughSequence: 20n,
      olderPageToken: 10n, pagination: operationSnapshot("idle"),
    };
    renderWithAppState(() => <RoutinesRouteWithRouter />, {
      routePath: "/routines", snapshot, commands,
    });

    const history = screen.getByLabelText("Routine invocation history");
    const items = within(history).getAllByRole("link");
    expect(items[0]!.textContent).toContain("newest");
    expect(items[0]!.textContent).toContain("Event invocation");
    expect(items[1]!.textContent).toContain("Manual invocation");
    fireEvent.click(screen.getByRole("button", { name: "Load older invocations" }));
    expect(commands).toContainEqual(expect.objectContaining({ type: "loadOlderRoutineInvocations", pageSize: 50 }));

    fireEvent.click(items[0]!);
    await waitFor(() => expect(testHistory().get()).toBe("/routines/newest"));
    expect(screen.queryByLabelText("Routine invocation history")).toBeNull();
    expect(screen.queryByText("Manual definitions")).toBeNull();
    expect(screen.getByText("Loading invocation detail...")).toBeTruthy();
    await waitFor(() => expect(commands).toContainEqual(expect.objectContaining({ type: "loadRoutineInvocationDetail", invocationId: "newest" })));
    await waitFor(() => expect(count(commands, "releaseRoutineDefinitionWatchInterest")).toBe(1));
    expect(count(commands, "releaseRoutineInvocationWatchInterest")).toBe(1);
    expect(count(commands, "registerRoutineInvocationWatchInterest")).toBe(2);
    expect(count(commands, "registerRoutineDefinitionWatchInterest")).toBe(1);

    fireEvent.click(screen.getByRole("link", { name: "Back to Routines" }));
    await waitFor(() => expect(testHistory().get()).toBe("/routines"));
    expect(screen.getByLabelText("Routine invocation history")).toBeTruthy();
    await waitFor(() => expect(count(commands, "releaseRoutineInvocationWatchInterest")).toBe(2));
    expect(count(commands, "registerRoutineDefinitionWatchInterest")).toBe(2);
    expect(count(commands, "registerRoutineInvocationWatchInterest")).toBe(3);
  });

  test("loads and retries the exact URL invocation without list-only watch ownership", async () => {
    const commands: ClientCommand[] = [];
    let publish!: (snapshot: ClientSnapshot) => void;
    const snapshot = baseSnapshot();
    snapshot.routines.definitions.watch = operationSnapshot("received");
    snapshot.routines.history.watch = operationSnapshot("received");
    renderWithAppState(() => <RoutinesRouteWithRouter />, {
      routePath: "/routines/direct-invocation", snapshot, commands, onSnapshotSetter: (setter) => { publish = setter; },
    });

    await waitFor(() => expect(commands).toContainEqual(expect.objectContaining({
      type: "loadRoutineInvocationDetail",
      invocationId: "direct-invocation",
    })));
    expect(screen.getByText("Loading invocation detail...")).toBeTruthy();
    expect(screen.queryByLabelText("Routine invocation history")).toBeNull();
    expect(screen.getByRole("link", { name: "Back to Routines" }).getAttribute("href")).toBe("/routines");
    expect(types(commands)).toContain("registerRoutineInvocationWatchInterest");
    expect(types(commands)).not.toContain("registerRoutineDefinitionWatchInterest");

    const failed = clone(snapshot);
    failed.routines.details["direct-invocation"] = { status: "failed", detail: null, error: "detail unavailable" };
    publish(failed);
    await waitFor(() => expect(screen.getByText("detail unavailable")).toBeTruthy());
    const firstLoad = detailLoads(commands)[0]!;
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(detailLoads(commands)).toHaveLength(2));
    const retry = detailLoads(commands)[1]!;
    expect(retry.invocationId).toBe("direct-invocation");
    expect(retry.requestId).not.toBe(firstLoad.requestId);

    const direct = summary("direct-invocation", "manual", "succeeded", 30n);
    const next = clone(failed);
    next.routines.details[direct.invocationId] = { status: "ready", detail: invocationDetail(direct), error: null };
    publish(next);
    await waitFor(() => expect(screen.getByLabelText("Routine invocation detail")).toBeTruthy());
    expect(screen.getByRole("heading", { name: "direct-invocation" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "source" }).getAttribute("href")).toBe("/sessions/source");

    const task = screen.getByText("create").closest("details")!;
    expect(task.open).toBe(false);
    expect(within(task).getByText("session")).toBeTruthy();
    expect(within(task).getByText("Failed")).toBeTruthy();

    fireEvent.click(within(task).getByText("create").closest("summary")!);
    expect(task.open).toBe(true);
    expect(within(task).getByText("safe failure")).toBeTruthy();
    expect(within(task).getByText("result text")).toBeTruthy();
    expect(within(task).getByRole("link", { name: "helper-session" }).getAttribute("href")).toBe("/sessions/helper-session");
    expect(screen.getByLabelText("Routine results").textContent).toContain("false");
  });

  test("distinguishes watch errors without stale data", () => {
    const snapshot = baseSnapshot();
    snapshot.routines.definitions.watch = { status: "failed", error: "offline", key: null };
    snapshot.routines.history.watch = { status: "failed", error: "offline", key: null };
    renderWithAppState(() => <RoutinesRouteWithRouter />, { routePath: "/routines", snapshot });
    expect(screen.getByText("Unable to load Routine definitions.")).toBeTruthy();
    expect(screen.getByText("Unable to load Routine history.")).toBeTruthy();
  });

  test("retains authoritative empty history while reconnecting from sequence zero", () => {
    const snapshot = baseSnapshot();
    snapshot.routines.history = {
      watch: { status: "failed", error: "offline", key: null },
      recent: [],
      throughSequence: 0n,
      olderPageToken: null,
      pagination: operationSnapshot("idle"),
    };

    renderWithAppState(() => <RoutinesRouteWithRouter />, { routePath: "/routines", snapshot });

    expect(screen.getByText("Reconnecting...")).toBeTruthy();
    expect(screen.getByText("No Routine invocations yet.")).toBeTruthy();
    expect(screen.queryByText("Unable to load Routine history.")).toBeNull();
    expect(screen.queryByText("Loading Routine history...")).toBeNull();
  });
});

function baseSnapshot() {
  return snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, inferenceDefaults: null });
}

function summary(invocationId: string, triggerKind: "manual" | "event", status: ClientRoutineInvocationSummary["status"], sequence: bigint): ClientRoutineInvocationSummary {
  return {
    admissionSequence: sequence, invocationId, routineName: invocationId, routineRevision: "rev-1", triggerName: triggerKind,
    triggerKind, correlationId: "correlation", metadata: triggerKind === "manual" ? { "waltr.routine.source_session_id": "source" } : {},
    sourceCursor: triggerKind === "event" ? "cursor-7" : null, status, admittedAtMs: 1_700_000_000_000n + sequence,
    startedAtMs: null, completedAtMs: null, updatedAtMs: 1_700_000_000_000n + sequence,
    terminalError: null, latestSequence: sequence,
  };
}

function invocationDetail(value: ClientRoutineInvocationSummary): ClientRoutineInvocationDetail {
  return {
    summary: value,
    tasks: [{
      name: "create", type: "session", status: "failed", results: [{ name: "note", value: { type: "string", value: "result text" } }],
      sessionReferences: { helper: "helper-session" }, error: { code: "task_failed", message: "safe failure" },
    }],
    results: [{ name: "complete", value: { type: "boolean", value: false } }],
  };
}

function clone(snapshot: ClientSnapshot): ClientSnapshot {
  return { ...snapshot, routines: { ...snapshot.routines, details: { ...snapshot.routines.details } } };
}

function types(commands: ClientCommand[]) { return commands.map((command) => command.type); }

function count(commands: ClientCommand[], type: ClientCommand["type"]) {
  return commands.filter((command) => command.type === type).length;
}

function detailLoads(commands: ClientCommand[]) {
  return commands.filter((command): command is Extract<ClientCommand, { type: "loadRoutineInvocationDetail" }> => command.type === "loadRoutineInvocationDetail");
}
