import { render, waitFor } from "@solidjs/testing-library";
import { expect, test, vi } from "vitest";

import { AppStateProvider } from "../../src/app/state";
import type { ClientAdapter } from "../../src/platform/client";
import { emptyConfig, snapshotWithConfig } from "../support/snapshots";

test("the neutral provider disposes on a non-persisted pagehide", async () => {
  const unsubscribe = vi.fn();
  const dispose = vi.fn();
  const snapshot = snapshotWithConfig(emptyConfig("lifecycle"));
  const adapter: ClientAdapter = {
    snapshot: () => snapshot,
    start: vi.fn().mockResolvedValue(undefined),
    subscribe: vi.fn(() => unsubscribe),
    dispatch: vi.fn().mockResolvedValue(snapshot),
    dispose,
  };
  const view = render(() => (
    <AppStateProvider loadClientAdapter={async () => adapter}>
      <div>content</div>
    </AppStateProvider>
  ));
  await waitFor(() => expect(adapter.start).toHaveBeenCalledOnce());

  window.dispatchEvent(pageTransition("pagehide", false));
  expect(dispose).toHaveBeenCalledOnce();
  view.unmount();
  expect(unsubscribe).toHaveBeenCalledOnce();
  expect(dispose).toHaveBeenCalledTimes(2);
});

test("a persisted pagehide keeps the adapter alive and pageshow restores its loop", async () => {
  const dispose = vi.fn();
  const start = vi.fn().mockResolvedValue(undefined);
  const snapshot = snapshotWithConfig(emptyConfig("bfcache"));
  const adapter: ClientAdapter = {
    snapshot: () => snapshot,
    start,
    subscribe: vi.fn(() => vi.fn()),
    dispatch: vi.fn().mockResolvedValue(snapshot),
    dispose,
  };
  const view = render(() => (
    <AppStateProvider loadClientAdapter={async () => adapter}>
      <div>content</div>
    </AppStateProvider>
  ));
  await waitFor(() => expect(start).toHaveBeenCalledOnce());

  window.dispatchEvent(pageTransition("pagehide", true));
  expect(dispose).not.toHaveBeenCalled();
  window.dispatchEvent(pageTransition("pageshow", true));
  await waitFor(() => expect(start).toHaveBeenCalledTimes(2));
  expect(dispose).not.toHaveBeenCalled();

  view.unmount();
  expect(dispose).toHaveBeenCalledOnce();
});

function pageTransition(type: "pagehide" | "pageshow", persisted: boolean) {
  const event = new Event(type);
  Object.defineProperty(event, "persisted", { value: persisted });
  return event;
}
