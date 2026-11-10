import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { expect, test } from "vitest";
import type { ClientCommand } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { SessionsRouteWithRouter } from "../support/routes";
import { routineDefinition } from "../support/routines";
import { readySessionTimeline, sessionView, snapshotWithSessions } from "../support/snapshots";

function fixture() {
  const snapshot = snapshotWithSessions([sessionView({ id: "source" })]);
  snapshot.sessions.timelines.source = readySessionTimeline();
  snapshot.routines.definitions.definitions = [routineDefinition(), routineDefinition({ routineName: "global-only", entrypoints: [{ name: "global", trigger: "manual", selector: null, startContexts: ["global"] }] })];
  const commands: ClientCommand[] = [];
  const result = renderWithAppState(() => <SessionsRouteWithRouter />, { routePath: "/sessions/source", snapshot, commands });
  return { ...result, commands };
}
test("definition interest, context eligibility, exact revision/session/JSON dispatch and focus restore", async () => {
  const f = fixture(); const button = screen.getByRole("button", { name: "Commands" }); button.focus(); fireEvent.click(button);
  await waitFor(() => expect(f.commands).toContainEqual({ type: "registerRoutineDefinitionWatchInterest" }));
  expect(screen.queryByText("global-only")).toBeNull();
  fireEvent.input(screen.getByLabelText("Parameters (JSON)"), { target: { value: '{ "direction": null, "options": [true] }' } });
  fireEvent.click(screen.getByRole("button", { name: "Start fork" }));
  expect(f.commands).toContainEqual(expect.objectContaining({ type: "startManualRoutine", routineName: "fork", routineRevision: "revision/9007199254740993", triggerName: "manual", sourceSessionId: "source", inputs: '{ "direction": null, "options": [true] }' }));
  fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  expect(document.activeElement).toBe(button);
  expect(f.commands).toContainEqual({ type: "releaseRoutineDefinitionWatchInterest" });
});
test("Ctrl+P only outside editable controls and competing dialogs", async () => {
  fixture();
  const input = document.createElement("input"); document.body.append(input);
  const editable = new KeyboardEvent("keydown", { key: "p", ctrlKey: true, bubbles: true, cancelable: true }); input.dispatchEvent(editable);
  expect(editable.defaultPrevented).toBe(false); input.remove();
  const event = new KeyboardEvent("keydown", { key: "p", ctrlKey: true, bubbles: true, cancelable: true }); document.body.dispatchEvent(event);
  expect(event.defaultPrevented).toBe(true); expect(await screen.findByRole("dialog")).toBeTruthy();
});
