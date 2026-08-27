import { fireEvent, screen, waitFor, within } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import type { ClientCommand, ClientManualRoutineDefinition, ClientSnapshot, RoutineStartOperationView } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { SessionsRouteWithRouter, testHistory } from "../support/routes";
import { operationSnapshot, readySessionTimeline, sessionView, snapshotWithSessions } from "../support/snapshots";

const definition: ClientManualRoutineDefinition = {
  routineName: "fork",
  triggerName: "manual",
  displayName: "Fork session",
  description: "Create a new session from this point.",
  scope: "session",
  inputs: [{ name: "Direction", type: "string", required: true, defaultValue: null, minimumStringLength: 3n, stringPattern: "[a-z]+" }],
  results: [{ name: "session-id", type: "string" }],
  routineRevision: "revision-1",
};

describe("session commands palette", () => {
  test("adds only the definition watch while open, never performs unary discovery, and restores focus", async () => {
    const commands: ClientCommand[] = [];
    const result = renderSession({ commands });
    const trigger = screen.getByRole("button", { name: "Commands" });
    trigger.focus();
    fireEvent.click(trigger);

    await waitFor(() => expect(commandTypes(commands)).toContain("registerRoutineDefinitionWatchInterest"));
    expect(commands).toContainEqual({ type: "registerSessionRoutineInvocationWatchInterest", sessionId: "source" });
    expect(commandTypes(commands)).not.toContain("requestManualRoutines");
    const dialog = screen.getByRole("dialog", { name: "Session commands dialog" });
    expect(screen.getByText("Fork session")).toBeTruthy();
    fireEvent.keyDown(dialog, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Session commands dialog" })).toBeNull());
    expect(commandTypes(commands)).toContain("releaseRoutineDefinitionWatchInterest");
    expect(commandTypes(commands)).not.toContain("releaseSessionRoutineInvocationWatchInterest");
    expect(document.activeElement).toBe(trigger);
    result.unmount();
    await waitFor(() => expect(commands).toContainEqual({ type: "releaseSessionRoutineInvocationWatchInterest", sessionId: "source" }));
  });

  test("opens with Ctrl+P and prevents browser Print", async () => {
    renderSession();
    const event = shortcutEvent(document.body, { key: "p", ctrlKey: true });
    expect(event.defaultPrevented).toBe(true);
    expect(await screen.findByRole("dialog", { name: "Session commands dialog" })).toBeTruthy();
  });

  test("preserves browser shortcuts in editable controls, conflicting dialogs, and non-Ctrl+P contexts", () => {
    let publish!: (snapshot: ClientSnapshot) => void;
    renderSession({ onSnapshotSetter: (setter) => { publish = setter; } });
    const input = document.createElement("input");
    document.body.append(input);
    expect(shortcutEvent(input, { key: "p", ctrlKey: true }).defaultPrevented).toBe(false);
    expect(screen.queryByRole("dialog", { name: "Session commands dialog" })).toBeNull();
    input.remove();

    const conflictingDialog = document.createElement("div");
    conflictingDialog.setAttribute("role", "dialog");
    conflictingDialog.setAttribute("aria-modal", "true");
    document.body.append(conflictingDialog);
    expect(shortcutEvent(document.body, { key: "p", ctrlKey: true }).defaultPrevented).toBe(false);
    conflictingDialog.remove();

    expect(shortcutEvent(document.body, { key: "p", metaKey: true }).defaultPrevented).toBe(false);
    expect(shortcutEvent(document.body, { key: "k", ctrlKey: true }).defaultPrevented).toBe(false);

    const notReady = sessionSnapshot();
    notReady.status.ready = false;
    publish(notReady);
    expect(shortcutEvent(document.body, { key: "p", ctrlKey: true }).defaultPrevented).toBe(false);
    expect(screen.queryByRole("dialog", { name: "Session commands dialog" })).toBeNull();
  });

  test("keeps validation local or structured and starts with the exact session association", async () => {
    const commands: ClientCommand[] = [];
    let publish!: (snapshot: ClientSnapshot) => void;
    const snapshot = sessionSnapshot();
    renderSession({ snapshot, commands, onSnapshotSetter: (setter) => { publish = setter; } });
    openAndSelect();
    fireEvent.click(screen.getByRole("button", { name: "Fork session" }));
    expect(screen.getByText("Direction is required.")).toBeTruthy();
    expect(starts(commands)).toHaveLength(0);

    fireEvent.input(screen.getByLabelText("Direction"), { target: { value: "north" } });
    fireEvent.click(screen.getByRole("button", { name: "Fork session" }));
    const command = starts(commands)[0]!;
    fireEvent.click(screen.getByRole("button", { name: "Starting command..." }));
    expect(starts(commands)).toHaveLength(1);
    expect(command.sourceSessionId).toBe("source");
    expect(command.inputs.Direction).toEqual({ type: "string", value: "north" });

    publish(withStart(snapshot, command, {
      status: "failed",
      validationFailure: { message: "Inputs are invalid.", violations: [{ fieldPath: "inputs.Direction", message: "Choose another direction." }] },
    }));
    await waitFor(() => expect(screen.getByText("Choose another direction.")).toBeTruthy());
    expect((screen.getByLabelText("Direction") as HTMLTextAreaElement).value).toBe("north");
    fireEvent.input(screen.getByLabelText("Direction"), { target: { value: "south" } });
    await waitFor(() => expect(screen.queryByText("Choose another direction.")).toBeNull());
    fireEvent.click(screen.getByRole("button", { name: "Fork session" }));
    const retry = starts(commands)[1]!;
    expect(retry.operationKey).not.toBe(command.operationKey);
    expect(retry.requestId).not.toBe(command.requestId);
    expect(retry.inputs.Direction).toEqual({ type: "string", value: "south" });
  });

  test("initializes server defaults and preserves user edits", () => {
    const withDefault: ClientManualRoutineDefinition = {
      ...definition,
      inputs: [{ ...definition.inputs[0]!, required: false, defaultValue: { type: "string", value: "north" } }],
    };
    renderSession({ snapshot: sessionSnapshot(withDefault) });
    openAndSelect();
    expect((screen.getByLabelText("Direction") as HTMLTextAreaElement).value).toBe("north");
    fireEvent.input(screen.getByLabelText("Direction"), { target: { value: "south" } });
    expect((screen.getByLabelText("Direction") as HTMLTextAreaElement).value).toBe("south");
  });

  test("shows immediate admission success without eager progress and Close stays on the source session", async () => {
    const commands: ClientCommand[] = [];
    let publish!: (snapshot: ClientSnapshot) => void;
    const snapshot = sessionSnapshot();
    renderSession({ snapshot, commands, onSnapshotSetter: (setter) => { publish = setter; } });
    openAndSelect();
    fireEvent.input(screen.getByLabelText("Direction"), { target: { value: "north" } });
    fireEvent.click(screen.getByRole("button", { name: "Fork session" }));
    const command = starts(commands)[0]!;

    publish(withStart(snapshot, command, { status: "ready", invocationId: "server-invocation" }));
    await waitFor(() => expect(screen.getByText("Command admitted.")).toBeTruthy());
    expect(within(screen.getByRole("dialog", { name: "Session commands dialog" })).getAllByRole("button").map((button) => button.textContent)).toEqual(["View invocation", "Close"]);
    expect(commandTypes(commands)).not.toContain("loadRoutineInvocationDetail");
    expect(screen.queryByLabelText("Routine invocation detail")).toBeNull();
    expect(testHistory().get()).toBe("/sessions/source");
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Session commands dialog" })).toBeNull());
    expect(testHistory().get()).toBe("/sessions/source");
  });

  test("navigates an admitted invocation to its durable detail URL", async () => {
    const commands: ClientCommand[] = [];
    let publish!: (snapshot: ClientSnapshot) => void;
    const snapshot = sessionSnapshot();
    renderSession({ snapshot, commands, onSnapshotSetter: (setter) => { publish = setter; } });
    openAndSelect();
    fireEvent.input(screen.getByLabelText("Direction"), { target: { value: "north" } });
    fireEvent.click(screen.getByRole("button", { name: "Fork session" }));
    const command = starts(commands)[0]!;
    publish(withStart(snapshot, command, { status: "ready", invocationId: "server/invocation" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "View invocation" })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "View invocation" }));
    await waitFor(() => expect(testHistory().get()).toBe("/routines/server%2Finvocation"));
  });
});

function renderSession(options: Parameters<typeof renderWithAppState>[1] = {}) {
  return renderWithAppState(() => <SessionsRouteWithRouter />, { routePath: "/sessions/source", snapshot: sessionSnapshot(), ...options });
}

function sessionSnapshot(routineDefinition = definition) {
  const snapshot = snapshotWithSessions([sessionView({ id: "source" })]);
  snapshot.sessions.timelines.source = readySessionTimeline();
  snapshot.routines.definitions = { watch: operationSnapshot("received"), digest: "definitions-1", definitions: [routineDefinition] };
  snapshot.routines.history.watch = operationSnapshot("received");
  return snapshot;
}

function openAndSelect() {
  fireEvent.click(screen.getByRole("button", { name: "Commands" }));
  fireEvent.click(screen.getByRole("button", { name: /Fork session/ }));
}

function starts(commands: ClientCommand[]) {
  return commands.filter((command): command is Extract<ClientCommand, { type: "startManualRoutine" }> => command.type === "startManualRoutine");
}

function commandTypes(commands: ClientCommand[]) { return commands.map((command) => command.type as string); }

function withStart(snapshot: ClientSnapshot, command: Extract<ClientCommand, { type: "startManualRoutine" }>, overrides: Partial<RoutineStartOperationView>) {
  const next = clone(snapshot);
  next.routines.starts[command.operationKey] = {
    operationKey: command.operationKey, requestId: command.requestId, routineName: command.routineName,
    triggerName: command.triggerName, sourceSessionId: command.sourceSessionId, inputs: command.inputs,
    status: "loading", invocationId: null, validationFailure: null, error: null, ...overrides,
  };
  return next;
}

function clone(snapshot: ClientSnapshot): ClientSnapshot {
  return { ...snapshot, routines: { ...snapshot.routines, starts: { ...snapshot.routines.starts }, details: { ...snapshot.routines.details } } };
}

function shortcutEvent(target: HTMLElement, init: KeyboardEventInit) {
  const event = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(event);
  return event;
}
