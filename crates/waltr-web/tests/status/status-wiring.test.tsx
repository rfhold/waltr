import { screen } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren } from "../support/routes";
import { operationSnapshot, snapshotWithConfig } from "../support/snapshots";

describe("status wiring", () => {
  test("loaded config surface shows no persistent watch footer", () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: {
        ...snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {} }),
        config: {
          current: { etag: "v1", providers: {}, mcpServers: {} },
          load: operationSnapshot("idle"),
          patch: operationSnapshot("idle"),
          watch: operationSnapshot("received"),
        },
        status: {
          ready: true,
          watch: operationSnapshot("opened"),
        },
      },
    });

    expect(screen.queryByLabelText("Configuration watch status")).toBeNull();
    expect(screen.queryByText(/Loading configuration\.\.\./)).toBeNull();
  });

  test("pending config surface shows the watch status in the loading panel", () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: {
        ...snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {} }),
        config: {
          current: null,
          load: operationSnapshot("idle"),
          patch: operationSnapshot("idle"),
          watch: operationSnapshot("opened"),
        },
        status: {
          ready: true,
          watch: operationSnapshot("opened"),
        },
      },
    });

    expect(screen.getByText(/Loading configuration\.\.\. \(opened\)/)).toBeTruthy();
  });
});
