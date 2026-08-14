import { fireEvent, screen, waitFor, within } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import type { ClientCommand } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren } from "../support/routes";
import { claudeConfig, modelView, snapshotWithConfig } from "../support/snapshots";

describe("inference defaults", () => {
  test("saves inference-only defaults", async () => {
    const commands: ClientCommand[] = [];
    const config = claudeConfig();
    config.providers.claude!.models.sonnet = modelView({
      name: "Claude Sonnet",
      options: { ...modelView().options, reasoningEfforts: ["low", "high"] },
    });

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      routePath: "/config/defaults",
      snapshot: snapshotWithConfig(config),
    });

    expect(screen.getByRole("heading", { name: "New session inference" })).toBeTruthy();
    fireEvent.change(screen.getByLabelText("Provider"), { target: { value: "claude" } });
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "sonnet" } });
    fireEvent.change(screen.getByLabelText("Reasoning effort"), { target: { value: "high" } });
    fireEvent.click(screen.getByRole("button", { name: "Save defaults" }));

    await waitFor(() => expect(commands).toContainEqual({
      type: "setInferenceDefaults",
      defaults: { providerKey: "claude", modelId: "sonnet", reasoningEffort: "high" },
    }));
    expect(within(screen.getByRole("heading", { name: "New session inference" }).closest("section")!).queryByText(/MCP/i)).toBeNull();
  });

  test("clears defaults with a null payload", async () => {
    const commands: ClientCommand[] = [];
    const config = claudeConfig();
    config.inferenceDefaults = { providerKey: "claude", modelId: "sonnet", reasoningEffort: null };

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      routePath: "/config/defaults",
      snapshot: snapshotWithConfig(config),
    });

    fireEvent.click(screen.getByRole("button", { name: "Clear defaults" }));

    await waitFor(() => expect(commands).toContainEqual({ type: "setInferenceDefaults", defaults: null }));
  });
});
