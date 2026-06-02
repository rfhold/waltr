import { screen } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { renderWithAppState } from "../support/render";
import { ShellRouteWithChildren } from "../support/routes";
import { snapshotWithConfig } from "../support/snapshots";

describe("shell routing", () => {
  test("marks nested config routes active in the shell", () => {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/config/provider/edit/claude",
      snapshot: snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {} }),
    });

    expect(screen.getByText("Config").classList.contains("active")).toBe(true);
  });
});
