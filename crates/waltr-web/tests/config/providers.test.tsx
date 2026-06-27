import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import type { ClientCommand } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren, testHistory } from "../support/routes";
import { claudeConfig, snapshotWithConfig } from "../support/snapshots";

describe("provider config", () => {
  test("creates edits and deletes providers from config route state", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      snapshot: snapshotWithConfig(claudeConfig()),
    });

    fireEvent.click(screen.getByText("Create"));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/create"));
    expect(screen.getByRole("dialog", { name: "Create provider dialog" })).toBeTruthy();
    fireEvent.input(screen.getByLabelText("Name"), { target: { value: "openai" } });
    fireEvent.change(screen.getByLabelText("Type"), { target: { value: "openAiChatCompletions" } });
    fireEvent.input(screen.getByLabelText("URL"), { target: { value: "https://api.openai.com/v1/chat/completions" } });
    fireEvent.submit(screen.getByLabelText("Create provider"));

    await waitFor(() => expect(screen.getByText("openai")).toBeTruthy());

    fireEvent.click(screen.getByText("openai"));
    fireEvent.click(screen.getByLabelText("Edit openai"));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/edit/openai"));
    expect(screen.getByRole("dialog", { name: "Edit provider dialog" })).toBeTruthy();
    fireEvent.input(screen.getByLabelText("URL"), { target: { value: "https://example.test" } });
    fireEvent.submit(screen.getByLabelText("Edit provider"));

    await waitFor(() => expect(screen.getByText("https://example.test")).toBeTruthy());

    fireEvent.click(screen.getByLabelText("Delete openai"));
    expect(screen.getByLabelText("Delete openai confirmation")).toBeTruthy();
    fireEvent.click(screen.getByLabelText("Confirm delete openai"));

    await waitFor(() => expect(screen.queryByText("openai")).toBeNull());
    expect(commands.map((command) => command.type)).toContain("addProvider");
    expect(commands.map((command) => command.type)).toContain("updateProvider");
    expect(commands.map((command) => command.type)).toContain("deleteProvider");
  });

  test("keeps provider form open until keyed patch failure is surfaced", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      configPatchError: "provider write failed",
      snapshot: snapshotWithConfig(claudeConfig()),
    });

    fireEvent.click(screen.getByText("Create"));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/create"));
    fireEvent.input(screen.getByLabelText("Name"), { target: { value: "openai" } });
    fireEvent.change(screen.getByLabelText("Type"), { target: { value: "openAiChatCompletions" } });
    fireEvent.input(screen.getByLabelText("URL"), { target: { value: "https://api.openai.com/v1/chat/completions" } });
    fireEvent.submit(screen.getByLabelText("Create provider"));

    expect(screen.getByRole("button", { name: "Save" }).hasAttribute("disabled")).toBe(true);
    await waitFor(() => expect(screen.getByText("provider write failed")).toBeTruthy());

    expect(screen.getByRole("dialog", { name: "Create provider dialog" })).toBeTruthy();
    expect(testHistory().get()).toBe("/config/provider/create");
    expect(commands.map((command) => command.type)).toContain("addProvider");
  });
});
