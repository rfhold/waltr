import { waitFor } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { renderWithAppState } from "../support/render";
import { ShellRouteWithChildren, testHistory } from "../support/routes";
import { snapshotWithConfig } from "../support/snapshots";

describe("readiness routing", () => {
  test("keeps shell routes in place while the splash is visible", async () => {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/config/provider",
      snapshot: snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, runtimeDefaults: null }, { ready: false }),
    });

    await waitFor(() => expect(testHistory().get()).toBe("/config/provider"));
  });
});
