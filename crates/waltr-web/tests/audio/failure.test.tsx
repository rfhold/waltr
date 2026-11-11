import { render, fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { afterEach, expect, test } from "vitest";
import { cleanup } from "@solidjs/testing-library";
import { AppStateProvider } from "../../src/app/state";
import { AppLayout } from "../../src/routes/layouts/AppLayout";
import type { AudioFailure, ClientAdapter } from "../../src/platform/client";
import { snapshotWithConfig, emptyConfig } from "../support/snapshots";

afterEach(cleanup);

test("audio failure stays accessible until dismissed or successful completion clears it", async () => {
  let publish!: (failure: AudioFailure | undefined) => void;
  const snapshot = snapshotWithConfig(emptyConfig());
  const adapter: ClientAdapter = {
    snapshot: () => snapshot,
    start: async () => {},
    subscribe: () => () => {},
    dispatch: async () => snapshot,
    dispose: () => {},
    audioFailure: {
      subscribe: listener => { publish = listener; listener({ diagnosticId: 42, category: "remote_failed" }); return () => {}; },
      dismiss: async id => { expect(id).toBe(42); publish(undefined); },
    },
  };
  render(() => <AppStateProvider loadClientAdapter={async () => adapter}><AppLayout><div>Shell</div></AppLayout></AppStateProvider>);
  expect((await screen.findByRole("alert")).textContent).toContain("Diagnostic ID: 42");
  expect(screen.getByRole("alert").textContent).toContain("failed on the server");
  await Promise.resolve();
  expect(screen.getByRole("alert")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Dismiss audio error" }));
  await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
  publish({ diagnosticId: 43, category: "permission" });
  expect(screen.getByRole("alert").textContent).toContain("Diagnostic ID: 43");
  publish(undefined);
  expect(screen.queryByRole("alert")).toBeNull();
});
