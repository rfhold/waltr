import { cleanup, fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren, ShellRouteWithChildren, testHistory } from "../support/routes";
import { claudeConfig, snapshotWithConfig, snapshotWithSessions, sessionView, readySessionTimeline, mcpServerView } from "../support/snapshots";

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
  test("navigates from a session authentication error into the matching MCP provider and back", async () => {
    const snapshot = snapshotWithSessions([sessionView({ status: {
      phase: "failed", suspensionKind: null, failedMessage: "Authentication required",
      authenticationFailure: { version: 1, domain: "outboundMcp", reason: "credentialRequired", target: { kind: "mcpServer", key: "protected" }, mcpContext: null },
    } })]);
    snapshot.sessions.timelines["session-a"] = readySessionTimeline();
    snapshot.config = snapshotWithConfig({ ...claudeConfig(), mcpServers: {
      protected: mcpServerView({ name: "protected", kind: "streamableHttp", url: "https://mcp.example" }),
    } }).config;
    renderWithAppState(() => <ShellRouteWithChildren />, { routePath: "/sessions/session-a", snapshot });
    fireEvent.click(screen.getByRole("link", { name: "Open MCP server Config" }));
    await waitFor(() => expect(screen.getByRole("dialog", { name: "Edit MCP server dialog" })).toBeTruthy());
    expect(screen.getByLabelText("Name")).toHaveProperty("value", "protected");
    expect(screen.getByRole("tab", { name: "MCP Servers" }).getAttribute("aria-selected")).toBe("true");
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(testHistory().get()).toBe("/config/mcp"));
    fireEvent.click(screen.getByLabelText("Edit protected"));
    await waitFor(() => expect(screen.getByLabelText("Name")).toHaveProperty("value", "protected"));
    testHistory().set({ value: "/sessions/session-a", replace: false, scroll: false });
    await waitFor(() => expect(screen.getByRole("link", { name: "Open MCP server Config" })).toBeTruthy());
    fireEvent.click(screen.getByRole("link", { name: "Open MCP server Config" }));
    await waitFor(() => expect(screen.getByRole("dialog", { name: "Edit MCP server dialog" })).toBeTruthy());
  });

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
