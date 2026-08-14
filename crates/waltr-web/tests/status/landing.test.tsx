import { screen } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { renderWithAppState } from "../support/render";
import { ShellRouteWithChildren } from "../support/routes";
import { snapshotWithConfig } from "../support/snapshots";

describe("landing status", () => {
  test("renders the ready home route", () => {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/",
      snapshot: snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, inferenceDefaults: null }),
    });

    expect(screen.getByText("Waltr is ready.")).toBeTruthy();
  });
});
