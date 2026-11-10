import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { expect, test } from "vitest";
import type { ClientCommand, ClientSnapshot } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { RoutinesRouteWithRouter } from "../support/routes";
import { routineDefinition, routineSnapshot, routineSummary } from "../support/routines";
import { initialParameters, validateParameters } from "../../src/routes/routines/parameters";
import { MemoryRouter, Route } from "@solidjs/router";
import { createSignal } from "solid-js";
import { SessionCommandPalette } from "../../src/routes/sessions/SessionCommandPalette";
import { testHistory } from "../support/routes";

test("session command keeps its draft and admission receipt across serialized snapshots", async () => {
  const snapshot = routineSnapshot();
  const commands: ClientCommand[] = [];
  let publish!: (snapshot: ClientSnapshot) => void;
  renderWithAppState(() => <MemoryRouter><Route path="*" component={() => <SessionCommandPalette sessionId="source" />} /></MemoryRouter>, {
    snapshot, commands, onSnapshotSetter: setter => { publish = setter; },
  });
  fireEvent.click(screen.getByRole("button", { name: "Commands" }));
  const json = '{"direction":"chosen","options":[true]}';
  fireEvent.input(screen.getByRole("textbox", { name: "Parameters (JSON)" }), { target: { value: json } });
  fireEvent.click(screen.getByRole("button", { name: "Start fork" }));
  const start = commands.find(command => command.type === "startManualRoutine") as Extract<ClientCommand, { type: "startManualRoutine" }>;
  expect(start).toMatchObject({ sourceSessionId: "source", inputs: json });
  const next = structuredClone(snapshot);
  next.routines.starts[start.operationKey] = {
    operationKey: start.operationKey, requestId: start.requestId, routineName: start.routineName,
    triggerName: start.triggerName, sourceSessionId: start.sourceSessionId, inputs: json,
    status: "ready", invocationId: "admitted", validationFailure: null, error: null,
  };
  publish(next);
  await waitFor(() => expect(screen.getByRole("textbox", { name: "Parameters (JSON)" })).toHaveProperty("value", json));
  expect(screen.getByRole("link", { name: "View invocation" }).getAttribute("href")).toBe("/routines/admitted");
  expect(commands.filter(command => command.type === "startManualRoutine")).toHaveLength(1);
});

for (const surface of ["session commands", "Routine definitions"] as const) {
  test(`${surface} preserves pending, error and result state; revision, removal and source changes reset it`, async () => {
    let snapshot = routineSnapshot();
    const commands: ClientCommand[] = [];
    let publish!: (snapshot: ClientSnapshot) => void;
    const [source, setSource] = createSignal("source");
    renderWithAppState(() => surface === "session commands"
      ? <MemoryRouter><Route path="*" component={() => <SessionCommandPalette sessionId={source()} />} /></MemoryRouter>
      : <RoutinesRouteWithRouter />, {
      routePath: "/routines?session=source", snapshot, commands, onSnapshotSetter: setter => { publish = setter; },
    });
    if (surface === "session commands") fireEvent.click(screen.getByRole("button", { name: "Commands" }));
    const draft = '{"direction":"chosen","options":[true]}';
    const input = () => screen.getByRole("textbox", { name: "Parameters (JSON)" });
    const submit = () => fireEvent.click(screen.getByRole("button", { name: "Start fork" }));
    const starts = () => commands.filter((command): command is Extract<ClientCommand, { type: "startManualRoutine" }> => command.type === "startManualRoutine");
    const update = () => { snapshot = structuredClone(snapshot); publish(snapshot); };
    const result = (status: "loading" | "failed" | "ready") => {
      const start = starts().at(-1)!;
      snapshot.routines.starts[start.operationKey] = {
        ...start, status, invocationId: status === "ready" ? "admitted" : null,
        validationFailure: null, error: status === "failed" ? "Admission denied." : null,
      };
      update();
    };
    fireEvent.input(input(), { target: { value: draft } });
    submit();
    result("loading");
    await waitFor(() => expect(screen.getByRole("button", { name: "Start fork" })).toHaveProperty("disabled", true));
    expect(input()).toHaveProperty("value", draft);
    result("failed");
    await screen.findByText("Admission denied.");
    update();
    expect(input()).toHaveProperty("value", draft);
    expect(screen.getByRole("alert").textContent).toBe("Admission denied.");
    result("ready");
    await screen.findByRole("link", { name: "View invocation" });
    update();
    expect(input()).toHaveProperty("value", draft);
    expect(screen.getByRole("link", { name: "View invocation" }).getAttribute("href")).toBe("/routines/admitted");
    expect(starts()).toHaveLength(1);

    snapshot.routines.definitions.definitions[0]!.routineRevision = "new-revision";
    update();
    await waitFor(() => expect(input()).toHaveProperty("value", initialParameters(routineDefinition().parameters.schema)));
    expect(screen.queryByRole("link", { name: "View invocation" })).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    submit();
    expect(starts().at(-1)).toMatchObject({ routineRevision: "new-revision", sourceSessionId: "source" });
    expect(starts()[1]!.operationKey).not.toBe(starts()[0]!.operationKey);
    result("ready");
    await screen.findByRole("link", { name: "View invocation" });

    const definition = snapshot.routines.definitions.definitions[0]!;
    snapshot.routines.definitions.definitions = [];
    update();
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "Parameters (JSON)" })).toBeNull());
    snapshot.routines.definitions.definitions = [definition];
    update();
    await screen.findByRole("textbox", { name: "Parameters (JSON)" });
    expect(screen.queryByRole("link", { name: "View invocation" })).toBeNull();
    fireEvent.input(input(), { target: { value: draft } });
    submit();
    result("ready");
    await screen.findByRole("link", { name: "View invocation" });
    if (surface === "session commands") setSource("other");
    else testHistory().set({ value: "/routines?session=other", replace: true, scroll: false });
    await waitFor(() => expect(input()).toHaveProperty("value", initialParameters(routineDefinition().parameters.schema)));
    expect(screen.queryByRole("link", { name: "View invocation" })).toBeNull();
    submit();
    expect(starts().at(-1)).toMatchObject({ sourceSessionId: "other", routineRevision: "new-revision" });
    expect(new Set(starts().map(start => start.operationKey)).size).toBe(4);
  });
}

test("uses both watch interests; exact keyed selected pages, expiry and matching release", async () => {
  const commands: ClientCommand[] = [];
  const snapshot = routineSnapshot(); snapshot.routines.history.recent = [routineSummary("recent")];
  let publish!: (s: ClientSnapshot) => void;
  const rendered = renderWithAppState(() => <RoutinesRouteWithRouter />, { routePath: "/routines", snapshot, commands, onSnapshotSetter: setter => { publish = setter; } });
  await waitFor(() => expect(commands).toContainEqual({ type: "registerRoutineDefinitionWatchInterest" }));
  expect(commands).toContainEqual({ type: "registerRoutineInvocationWatchInterest" });
  fireEvent.click(screen.getByRole("button", { name: "Browse history from beginning" }));
  const command = commands.find(c => c.type === "routineRequest") as Extract<ClientCommand, { type: "routineRequest" }>;
  expect(command.command).toEqual({ type: "invocations", afterCursor: "0", limit: 50, sourceSession: null });
  publish({ ...snapshot, routines: { ...snapshot.routines, requests: { [command.operationKey]: { requestId: command.requestId, command: command.command, status: "ready", error: null, nextCursor: "9007199254740997", cancellationStatus: null, selectedInvocations: [routineSummary("selected")] } } } });
  await screen.findByText(/selected/);
  fireEvent.click(screen.getByRole("button", { name: "Next history page" }));
  const next = commands.filter(c => c.type === "routineRequest").at(-1) as typeof command;
  expect(next.command).toMatchObject({ afterCursor: "9007199254740997" });
  expect(next.requestId).not.toBe(command.requestId);
  publish({ ...snapshot, routines: { ...snapshot.routines, requests: { [next.operationKey]: { requestId: next.requestId, command: next.command, status: "expired", error: "Page budget expired.", nextCursor: null, cancellationStatus: null, selectedInvocations: [] } } } });
  await screen.findByText("This history page expired. Load it again explicitly.");
  rendered.unmount();
  expect(commands).toContainEqual({ type: "releaseRoutineRequest", operationKey: next.operationKey, requestId: next.requestId });
  expect(commands).toContainEqual({ type: "releaseRoutineInvocationWatchInterest" });
  expect(commands).toContainEqual({ type: "releaseRoutineDefinitionWatchInterest" });
});

test("exact session filter is shared-client watch and page identity, not local filtering", async () => {
  const commands: ClientCommand[] = [];
  renderWithAppState(() => <RoutinesRouteWithRouter />, { routePath: "/routines?session=source%2Fexact", snapshot: routineSnapshot(), commands });
  await waitFor(() => expect(commands).toContainEqual({ type: "registerSessionRoutineInvocationWatchInterest", sessionId: "source/exact" }));
  fireEvent.click(screen.getByRole("button", { name: "Browse history from beginning" }));
  expect(commands).toContainEqual(expect.objectContaining({ type: "routineRequest", command: { type: "invocations", afterCursor: "0", limit: 50, sourceSession: "source/exact" } }));
});

test("historical definition survives catalog deletion; dynamic uncertain operations and keyed cancellation", async () => {
  const snapshot = routineSnapshot(); snapshot.routines.definitions.definitions = [];
  const summary = routineSummary("opaque");
  snapshot.routines.details.opaque = { status: "ready", error: null, detail: { summary, pinnedDefinition: routineDefinition(), operations: [{ version: 2, cursor: "9007199254740999", id: "operation", invocationId: "opaque", name: "send", status: "uncertain", arguments: '{"target":"x"}', result: null, updatedMs: 1n, observationRevision: "2" }], nextOperationCursor: "9007199254740999" } };
  const commands: ClientCommand[] = [];
  renderWithAppState(() => <RoutinesRouteWithRouter />, { routePath: "/routines/opaque", snapshot, commands });
  expect(screen.getByText("Pinned historical definition")).toBeTruthy();
  expect(screen.getByText(/send · Uncertain/)).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Request cancellation" }));
  expect(commands).toContainEqual(expect.objectContaining({ type: "routineRequest", command: { type: "cancel", invocationId: "opaque" } }));
  expect(snapshot.routines.details.opaque.detail!.summary.status).toBe("running");
});

test("closed parameter dialect supports nullable, arrays, objects and preserves exact JSON", () => {
  const schema = routineDefinition().parameters.schema;
  expect(initialParameters(schema)).toContain('"direction": null');
  expect(validateParameters('{"direction":null,"options":[true,false]}', schema)).toBeUndefined();
  expect(validateParameters('{"direction":1}', schema)).toContain("expected string");
  expect(validateParameters('{"direction":null,"extra":1}', schema)).toContain("unknown property");
  expect(validateParameters('[]', schema)).toContain("expected object");
  expect(validateParameters('{"direction":null,"options":[null]}', schema)).toContain("expected boolean");
});
