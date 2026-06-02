import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import type { ClientCommand } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren, testHistory } from "../support/routes";
import { claudeConfig, mcpServerView, snapshotWithConfig } from "../support/snapshots";

function configWithMcp() {
  const config = claudeConfig();
  return {
    ...config,
    mcpServers: {
      files: mcpServerView({ name: "files", kind: "stdio", command: "mcp-files" }),
    },
  };
}

describe("mcp servers tab", () => {
  test("renders an MCP Servers tab alongside Providers and switches to it", () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: snapshotWithConfig(configWithMcp()),
    });

    const tablist = screen.getByRole("tablist", { name: "Configuration sections" });
    const providersTab = screen.getByRole("tab", { name: "Providers" });
    const mcpTab = screen.getByRole("tab", { name: "MCP Servers" });
    expect(tablist.contains(mcpTab)).toBe(true);
    expect(providersTab.getAttribute("aria-selected")).toBe("true");

    fireEvent.click(mcpTab);

    expect(mcpTab.getAttribute("aria-selected")).toBe("true");
    // Activating the MCP tab shows the MCP section content.
    expect(screen.getByText("files")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Create" })).toBeTruthy();
  });
});

describe("mcp servers section", () => {
  test("creates edits and deletes MCP servers", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      snapshot: snapshotWithConfig(configWithMcp()),
    });

    fireEvent.click(screen.getByRole("tab", { name: "MCP Servers" }));

    // Create a new stdio server.
    fireEvent.click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(testHistory().get()).toBe("/config/mcp/create"));
    expect(screen.getByRole("dialog", { name: "Create MCP server dialog" })).toBeTruthy();
    fireEvent.input(screen.getByLabelText("Name"), { target: { value: "search" } });
    fireEvent.input(screen.getByLabelText("Command"), { target: { value: "mcp-search" } });
    fireEvent.submit(screen.getByLabelText("Create MCP server"));

    await waitFor(() => expect(screen.getByText("search")).toBeTruthy());

    // Edit the new server.
    fireEvent.click(screen.getByLabelText("Edit search"));
    await waitFor(() => expect(testHistory().get()).toBe("/config/mcp/edit/search"));
    expect(screen.getByRole("dialog", { name: "Edit MCP server dialog" })).toBeTruthy();
    fireEvent.input(screen.getByLabelText("Command"), { target: { value: "mcp-search-v2" } });
    fireEvent.submit(screen.getByLabelText("Edit MCP server"));

    await waitFor(() => expect(screen.getByText("mcp-search-v2")).toBeTruthy());

    // Delete it.
    fireEvent.click(screen.getByLabelText("Delete search"));
    expect(screen.getByLabelText("Delete search confirmation")).toBeTruthy();
    fireEvent.click(screen.getByLabelText("Confirm delete search"));

    await waitFor(() => expect(screen.queryByText("search")).toBeNull());

    const types = commands.map((command) => command.type);
    expect(types).toContain("addMcpServer");
    expect(types).toContain("updateMcpServer");
    expect(types).toContain("deleteMcpServer");

    const update = commands.find((command) => command.type === "updateMcpServer");
    expect(update).toMatchObject({ type: "updateMcpServer", originalName: "search" });
  });

  test("shows a URL field for sse servers and a command field for stdio servers", async () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: snapshotWithConfig(configWithMcp()),
    });

    fireEvent.click(screen.getByRole("tab", { name: "MCP Servers" }));
    fireEvent.click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(testHistory().get()).toBe("/config/mcp/create"));

    // stdio default: command present, url absent.
    expect(screen.getByLabelText("Command")).toBeTruthy();
    expect(screen.queryByLabelText("URL")).toBeNull();

    fireEvent.change(screen.getByLabelText("Type"), { target: { value: "sse" } });

    // sse: url present, command absent.
    expect(screen.getByLabelText("URL")).toBeTruthy();
    expect(screen.queryByLabelText("Command")).toBeNull();
  });

  test("prevents saving when the type-required field is empty", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      snapshot: snapshotWithConfig(configWithMcp()),
    });

    fireEvent.click(screen.getByRole("tab", { name: "MCP Servers" }));
    fireEvent.click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(testHistory().get()).toBe("/config/mcp/create"));

    fireEvent.input(screen.getByLabelText("Name"), { target: { value: "empty" } });
    // command left empty
    fireEvent.submit(screen.getByLabelText("Create MCP server"));

    await Promise.resolve();
    expect(commands.some((command) => command.type === "addMcpServer")).toBe(false);
    expect(screen.getByRole("alert")).toBeTruthy();
  });

  test("dispatches AddMcpServer with conditional url for a new sse server", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      snapshot: snapshotWithConfig(configWithMcp()),
    });

    fireEvent.click(screen.getByRole("tab", { name: "MCP Servers" }));
    fireEvent.click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(testHistory().get()).toBe("/config/mcp/create"));
    fireEvent.input(screen.getByLabelText("Name"), { target: { value: "remote" } });
    fireEvent.change(screen.getByLabelText("Type"), { target: { value: "sse" } });
    fireEvent.input(screen.getByLabelText("URL"), { target: { value: "https://mcp.example.test" } });
    fireEvent.submit(screen.getByLabelText("Create MCP server"));

    await waitFor(() => expect(screen.getByText("remote")).toBeTruthy());

    const add = commands.find((command) => command.type === "addMcpServer");
    expect(add).toMatchObject({
      type: "addMcpServer",
      server: { name: "remote", kind: "sse", url: "https://mcp.example.test" },
    });
  });
});
