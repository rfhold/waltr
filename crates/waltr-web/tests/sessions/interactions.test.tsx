import { fireEvent, screen, waitFor, within } from "@solidjs/testing-library";
import { createMemo, createSignal } from "solid-js";
import { describe, expect, test, vi } from "vitest";
import type { ClientCommand, ClientSnapshot, InteractionFormField, InteractionRequestView, PendingInteractionView } from "../../src/generated/waltr-client";
import { SessionInteractions } from "../../src/routes/sessions/SessionInteractions";
import { useAppState } from "../../src/app/context";
import { formValues, initialFormDraft, questionAnswers } from "../../src/routes/sessions/interaction-drafts";
import { renderWithAppState } from "../support/render";
import { emptyConfig, snapshotWithConfig } from "../support/snapshots";

function pending(request: InteractionRequestView, id = "request", generation = "server-1", operationId = "operation-1"): PendingInteractionView {
  return { identity: { sessionId: "s", generation, id, operationId }, request, responding: false, error: null };
}
function snapshot(...requests: PendingInteractionView[]): ClientSnapshot {
  return { ...snapshotWithConfig(emptyConfig()), interactions: { s: { generation: "server-1", revision: 1n, connected: true, error: null, pending: requests } } };
}
const permission: InteractionRequestView = { kind: "permission", message: "Read outside workspace", identity: "read", claims: ['{"path":"../private/note"}'], suggestions: ['{"path":"../private/**"}', '{"path":"../other/**"}'] };
const fields: InteractionFormField[] = [
  { name: "enabled", title: "Enabled", description: "Explicit choice", required: true, default: false, choiceTitles: {}, schema: { kind: "boolean" } },
  { name: "count", title: "Count", description: null, required: true, default: 2, choiceTitles: {}, schema: { kind: "number", integer: true, minimum: 1, maximum: 5 } },
  { name: "email", title: "Email", description: null, required: true, default: null, choiceTitles: {}, schema: { kind: "string", choices: null, minLength: 3n, maxLength: 40n, format: "email" } },
  { name: "mode", title: "Mode", description: null, required: true, default: "a", choiceTitles: { a: "Mode A", b: "Mode B" }, schema: { kind: "string", choices: ["a", "b"], minLength: null, maxLength: null, format: null } },
  { name: "tags", title: "Tags", description: null, required: true, default: ["red"], choiceTitles: { red: "Red tag", blue: "Blue tag" }, schema: { kind: "stringArray", choices: ["red", "blue"], minItems: 1n, maxItems: 2n } },
];

describe("server-owned session interactions", () => {
  test("snapshot-backed selection retains by session ID, not snapshot object identity", async () => {
    const commands: ClientCommand[] = [];
    let publish!: (snapshot: ClientSnapshot) => void;
    let select!: (id: string) => void;
    const request = pending(permission);
    let feedbackUpdates = 0;
    const rendered = renderWithAppState(() => {
      const app = useAppState();
      const [id, setId] = createSignal("s");
      select = setId;
      const selected = createMemo(() => ({ id: id(), snapshot: app.snapshot() }));
      return <><SessionInteractions sessionId={selected().id} /><SessionInteractions sessionId="s" /></>;
    }, {
      snapshot: snapshot(request), commands,
      onSnapshotSetter: setter => { publish = setter; },
      onDispatch(command, current, setSnapshot) {
        if ((command.type === "retainInteractions" || command.type === "releaseInteractions") && feedbackUpdates++ < 8) {
          setSnapshot({ ...current! });
        }
        return current!;
      },
    });
    expect(commands).toEqual([{ type: "retainInteractions", sessionId: "s" }, { type: "retainInteractions", sessionId: "s" }]);
    for (let i = 0; i < 4; i++) publish(snapshot(request));
    expect(commands).toHaveLength(2);
    fireEvent.click(screen.getAllByRole("button", { name: "Allow once" })[0]!);
    await waitFor(() => expect(commands).toContainEqual({ type: "respondInteraction", identity: request.identity, response: { kind: "permission", decision: "allowOnce", suggestionIndices: [], message: null } }));
    select("other");
    expect(commands.slice(-2)).toEqual([{ type: "releaseInteractions", sessionId: "s" }, { type: "retainInteractions", sessionId: "other" }]);
    rendered.unmount();
    expect(commands.filter(c => c.type === "retainInteractions" && c.sessionId === "s")).toHaveLength(2);
    expect(commands.filter(c => c.type === "releaseInteractions" && c.sessionId === "s")).toHaveLength(2);
    expect(commands.filter(c => c.type === "releaseInteractions" && c.sessionId === "other")).toHaveLength(1);
    expect(commands.filter(c => c.type === "respondInteraction")).toHaveLength(1);
    expect(commands.some(c => c.type === "interruptSession")).toBe(false);
  });

  test("permission renders exact claims and sends only authoritative selected scope indices", async () => {
    const commands: ClientCommand[] = [];
    const request = pending(permission);
    renderWithAppState(() => <SessionInteractions sessionId="s" />, { snapshot: snapshot(request), commands });
    expect(screen.getByText('{"path":"../private/note"}')).toBeTruthy();
    const always = screen.getByRole("button", { name: "Allow selected scopes always" }) as HTMLButtonElement;
    expect(always.disabled).toBe(true);
    fireEvent.click(screen.getAllByRole("checkbox")[1]!);
    fireEvent.click(always);
    await waitFor(() => expect(commands).toContainEqual({ type: "respondInteraction", identity: request.identity, response: { kind: "permission", decision: "allowAlways", suggestionIndices: [1], message: null } }));
    expect(screen.getByLabelText("permission interaction")).toBeTruthy();
    expect(commands.some(command => command.type === "interruptSession")).toBe(false);
  });

  test.each([ ["Allow once", "allowOnce"], ["Deny", "deny"], ["Cancel work", "cancelWork"] ] as const)("%s dispatches its typed permission decision without a second interrupt", async (label, decision) => {
    const commands: ClientCommand[] = [];
    const request = pending(permission);
    renderWithAppState(() => <SessionInteractions sessionId="s" />, { snapshot: snapshot(request), commands });
    fireEvent.click(screen.getByRole("button", { name: label }));
    await waitFor(() => expect(commands).toContainEqual({ type: "respondInteraction", identity: request.identity, response: { kind: "permission", decision, suggestionIndices: [], message: null } }));
    expect(commands.filter(c => c.type === "respondInteraction")).toHaveLength(1);
  });

  test("questions are indexed, support duplicate headers, single custom replacement and multiple choices", async () => {
    const commands: ClientCommand[] = [];
    const request = pending({ kind: "question", questions: [
      { header: "Choice", question: "First?", options: [{ label: "A", description: "first option" }], multiple: false, custom: true },
      { header: "Choice", question: "Second?", options: [{ label: "B", description: "second option" }, { label: "C", description: "third option" }], multiple: true, custom: false },
    ] });
    renderWithAppState(() => <SessionInteractions sessionId="s" />, { snapshot: snapshot(request), commands });
    fireEvent.click(screen.getByRole("button", { name: "Submit answers" }));
    expect(screen.getByText(/Answer each question/)).toBeTruthy();
    expect(commands.some(c => c.type === "respondInteraction")).toBe(false);
    fireEvent.click(screen.getByRole("radio"));
    fireEvent.input(screen.getByLabelText("Custom answer"), { target: { value: "Custom" } });
    fireEvent.click(screen.getAllByRole("checkbox")[0]!);
    fireEvent.click(screen.getAllByRole("checkbox")[1]!);
    fireEvent.click(screen.getByRole("button", { name: "Submit answers" }));
    await waitFor(() => expect(commands).toContainEqual({ type: "respondInteraction", identity: request.identity, response: { kind: "question", answers: [["Custom"], ["B", "C"]] } }));
  });

  test("form defaults and controls preserve boolean, number, enum and array types", async () => {
    const commands: ClientCommand[] = [];
    const request = pending({ kind: "form", message: "Complete fields", fields });
    renderWithAppState(() => <SessionInteractions sessionId="s" />, { snapshot: snapshot(request), commands });
    expect((screen.getByLabelText("Enabled (required)") as HTMLSelectElement).value).toBe("false");
    expect(screen.getByRole("option", { name: "Mode A" })).toBeTruthy();
    fireEvent.input(screen.getByLabelText("Count (required)"), { target: { value: "3.5" } });
    fireEvent.input(screen.getByLabelText("Email (required)"), { target: { value: "a@example.test" } });
    fireEvent.click(screen.getByRole("button", { name: "Accept" }));
    expect(screen.getByText(/finite integer/)).toBeTruthy();
    fireEvent.input(screen.getByLabelText("Count (required)"), { target: { value: "3" } });
    fireEvent.click(screen.getByRole("checkbox", { name: "Blue tag" }));
    fireEvent.click(screen.getByRole("button", { name: "Accept" }));
    await waitFor(() => expect(commands).toContainEqual({ type: "respondInteraction", identity: request.identity, response: { kind: "form", action: "accept", values: { enabled: false, count: 3, email: "a@example.test", mode: "a", tags: ["red", "blue"] } } }));
  });

  test("unsupported schema never offers accept and decline carries no draft values", async () => {
    const commands: ClientCommand[] = [];
    const request = pending({ kind: "unsupported", form: true, message: "Nested objects are unsupported" });
    renderWithAppState(() => <SessionInteractions sessionId="s" />, { snapshot: snapshot(request), commands });
    expect(screen.queryByRole("button", { name: "Accept" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Decline" }));
    await waitFor(() => expect(commands).toContainEqual({ type: "respondInteraction", identity: request.identity, response: { kind: "form", action: "decline", values: {} } }));
  });

  test.each(["Accept", "Decline", "Cancel request"] as const)("URL %s is consent only, never an opener, fetch or navigation", async label => {
    const commands: ClientCommand[] = [];
    const opener = vi.fn();
    const request = pending({ kind: "urlConsent", message: "External step", url: "https://service.example/path?step=full#visible" });
    const rendered = renderWithAppState(() => <SessionInteractions sessionId="s" />, { snapshot: snapshot(request), commands, openExternalUrl: opener });
    expect(screen.getByText("https://service.example/path?step=full#visible")).toBeTruthy();
    expect(screen.queryByRole("link")).toBeNull();
    expect(opener).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: label }));
    await waitFor(() => expect(commands).toContainEqual({ type: "respondInteraction", identity: request.identity, response: { kind: "urlConsent", action: label === "Accept" ? "accept" : label === "Decline" ? "decline" : "cancel" } }));
    rendered.unmount();
    expect(commands.at(-1)).toEqual({ type: "releaseInteractions", sessionId: "s" });
    expect(opener).not.toHaveBeenCalled();
  });

  test("multiple pending requests have independent busy and server errors; authoritative removal wins", async () => {
    let publish!: (snapshot: ClientSnapshot) => void;
    const first = { ...pending(permission, "one"), responding: true };
    const second = { ...pending(permission, "two"), error: "Stale response conflict" };
    const state = snapshot(first, second);
    renderWithAppState(() => <SessionInteractions sessionId="s" />, { snapshot: state, onSnapshotSetter: setter => { publish = setter; } });
    const cards = screen.getAllByLabelText("permission interaction");
    expect((within(cards[0]!).getByRole("button", { name: "Allow once" }) as HTMLButtonElement).disabled).toBe(true);
    expect((within(cards[1]!).getByRole("button", { name: "Allow once" }) as HTMLButtonElement).disabled).toBe(false);
    expect(screen.getByText("Stale response conflict")).toBeTruthy();
    publish(snapshot(second));
    await waitFor(() => expect(screen.getAllByLabelText("permission interaction")).toHaveLength(1));
    publish({ ...state, interactions: { s: { ...state.interactions.s!, connected: false, error: "Watch unavailable", pending: [second] } } });
    expect((screen.getByRole("button", { name: "Allow once" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("Watch unavailable")).toBeTruthy();
  });

  test("replacement generation/operation resets drafts and late command failure cannot affect its successor", async () => {
    let publish!: (snapshot: ClientSnapshot) => void;
    let reject!: (reason: unknown) => void;
    const state = snapshot(pending(permission));
    renderWithAppState(() => <SessionInteractions sessionId="s" />, {
      snapshot: state, onSnapshotSetter: setter => { publish = setter; },
      onDispatch(command, current) { return command.type === "respondInteraction" ? new Promise<ClientSnapshot>((_resolve, fail) => { reject = fail; }) : current!; },
    });
    fireEvent.input(screen.getByLabelText("Response message"), { target: { value: "old draft" } });
    fireEvent.click(screen.getByRole("button", { name: "Allow once" }));
    publish(snapshot(pending(permission, "request", "server-2", "operation-2")));
    await waitFor(() => expect((screen.getByLabelText("Response message") as HTMLTextAreaElement).value).toBe(""));
    reject(new Error("late failure"));
    await Promise.resolve();
    await Promise.resolve();
    expect(screen.queryByText(/Unable to send response/)).toBeNull();
    expect((screen.getByRole("button", { name: "Allow once" }) as HTMLButtonElement).disabled).toBe(false);
  });

  test("authoritative removal updates both viewers and reconnect never replays a response", async () => {
    let publish!: (snapshot: ClientSnapshot) => void;
    const commands: ClientCommand[] = [];
    const state = snapshot(pending(permission));
    renderWithAppState(() => <><SessionInteractions sessionId="s" /><SessionInteractions sessionId="s" /></>, { snapshot: state, commands, onSnapshotSetter: setter => { publish = setter; } });
    fireEvent.click(screen.getAllByRole("button", { name: "Allow once" })[0]!);
    await waitFor(() => expect(commands.filter(c => c.type === "respondInteraction")).toHaveLength(1));
    const busy = { ...state, interactions: { s: { ...state.interactions.s!, pending: [{ ...state.interactions.s!.pending[0]!, responding: true }] } } };
    publish(busy);
    expect(screen.getAllByRole("button", { name: "Allow once" }).every(button => (button as HTMLButtonElement).disabled)).toBe(true);
    publish({ ...state, interactions: { s: { ...state.interactions.s!, connected: false } } });
    publish(state);
    publish(snapshot());
    await waitFor(() => expect(screen.queryByLabelText("permission interaction")).toBeNull());
    expect(commands.filter(c => c.type === "respondInteraction")).toHaveLength(1);
  });

  test("changed descriptor under the same identity resets approval selections and drafts", async () => {
    let publish!: (snapshot: ClientSnapshot) => void;
    renderWithAppState(() => <SessionInteractions sessionId="s" />, { snapshot: snapshot(pending(permission)), onSnapshotSetter: setter => { publish = setter; } });
    fireEvent.click(screen.getAllByRole("checkbox")[0]!);
    fireEvent.input(screen.getByLabelText("Response message"), { target: { value: "old" } });
    publish(snapshot(pending({ ...permission, message: "Changed scope descriptor" })));
    await waitFor(() => expect((screen.getByLabelText("Response message") as HTMLTextAreaElement).value).toBe(""));
    expect((screen.getByRole("button", { name: "Allow selected scopes always" }) as HTMLButtonElement).disabled).toBe(true);
  });

  test.each(["Decline", "Cancel request"] as const)("form %s ignores entered values", async label => {
    const commands: ClientCommand[] = [];
    const request = pending({ kind: "form", message: "Form", fields });
    renderWithAppState(() => <SessionInteractions sessionId="s" />, { snapshot: snapshot(request), commands });
    fireEvent.input(screen.getByLabelText("Email (required)"), { target: { value: "invalid" } });
    fireEvent.click(screen.getByRole("button", { name: label }));
    await waitFor(() => expect(commands).toContainEqual({ type: "respondInteraction", identity: request.identity, response: { kind: "form", action: label === "Decline" ? "decline" : "cancel", values: {} } }));
  });

  test("two observers retain independently; navigation and unmount only release, without responses or interruption", async () => {
    const commands: ClientCommand[] = [];
    let navigate!: (sessionId: string) => void;
    const rendered = renderWithAppState(() => {
      const [id, setId] = createSignal("s");
      navigate = setId;
      return <><SessionInteractions sessionId={id()} /><SessionInteractions sessionId="s" /></>;
    }, { snapshot: snapshot(pending(permission)), commands });
    expect(commands).toEqual([{ type: "retainInteractions", sessionId: "s" }, { type: "retainInteractions", sessionId: "s" }]);
    navigate("other");
    await waitFor(() => expect(commands).toContainEqual({ type: "releaseInteractions", sessionId: "s" }));
    expect(commands.at(-1)).toEqual({ type: "retainInteractions", sessionId: "other" });
    rendered.unmount();
    expect(commands.filter(c => c.type === "releaseInteractions" && c.sessionId === "s")).toHaveLength(2);
    expect(commands.every(c => c.type === "retainInteractions" || c.type === "releaseInteractions")).toBe(true);
  });
});

describe("interaction draft validation", () => {
  test("required false is a value, missing is not; numeric bounds/integer and array counts are enforced", () => {
    const draft = { ...initialFormDraft(fields), email: "a@example.test" };
    expect(formValues(fields, draft)).toEqual({ enabled: false, count: 2, email: "a@example.test", mode: "a", tags: ["red"] });
    for (const count of [0, 6, 2.5, "Infinity", ""]) expect(typeof formValues(fields, { ...draft, count })).toBe("string");
    for (const tags of [[], ["invalid"], ["red", "red"], ["red", "blue", "red"]]) expect(typeof formValues(fields, { ...draft, tags })).toBe("string");
    expect(typeof formValues(fields, { ...draft, enabled: undefined })).toBe("string");
    expect(typeof formValues(fields, { ...draft, mode: "not-offered" })).toBe("string");
  });

  test.each([
    ["email", "a@example.test", "bad"], ["uri", "https://example.test/a", "relative/path"],
    ["date", "2026-10-28", "2026-02-30"], ["date-time", "2026-10-28T12:30:00Z", "2026-10-28T12:30:00"],
  ])("validates %s and Unicode character length", (format, valid, invalid) => {
    const field: InteractionFormField = { ...fields[2]!, schema: { kind: "string", choices: null, minLength: 1n, maxLength: 80n, format: format! } };
    expect(typeof formValues([field], { email: valid })).toBe("object");
    expect(typeof formValues([field], { email: invalid })).toBe("string");
    expect(typeof formValues([{ ...field, schema: { kind: "string", choices: null, minLength: 1n, maxLength: 1n, format: null } }], { email: "😀" })).toBe("object");
  });

  test("question cardinality and custom answers are validated without merging duplicate headers", () => {
    const question = { header: "same", question: "?", options: [{ label: "a", description: "" }], multiple: false, custom: true };
    expect(questionAnswers([question, question], [["a"], []], ["", "custom"])).toEqual([["a"], ["custom"]]);
    expect(typeof questionAnswers([question], [["a"]], ["custom"])).toBe("string");
    expect(typeof questionAnswers([question], [[]], [" "])).toBe("string");
  });

  test("optional fields omit cleanly, text bounds and decimal number bounds remain typed", () => {
    const optional = { ...fields[2]!, required: false };
    expect(formValues([optional], {})).toEqual({});
    expect(typeof formValues([optional], { email: "x" })).toBe("string");
    expect(typeof formValues([optional], { email: "a".repeat(41) })).toBe("string");
    const decimal: InteractionFormField = { ...fields[1]!, schema: { kind: "number", integer: false, minimum: 1, maximum: 5 } };
    expect(formValues([decimal], { count: "2.5" })).toEqual({ count: 2.5 });
    const emptyArray: InteractionFormField = { ...fields[4]!, schema: { kind: "stringArray", choices: ["red"], minItems: 0n, maxItems: 1n } };
    expect(formValues([emptyArray], { tags: [] })).toEqual({ tags: [] });
  });
});
