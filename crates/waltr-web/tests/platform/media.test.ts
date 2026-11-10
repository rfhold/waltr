import { afterEach, expect, test, vi } from "vitest";
import { cancelOwnedMedia, waitAccepted } from "../../src/platform/media";
import { mediaBridge } from "../support/routines";

afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); });

test("acceptance consumes the original monotonic open deadline instead of starting a new budget", async () => {
  vi.useFakeTimers();
  const bridge = mediaBridge();
  bridge.mediaInteractionStatusJson.mockReturnValue(JSON.stringify({ phase: "opening" }));
  const deadline = performance.now() + 20000;
  await vi.advanceTimersByTimeAsync(19000);
  const accepted = waitAccepted(bridge, 7, () => false, deadline);
  const rejected = expect(accepted).rejects.toThrow("acceptance timed out");
  vi.setSystemTime(Date.now() - 60000);
  await vi.advanceTimersByTimeAsync(1000);
  await rejected;
  expect(bridge.mediaInteractionStatusJson).toHaveBeenCalledTimes(100);
});

test("cancelled opening does not permit input or retry admission", async () => {
  vi.useFakeTimers();
  const bridge = mediaBridge();
  bridge.mediaInteractionStatusJson.mockReturnValue(JSON.stringify({ phase: "opening" }));
  let cancelled = false;
  const accepted = waitAccepted(bridge, 7, () => cancelled);
  const rejected = expect(accepted).rejects.toThrow("cancelled");
  cancelled = true;
  await vi.advanceTimersByTimeAsync(10);
  await rejected;
  expect(bridge.openMediaInteraction).not.toHaveBeenCalled();
  expect(bridge.pushMediaPcm).not.toHaveBeenCalled();
});

test("already-joined WASM admission failure preserves remote uncertainty without inventing cleanup", async () => {
  const bridge = mediaBridge();
  bridge.cancelMediaInteraction.mockRejectedValue(new Error("stale owner"));
  bridge.mediaInteractionStatusJson.mockReturnValue(JSON.stringify({ phase: "error", cleanupVerified: false }));
  expect(await cancelOwnedMedia(bridge, 7)).toBe(false);
  bridge.mediaInteractionStatusJson.mockReturnValue(JSON.stringify({ phase: "opening", cleanupVerified: false }));
  await expect(cancelOwnedMedia(bridge, 7)).rejects.toThrow("stale owner");
  bridge.nativePlayback = { drop: async () => {} };
  bridge.mediaInteractionStatusJson.mockReturnValue(JSON.stringify({ phase: "error", cleanupVerified: false }));
  await expect(cancelOwnedMedia(bridge, 7)).rejects.toThrow("stale owner");
});
