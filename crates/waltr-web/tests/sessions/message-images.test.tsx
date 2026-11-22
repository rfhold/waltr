import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { afterEach, expect, test, vi } from "vitest";
import { sessionDrafts } from "../../src/app/session-draft";
import type { ClientSnapshot, ProjectedTimelineEntryView } from "../../src/generated/waltr-client";
import type { MessageImagesBridge } from "../../src/platform/message-images";
import { renderWithAppState } from "../support/render";
import { SessionsRouteWithRouter, testHistory } from "../support/routes";
import { readySessionTimeline, sessionView, snapshotWithSessions } from "../support/snapshots";

const signature = [137, 80, 78, 71, 13, 10, 26, 10];
function pngFixture(width: number, height: number, animated = false) {
  const image = new Uint8Array([...signature,
    0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 0, 0, 0, 0, 0, 8, 6, 0, 0, 0, 0, 0, 0, 0,
    ...(animated ? [0, 0, 0, 8, 97, 99, 84, 76, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0] : []),
    0, 0, 0, 1, 73, 68, 65, 84, 0, 0, 0, 0, 0, 0, 0, 0, 0, 73, 69, 78, 68, 0, 0, 0, 0,
  ]);
  const view = new DataView(image.buffer);
  view.setUint32(16, width);
  view.setUint32(20, height);
  return image;
}
const png = pngFixture(1, 1);
const file = () => new File([png], "private.png", { type: "image/png" });
const prompt = () => screen.getByRole("textbox", { name: "Session message" }) as HTMLTextAreaElement;
const pick = (image: File) => fireEvent.change(screen.getByLabelText("Choose images"), { target: { files: [image] } });
const sendButton = () => screen.getByRole("button", { name: "Send" });

function setup(events: ProjectedTimelineEntryView[] = [], accessLayout = false) {
  const snapshot = snapshotWithSessions([sessionView({ id: "alpha" }), sessionView({ id: "beta" })]);
  snapshot.sessions.timelines.alpha = { ...readySessionTimeline(), events };
  snapshot.sessions.timelines.beta = readySessionTimeline();
  const retirement = new AbortController();
  const bridge = {
    retired: retirement.signal,
    send: vi.fn<MessageImagesBridge["send"]>().mockResolvedValue(undefined),
    read: vi.fn<MessageImagesBridge["read"]>().mockResolvedValue(png),
    cancelSend: vi.fn<MessageImagesBridge["cancelSend"]>(),
    cancelAll: vi.fn<MessageImagesBridge["cancelAll"]>(),
  };
  const create = vi.spyOn(URL, "createObjectURL");
  const revoke = vi.spyOn(URL, "revokeObjectURL");
  let publish!: (value: ClientSnapshot) => void;
  const drafts = sessionDrafts();
  renderWithAppState(() => <SessionsRouteWithRouter accessLayout={accessLayout} />, {
    routePath: "/sessions/alpha", snapshot, messageImages: bridge, sessionDrafts: drafts, onSnapshotSetter: setter => { publish = setter; },
  });
  return { snapshot, bridge, publish, retirement, create, revoke, drafts };
}

afterEach(() => vi.restoreAllMocks());

test("picks an image, sends it without text, and revokes its successful draft preview", async () => {
  const { bridge, revoke } = setup();
  pick(file());
  const preview = await screen.findByRole("img", { name: "Attachment 1" });
  const source = preview.getAttribute("src");
  fireEvent.click(sendButton());
  await waitFor(() => expect(screen.queryByRole("img", { name: "Attachment 1" })).toBeNull());
  expect(bridge.send).toHaveBeenCalledExactlyOnceWith("alpha", [{ kind: "image", format: "png", bytes: png }]);
  expect(revoke).toHaveBeenCalledWith(source);
});

test("preserves the full mixed draft on upload failure and cancellation and removes its preview explicitly", async () => {
  const { bridge, revoke } = setup();
  const clipboardData = new DataTransfer();
  clipboardData.items.add(file());
  fireEvent.paste(prompt(), { clipboardData });
  fireEvent.input(prompt(), { target: { value: "  describe this  " } });
  const preview = await screen.findByRole("img", { name: "Attachment 1" });
  bridge.send.mockRejectedValueOnce("Unable to upload the image");
  fireEvent.click(sendButton());
  await screen.findByText("Unable to upload the image");
  expect(bridge.send).toHaveBeenCalledWith("alpha", [{ kind: "text", text: "describe this" }, { kind: "image", format: "png", bytes: png }]);
  expect(prompt().value).toBe("  describe this  ");
  expect(preview.isConnected).toBe(true);

  let finish!: (reason: Error) => void;
  bridge.send.mockImplementationOnce(() => new Promise((_resolve, reject) => { finish = reject; }));
  fireEvent.click(sendButton());
  await waitFor(() => expect(bridge.send).toHaveBeenCalledTimes(2));
  fireEvent.click(screen.getByRole("button", { name: "Cancel upload" }));
  expect(bridge.cancelSend).toHaveBeenCalledOnce();
  finish(new Error("Image send cancelled"));
  await screen.findByText("Image send cancelled");
  expect(preview.isConnected).toBe(true);
  fireEvent.click(screen.getByRole("button", { name: "Remove attachment 1" }));
  expect(revoke).toHaveBeenCalledWith(preview.getAttribute("src"));
  expect(prompt().value).toBe("  describe this  ");
});

test("rejects a non-raster drop without previewing or sending it", async () => {
  const { bridge, create } = setup();
  const dataTransfer = new DataTransfer();
  dataTransfer.items.add(new File(["<svg></svg>"], "untrusted.svg", { type: "image/svg+xml" }));
  fireEvent.drop(screen.getByRole("form", { name: "Send session message" }), { dataTransfer });
  await screen.findByText(/Use still PNG, JPEG, or WebP/);
  expect(create).not.toHaveBeenCalled();
  expect(bridge.send).not.toHaveBeenCalled();
});

function history(size = png.byteLength): ProjectedTimelineEntryView {
  return {
    entryKind: "durable", presentation: "durable", identity: "saved",
    event: {
      cursor: 2n, sessionId: "alpha", turn: 1n, itemId: "saved", parentId: null,
      kind: "itemUpdated", channel: "user_text", status: null, metadataJson: "{}",
      content: { kind: "userMessage", parts: [
        { kind: "text", text: "Before image" }, { kind: "image", format: "png", size_bytes: size },
        { kind: "text", text: "After image" },
      ] },
    },
  };
}

test("renders ordered history via its exact authorized part and revokes it on access loss", async () => {
  const { bridge, snapshot, publish, revoke } = setup([history()]);
  const image = await screen.findByRole("img", { name: "Message attachment" });
  expect(bridge.read).toHaveBeenCalledWith("alpha", "saved", 1, "png", png.byteLength);
  const before = await screen.findByText("Before image");
  const after = await screen.findByText("After image");
  expect(before.compareDocumentPosition(image) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(image.compareDocumentPosition(after) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  publish({ ...snapshot, access: { ...snapshot.access, status: "loginRequired" } });
  expect(bridge.cancelAll).toHaveBeenCalledOnce();
  expect(revoke).toHaveBeenCalledWith(image.getAttribute("src"));
  expect(screen.queryByRole("img", { name: "Message attachment" })).toBeNull();
});

test("refuses pixel bombs and APNG before creating draft or history object URLs", async () => {
  const animated = pngFixture(1, 1, true);
  const { bridge, create } = setup([history(animated.byteLength)]);
  bridge.read.mockResolvedValue(animated);
  await screen.findByText("Image unavailable");
  for (const bytes of [pngFixture(8192, 8192), animated]) {
    pick(new File([bytes], "unsafe.png", { type: "image/png" }));
    await screen.findByText(/Use still PNG, JPEG, or WebP/);
  }
  expect(create).not.toHaveBeenCalled();
});

test("access expiry unmounts the composer but restores only text for the same owner and route", async () => {
  const { bridge, snapshot, publish, drafts } = setup([], true);
  const expire = () => publish({ ...snapshot, access: { ...snapshot.access, status: "loginRequired" } });
  fireEvent.input(prompt(), { target: { value: "retain across access expiry" } });
  pick(file());
  await screen.findByRole("img", { name: "Attachment 1" });
  bridge.send.mockImplementationOnce(async () => { expire(); throw new Error("Waltr login required"); });
  fireEvent.click(sendButton());
  await waitFor(() => expect(screen.queryByRole("textbox", { name: "Session message" })).toBeNull());
  publish({ ...snapshot });
  expect(prompt().value).toBe("retain across access expiry");
  expect(screen.queryByRole("img", { name: "Attachment 1" })).toBeNull();

  expire();
  const selections = vi.spyOn(drafts, "select");
  testHistory().set({ value: "/sessions/beta", replace: false, scroll: false });
  await waitFor(() => expect(selections).toHaveBeenCalledWith("/sessions/beta"));
  testHistory().set({ value: "/sessions/alpha", replace: false, scroll: false });
  await waitFor(() => expect(selections).toHaveBeenLastCalledWith("/sessions/alpha"));
  publish({ ...snapshot });
  expect(prompt().value).toBe("");

  fireEvent.input(prompt(), { target: { value: "do not restore after sign-out" } });
  drafts.clear();
  expire();
  publish({ ...snapshot });
  expect(prompt().value).toBe("");
});

test("reports unconfirmed cancellation while preserving the draft", async () => {
  const { bridge } = setup();
  fireEvent.input(prompt(), { target: { value: "keep this" } });
  pick(file());
  await screen.findByRole("img", { name: "Attachment 1" });
  let finish!: (reason: Error) => void;
  bridge.send.mockImplementationOnce(() => new Promise((_resolve, reject) => { finish = reject; }));
  bridge.cancelSend.mockRejectedValueOnce(new Error("IPC rejected"));
  fireEvent.click(sendButton());
  await waitFor(() => expect(bridge.send).toHaveBeenCalledOnce());
  fireEvent.click(screen.getByRole("button", { name: "Cancel upload" }));
  await screen.findByText(/Upload cancellation could not be confirmed/);
  finish(new Error("send outcome unknown"));
  await screen.findByRole("button", { name: "Send" });
  expect(prompt().value).toBe("keep this");
  expect(screen.getByRole("img", { name: "Attachment 1" })).toBeTruthy();
});

test("ignores a late read after a route switch and a late upload cannot clear a newer draft", async () => {
  const { bridge, create } = setup([history()]);
  let read!: (bytes: Uint8Array) => void;
  bridge.read.mockImplementation(() => new Promise(resolve => { read = resolve; }));
  let upload!: () => void;
  bridge.send.mockImplementation(() => new Promise(resolve => { upload = resolve; }));
  pick(file());
  await screen.findByRole("img", { name: "Attachment 1" });
  fireEvent.click(sendButton());
  await waitFor(() => expect(bridge.send).toHaveBeenCalledOnce());
  await waitFor(() => expect(bridge.read).toHaveBeenCalled());
  testHistory().set({ value: "/sessions/beta", replace: false, scroll: false });
  await waitFor(() => expect(screen.queryByRole("img", { name: "Attachment 1" })).toBeNull());
  fireEvent.input(prompt(), { target: { value: "new draft" } });
  const before = create.mock.calls.length;
  read(png);
  upload();
  await Promise.resolve();
  expect(create).toHaveBeenCalledTimes(before);
  expect(screen.queryByRole("img", { name: "Message attachment" })).toBeNull();
  expect(prompt().value).toBe("new draft");
});
