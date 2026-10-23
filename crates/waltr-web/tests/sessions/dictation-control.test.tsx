import { cleanup, fireEvent, render, screen, waitFor } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { afterEach, expect, test, vi } from "vitest";
import { DictationControl } from "../../src/components/DictationControl";
import type { DictationStatus, RoutineSelection } from "../../src/platform/dictation";
afterEach(cleanup);
const selection = { routine: "r", trigger: "t", input: "i", output: "o" };
function fixture() {
  let state: DictationStatus = { operationId: 1, phase: "starting", preview: "" };
  const cancel = vi.fn();
  const operation = { operationId: 1, status: vi.fn(async () => state), finish: vi.fn(async () => undefined), cancel };
  const host = { start: vi.fn(() => operation), cancel, dispose: vi.fn() };
  const insert = vi.fn();
  let configure!: (value: RoutineSelection | undefined) => void;
  let revise!: (value: number) => void;
  let ready!: (value: boolean) => void;
  let version!: (value: string) => void;
  let invalidate!: (value: number) => void;
  render(() => {
    const [config, setConfig] = createSignal<RoutineSelection | undefined>(selection);
    const [revision, setRevision] = createSignal(0);
    const [isReady, setReady] = createSignal(true);
    const [etag, setEtag] = createSignal("v1");
    const [generation, setGeneration] = createSignal(0);
    configure = setConfig; revise = setRevision; ready = setReady; version = setEtag; invalidate = setGeneration;
    return <DictationControl host={host} scope="server" sessionId="s" ready={isReady()} configurationVersion={etag()} configuredSelection={config()} revision={revision()} selection={{ start: 0, end: 0 }} invalidation={generation()} insert={insert} />;
  });
  const button = screen.getByRole("button", { name: "Hold to dictate" });
  return { button, host, operation, insert, configure, revise, ready, version, invalidate, state: (value: Partial<DictationStatus>) => { state = { ...state, ...value }; } };
}
test("release during startup finishes once; click does not start and processing cannot reenter", () => {
  const f = fixture();
  fireEvent.click(f.button); expect(f.host.start).not.toHaveBeenCalled();
  fireEvent.pointerDown(f.button, { pointerId: 1, button: 0 });
  expect(f.host.start).toHaveBeenCalledWith(selection, "s");
  fireEvent.pointerUp(f.button, { pointerId: 1 });
  fireEvent.lostPointerCapture(f.button, { pointerId: 1 });
  fireEvent.pointerDown(f.button, { pointerId: 2, button: 0 });
  expect(f.operation.finish).toHaveBeenCalledOnce();
  expect(f.operation.cancel).not.toHaveBeenCalled();
  expect(f.host.start).toHaveBeenCalledOnce();
});
test.each([" ", "Enter"])("%s keyboard press/release suppresses repeat and generated click", key => {
  const f = fixture();
  fireEvent.keyDown(f.button, { key });
  fireEvent.keyDown(f.button, { key, repeat: true });
  fireEvent.keyUp(f.button, { key });
  fireEvent.click(f.button);
  expect(f.host.start).toHaveBeenCalledOnce(); expect(f.operation.finish).toHaveBeenCalledOnce();
});
test.each(["pointercancel", "lostcapture", "blur", "windowblur", "config", "access", "generation", "unmount"])("%s cancels active operation", async action => {
  const f = fixture(); fireEvent.pointerDown(f.button, { pointerId: 1, button: 0 });
  if (action === "pointercancel") fireEvent.pointerCancel(f.button);
  if (action === "lostcapture") fireEvent.lostPointerCapture(f.button);
  if (action === "blur") fireEvent.blur(f.button);
  if (action === "windowblur") fireEvent(window, new Event("blur"));
  if (action === "config") f.configure({ ...selection, routine: "new" });
  if (action === "access") f.ready(false);
  if (action === "generation") f.invalidate(1);
  if (action === "unmount") cleanup();
  await waitFor(() => expect(f.operation.cancel).toHaveBeenCalledOnce());
  f.state({ phase: "succeeded", finalText: "stale" });
  expect(f.insert).not.toHaveBeenCalled();
});
test("equivalent snapshot projections preserve capture; a new config ETag cancels it", () => {
  const f = fixture(); fireEvent.keyDown(f.button, { key: " " });
  f.configure({ ...selection });
  expect(f.operation.cancel).not.toHaveBeenCalled();
  f.version("v2");
  expect(f.operation.cancel).toHaveBeenCalledOnce();
});
test("absent config disables microphone without capture or local preferences", () => {
  const f = fixture(); f.configure(undefined);
  expect((f.button as HTMLButtonElement).disabled).toBe(true);
  fireEvent.pointerDown(f.button, { pointerId: 1, button: 0 });
  expect(f.host.start).not.toHaveBeenCalled();
  expect(screen.queryByRole("textbox")).toBeNull();
});
test("stale draft is retained for insert/discard; config change discards it", async () => {
  const f = fixture(); fireEvent.keyDown(f.button, { key: " " }); fireEvent.keyUp(f.button, { key: " " });
  f.revise(1); f.state({ phase: "succeeded", finalText: "text" });
  await screen.findByRole("button", { name: "Insert dictation" });
  expect(f.insert).not.toHaveBeenCalled();
  f.configure({ ...selection, output: "other" });
  expect(screen.queryByRole("button", { name: "Insert dictation" })).toBeNull();
});
test("late finish failure cannot cancel a replacement operation", async () => {
  const f = fixture();
  let reject!: (reason: Error) => void;
  const pending = new Promise<undefined>((_, no) => { reject = no; });
  f.operation.finish.mockReturnValueOnce(pending);
  fireEvent.keyDown(f.button, { key: "Enter" }); fireEvent.keyUp(f.button, { key: "Enter" });
  f.ready(false); f.ready(true);
  fireEvent.keyDown(f.button, { key: "Enter" });
  expect(f.host.start).toHaveBeenCalledTimes(2);
  reject(new Error("late")); await pending.catch(() => undefined); await Promise.resolve();
  expect(f.operation.cancel).toHaveBeenCalledOnce();
  expect(screen.queryByRole("alert")).toBeNull();
});
test("finish failure cancels only the owning operation and reports error", async () => {
  const f = fixture(); f.operation.finish.mockRejectedValueOnce(new Error("failure"));
  fireEvent.keyDown(f.button, { key: "Enter" }); fireEvent.keyUp(f.button, { key: "Enter" });
  await screen.findByRole("alert"); expect(f.operation.cancel).toHaveBeenCalledOnce();
});
