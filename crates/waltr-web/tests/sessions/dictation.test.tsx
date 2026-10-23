import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { expect, test, vi } from "vitest";
import { renderWithAppState } from "../support/render";
import { SessionsRouteWithRouter, testHistory } from "../support/routes";
import { snapshotWithSessions, sessionView, readySessionTimeline } from "../support/snapshots";
import { type DictationStatus } from "../../src/platform/dictation";
import { browserDictation } from "../../src/platform/browser-dictation";
function fixture(extra: Parameters<typeof renderWithAppState>[1] = {}) {
  const configuredSelection = { routine: "transcribe", trigger: "audio", input: "audio", output: "text" };
  let state: DictationStatus = { operationId: 1, phase: "recording", preview: "" };
  const operation = { operationId: 1, status: vi.fn(async () => state), finish: vi.fn(async () => undefined), cancel: vi.fn() };
  const host = { livePreview: true, start: vi.fn(() => operation), cancel: vi.fn(), dispose: vi.fn() };
  const snapshot = snapshotWithSessions([sessionView({ id: "s" }), sessionView({ id: "other" })]);
  snapshot.config.current = { etag: "dictation", providers: {}, mcpServers: {}, inferenceDefaults: null, sessionDictation: configuredSelection };
  snapshot.sessions.timelines.s = readySessionTimeline(); snapshot.sessions.timelines.other = readySessionTimeline();
  const commands: unknown[] = [];
  let setSnapshot!: (value: typeof snapshot) => void;
  const rendered = renderWithAppState(() => <SessionsRouteWithRouter />, { routePath: "/sessions/s", snapshot, commands: commands as never[], dictation: host, onSnapshotSetter: setter => { setSnapshot = setter; }, ...extra });
  const draft = screen.getByRole("textbox", { name: "Session message" }) as HTMLTextAreaElement;
  return { ...rendered, host, operation, draft, commands, updateSnapshot: (update: (value: typeof snapshot) => typeof snapshot) => setSnapshot(update(snapshot)), state: (next: Partial<DictationStatus>) => { state = { ...state, ...next }; } };
}
test("live revisions replace the captured selection and finalize without duplicates or sending", async () => {
  const f = fixture();
  fireEvent.input(f.draft, { target: { value: "hello world", selectionStart: 6, selectionEnd: 11 } });
  fireEvent.keyDown(screen.getByRole("button", { name: "Hold to dictate" }), { key: " " });
  expect(f.host.start).toHaveBeenCalledOnce();
  f.state({ preview: "interim" });
  expect(screen.queryByLabelText("Dictation preview")).toBeNull();
  await waitFor(() => expect(f.draft.value).toBe("hello interim"));
  f.state({ preview: "interim revised" });
  await waitFor(() => expect(f.draft.value).toBe("hello interim revised"));
  f.state({ preview: "" });
  await waitFor(() => expect(f.draft.value).toBe("hello "));
  fireEvent.keyUp(screen.getByRole("button", { name: "Hold to dictate" }), { key: " " });
  f.state({ phase: "succeeded", finalText: "there", preview: "there" });
  await waitFor(() => expect(f.draft.value).toBe("hello there"));
  expect(f.commands.some(command => (command as { type: string }).type === "submitUserMessage")).toBe(false);
});
test("draft edits retain final result for explicit insert at current selection", async () => {
  const f = fixture();
  fireEvent.keyDown(screen.getByRole("button", { name: "Hold to dictate" }), { key: " " });
  fireEvent.input(f.draft, { target: { value: "typed ", selectionStart: 6, selectionEnd: 6 } });
  f.state({ phase: "succeeded", finalText: "speech" });
  fireEvent.click(await screen.findByRole("button", { name: "Insert dictation" }));
  expect(f.draft.value).toBe("typed speech");
});
test("moving the caret fences automatic insertion even when the draft text is unchanged", async () => {
  const f = fixture();
  fireEvent.input(f.draft, { target: { value: "draft", selectionStart: 5, selectionEnd: 5 } });
  fireEvent.keyDown(screen.getByRole("button", { name: "Hold to dictate" }), { key: " " });
  f.draft.setSelectionRange(0, 0); fireEvent.select(f.draft);
  f.state({ phase: "succeeded", finalText: "speech " });
  await screen.findByRole("button", { name: "Insert dictation" });
  expect(f.draft.value).toBe("draft");
});
test.each(["cancel", "navigation", "session"])("%s invalidates pending output and stale late success never inserts", async exit => {
  const f = fixture();
  fireEvent.input(f.draft, { target: { value: "draft", selectionStart: 5, selectionEnd: 5 } });
  fireEvent.keyDown(screen.getByRole("button", { name: "Hold to dictate" }), { key: " " });
  await waitFor(() => expect(f.operation.status).toHaveBeenCalled());
  if (exit === "cancel") fireEvent.blur(screen.getByRole("button", { name: "Hold to dictate" }));
  if (exit === "navigation") testHistory().set({ value: "/sessions", replace: false, scroll: false });
  if (exit === "session") testHistory().set({ value: "/sessions/other", replace: false, scroll: false });
  await waitFor(() => expect(f.operation.cancel).toHaveBeenCalledOnce());
  f.state({ phase: "succeeded", finalText: "stale" });
  await new Promise(resolve => setTimeout(resolve, 120));
  expect(screen.queryByRole("button", { name: "Insert dictation" })).toBeNull();
  expect((screen.queryByRole("textbox", { name: "Session message" }) as HTMLTextAreaElement | null)?.value ?? "").not.toContain("stale");
});
test.each(["error", "missingFinal"])("%s output with preview never inserts", async failure => {
  const f = fixture(); fireEvent.keyDown(screen.getByRole("button", { name: "Hold to dictate" }), { key: " " });
  f.state({ phase: failure === "error" ? "error" : "succeeded", preview: "uncommitted", finalText: failure === "error" ? "untrusted" : null });
  await screen.findByRole("alert");
  expect(f.draft.value).toBe("");
  expect(screen.queryByRole("button", { name: "Insert dictation" })).toBeNull();
});
test.each(["cancel", "session", "configuration"])("%s removes owned preview and rejects late output", async exit => {
  const f = fixture();
  fireEvent.input(f.draft, { target: { value: "original", selectionStart: 0, selectionEnd: 8 } });
  const mic = screen.getByRole("button", { name: "Hold to dictate" });
  fireEvent.keyDown(mic, { key: " " });
  f.state({ preview: "provisional" });
  await waitFor(() => expect(f.draft.value).toBe("provisional"));
  if (exit === "cancel") fireEvent.blur(mic);
  if (exit === "session") testHistory().set({ value: "/sessions/other", replace: false, scroll: false });
  if (exit === "configuration") f.updateSnapshot(current => ({ ...current, config: { ...current.config, current: { ...current.config.current!, etag: "changed" } } }));
  await waitFor(() => expect(f.operation.cancel).toHaveBeenCalledOnce());
  f.state({ phase: "succeeded", finalText: "stale" });
  await new Promise(resolve => setTimeout(resolve, 120));
  expect((screen.getByRole("textbox", { name: "Session message" }) as HTMLTextAreaElement).value).not.toMatch(/provisional|stale/);
});
test("failure preserves user edits after a provisional update", async () => {
  const f = fixture();
  fireEvent.keyDown(screen.getByRole("button", { name: "Hold to dictate" }), { key: " " });
  f.state({ preview: "provisional" });
  await waitFor(() => expect(f.draft.value).toBe("provisional"));
  fireEvent.input(f.draft, { target: { value: "edited provisional" } });
  f.state({ phase: "error" });
  await screen.findByRole("alert");
  expect(f.draft.value).toBe("edited provisional");
});
test.each(["active", "succeeded"])("late submission completion preserves %s dictation", async phase => {
  let complete!: (value: never) => void;
  const submitted = new Promise<never>(resolve => { complete = resolve; });
  const submitUserMessage = vi.fn(() => submitted);
  const f = fixture({ submitUserMessage });
  fireEvent.input(f.draft, { target: { value: "first", selectionStart: 0, selectionEnd: 5 } });
  fireEvent.click(screen.getByRole("button", { name: "Send" }));
  expect(submitUserMessage).toHaveBeenCalledOnce();
  const mic = screen.getByRole("button", { name: "Hold to dictate" });
  fireEvent.keyDown(mic, { key: " " });
  f.state({ preview: "speech" });
  await waitFor(() => expect(f.draft.value).toBe("speech"));
  if (phase === "succeeded") {
    fireEvent.keyUp(mic, { key: " " });
    f.state({ phase: "succeeded", finalText: "final speech" });
    await waitFor(() => expect(f.draft.value).toBe("final speech"));
  }
  complete(undefined as never); await submitted;
  await waitFor(() => expect(f.draft.value).toBe(phase === "succeeded" ? "final speech" : "speech"));
  if (phase === "active") {
    f.state({ preview: "new revision" });
    await waitFor(() => expect(f.draft.value).toBe("new revision"));
  }
});
test("late edit-message completion preserves a new dictation transaction", async () => {
  let complete!: (value: never) => void;
  const rollback = new Promise<never>(resolve => { complete = resolve; });
  const f = fixture({ onDispatch: (command, current) => command.type === "rollbackSessionBeforeCursor" ? rollback : current! });
  f.updateSnapshot(current => ({ ...current, sessions: { ...current.sessions, timelines: { ...current.sessions.timelines, s: {
    ...readySessionTimeline(), events: [{ entryKind: "durable", identity: "user-item", presentation: "durable", event: {
      cursor: 7n, sessionId: "s", turn: 1n, itemId: "user-item", parentId: null, kind: "itemUpdated", channel: "user_text",
      content: { kind: "textSnapshot", text: "old prompt" }, status: null, metadataJson: "{}",
    } }],
  } } } }));
  fireEvent.click(screen.getByRole("button", { name: "Edit" }));
  expect(f.commands).toContainEqual({ type: "rollbackSessionBeforeCursor", sessionId: "s", cursor: 7n });
  const mic = screen.getByRole("button", { name: "Hold to dictate" });
  fireEvent.keyDown(mic, { key: " " });
  f.state({ preview: "new speech" });
  await waitFor(() => expect(f.draft.value).toBe("new speech"));
  fireEvent.keyUp(mic, { key: " " });
  f.state({ phase: "succeeded", finalText: "final speech" });
  await waitFor(() => expect(f.draft.value).toBe("final speech"));
  complete(undefined as never); await rollback;
  await waitFor(() => expect(f.draft.value).toBe("final speech"));
});
test.each([false, true])("actual browser Run keeps hypotheses out of composer; edited=%s", async edited => {
  let state: DictationStatus = { operationId: 7, phase: "processing", preview: "browser hypothesis" };
  const bridge = {
    startAudioRun: vi.fn(() => 7),
    audioRunStatusJson: vi.fn(() => JSON.stringify(state)),
    cancelAudioRun: vi.fn(),
  };
  const capture = vi.fn(() => ({
    ready: Promise.resolve(), cancel: vi.fn(),
    stop: vi.fn(async () => ({ bytes: new Uint8Array([1, 2]), format: { encoding: "PCM_S16LE" as const, sampleRate: 24000 as const, channels: 1 as const } })),
  }));
  const f = fixture({ dictation: browserDictation(bridge, capture) });
  fireEvent.input(f.draft, { target: { value: "hello world", selectionStart: 6, selectionEnd: 11 } });
  const mic = screen.getByRole("button", { name: "Hold to dictate" });
  fireEvent.keyDown(mic, { key: " " }); fireEvent.keyUp(mic, { key: " " });
  await waitFor(() => expect(bridge.audioRunStatusJson).toHaveBeenCalled());
  expect(capture).toHaveBeenCalledOnce();
  expect(bridge.startAudioRun).toHaveBeenCalledOnce();
  expect(f.draft.value).toBe("hello world");
  if (edited) fireEvent.input(f.draft, { target: { value: "typed ", selectionStart: 6, selectionEnd: 6 } });
  state = { ...state, phase: "succeeded", finalText: "final speech" };
  if (edited) {
    fireEvent.click(await screen.findByRole("button", { name: "Insert dictation" }));
    expect(f.draft.value).toBe("typed final speech");
  } else await waitFor(() => expect(f.draft.value).toBe("hello final speech"));
  expect(f.commands.some(command => (command as { type: string }).type === "submitUserMessage")).toBe(false);
});
test("late successful submission cannot erase a newly edited draft", async () => {
  let complete!: (value: never) => void;
  const submitted = new Promise<never>(resolve => { complete = resolve; });
  const f = fixture({ submitUserMessage: vi.fn(() => submitted) });
  fireEvent.input(f.draft, { target: { value: "first" } });
  fireEvent.click(screen.getByRole("button", { name: "Send" }));
  fireEvent.input(f.draft, { target: { value: "new draft" } });
  complete(undefined as never); await submitted;
  await waitFor(() => expect(f.draft.value).toBe("new draft"));
});
test.each(["error", "cancelled"])("%s rolls back only the owned provisional selection", async phase => {
  const f = fixture();
  fireEvent.input(f.draft, { target: { value: "hello world", selectionStart: 6, selectionEnd: 11 } });
  fireEvent.keyDown(screen.getByRole("button", { name: "Hold to dictate" }), { key: " " });
  f.state({ preview: "speech" });
  await waitFor(() => expect(f.draft.value).toBe("hello speech"));
  f.state({ phase: phase as "error" | "cancelled" });
  await waitFor(() => expect(f.draft.value).toBe("hello world"));
});
test.each(["caret", "ime", "typing"])("%s detaches permanently and leaves edited provisional text for explicit replacement", async edit => {
  const f = fixture();
  fireEvent.keyDown(screen.getByRole("button", { name: "Hold to dictate" }), { key: " " });
  f.state({ preview: "speech" });
  await waitFor(() => expect(f.draft.value).toBe("speech"));
  if (edit === "caret") {
    f.draft.setSelectionRange(0, 0); fireEvent.select(f.draft);
    f.draft.setSelectionRange(6, 6); fireEvent.select(f.draft);
  } else if (edit === "ime") fireEvent.compositionStart(f.draft);
  else fireEvent.input(f.draft, { target: { value: "edited speech", selectionStart: 13, selectionEnd: 13 } });
  f.state({ preview: "revision", phase: "succeeded", finalText: "final" });
  await screen.findByRole("button", { name: "Insert dictation" });
  expect(f.draft.value).toBe(edit === "typing" ? "edited speech" : "speech");
  f.draft.setSelectionRange(0, f.draft.value.length); fireEvent.select(f.draft);
  fireEvent.click(screen.getByRole("button", { name: "Insert dictation" }));
  expect(f.draft.value).toBe("final");
});
test("send is blocked during capture and processing while previews continue after release", async () => {
  const f = fixture();
  const mic = screen.getByRole("button", { name: "Hold to dictate" });
  fireEvent.keyDown(mic, { key: " " });
  f.state({ preview: "partial" });
  await waitFor(() => expect(f.draft.value).toBe("partial"));
  expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
  fireEvent.submit(screen.getByRole("form", { name: "Send session message" }));
  expect(f.commands.some(command => (command as { type: string }).type === "submitUserMessage")).toBe(false);
  fireEvent.keyUp(mic, { key: " " });
  f.state({ preview: "processing revision" });
  await waitFor(() => expect(f.draft.value).toBe("processing revision"));
  f.state({ phase: "succeeded", finalText: "final" });
  await waitFor(() => expect(f.draft.value).toBe("final"));
  expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(false);
});
