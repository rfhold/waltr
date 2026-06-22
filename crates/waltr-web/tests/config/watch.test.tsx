import { describe, expect, test } from "vitest";
import { waitFor } from "@solidjs/testing-library";

import type { ClientCommand } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren } from "../support/routes";
import { snapshotWithConfig } from "../support/snapshots";

describe("config watch", () => {
  test("registers config watch interest while the config route is mounted", async () => {
    const commands: ClientCommand[] = [];
    const result = renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      snapshot: snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, runtimeDefaults: null }),
    });

    await waitFor(() =>
      expect(commands.map((command) => command.type)).toContain("registerConfigWatchInterest"),
    );

    result.unmount();

    await waitFor(() =>
      expect(commands.map((command) => command.type)).toEqual([
        "registerConfigWatchInterest",
        "releaseConfigWatchInterest",
      ]),
    );
  });

  test("waits for system readiness before registering config watch interest", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      snapshot: snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, runtimeDefaults: null }, { ready: false }),
    });

    await Promise.resolve();

    expect(commands).toHaveLength(0);
  });
});
