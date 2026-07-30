import { screen } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren } from "../support/routes";
import { claudeConfig, operationSnapshot, snapshotWithConfig } from "../support/snapshots";

describe("config read", () => {
  test("loads configuration from the config route state", () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: snapshotWithConfig(claudeConfig()),
    });

    expect(screen.queryByText(/Loading configuration\.\.\./)).toBeNull();
    expect(screen.getByText("claude")).toBeTruthy();
    expect(screen.getByText("https://api.anthropic.com/v1/messages")).toBeTruthy();
  });

  test("renders loading with the watch status until a snapshot is received", () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: {
        ...snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, runtimeDefaults: null }),
        config: {
          current: null,
          load: operationSnapshot("idle"),
          patch: operationSnapshot("idle"),
          reset: operationSnapshot("idle"),
          watch: operationSnapshot("opened"),
        },
      },
    });

    expect(screen.getByText("Loading configuration... (opened)")).toBeTruthy();
    // Section content must not render while pending.
    expect(screen.queryByRole("button", { name: "Create" })).toBeNull();
    expect(screen.queryByText("No providers configured yet.")).toBeNull();
  });

  test("renders the section and hides the loading panel once config is available", () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: snapshotWithConfig(claudeConfig()),
    });

    expect(screen.queryByText(/Loading configuration\.\.\./)).toBeNull();
    expect(screen.getByRole("button", { name: "Create" })).toBeTruthy();
    expect(screen.getByText("claude")).toBeTruthy();
  });

  test("renders the error panel instead of the loading panel on a watch error", () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: {
        ...snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, runtimeDefaults: null }),
        config: {
          current: null,
          load: operationSnapshot("idle"),
          patch: operationSnapshot("idle"),
          reset: operationSnapshot("idle"),
          watch: { status: "waiting", error: "watch failed", key: null },
        },
      },
    });

    expect(screen.getByText("Unable to load configuration.")).toBeTruthy();
    expect(screen.queryByText(/Loading configuration\.\.\./)).toBeNull();
  });
});
