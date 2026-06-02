import { cleanup, fireEvent, screen } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren } from "../support/routes";
import { claudeConfig, snapshotWithConfig } from "../support/snapshots";

describe("config tabs", () => {
  test("renders a tab strip with an active Providers tab and its section content", () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: snapshotWithConfig(claudeConfig()),
    });

    const tablist = screen.getByRole("tablist", { name: "Configuration sections" });
    const providersTab = screen.getByRole("tab", { name: "Providers" });
    expect(tablist.contains(providersTab)).toBe(true);
    expect(providersTab.getAttribute("aria-selected")).toBe("true");
    // Active tab shows the providers section content.
    expect(screen.getByText("claude")).toBeTruthy();
  });

  test("activating a tab sets it active and shows its section content", () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: snapshotWithConfig(claudeConfig()),
    });

    const providersTab = screen.getByRole("tab", { name: "Providers" });
    fireEvent.click(providersTab);

    expect(providersTab.getAttribute("aria-selected")).toBe("true");
    expect(screen.getByText("claude")).toBeTruthy();
  });
});

describe("config routing", () => {
  test("opens provider forms from config child routes", () => {
    const snapshot = snapshotWithConfig(claudeConfig());

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      routePath: "/config/provider/create",
      snapshot,
    });
    expect(screen.getByRole("dialog", { name: "Create provider dialog" })).toBeTruthy();
    cleanup();

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      routePath: "/config/provider/edit/claude",
      snapshot,
    });
    expect(screen.getByRole("dialog", { name: "Edit provider dialog" })).toBeTruthy();
    expect(screen.getByLabelText("Name")).toHaveProperty("value", "claude");
  });
});
