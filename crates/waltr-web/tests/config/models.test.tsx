import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import type { ClientCommand } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren, testHistory } from "../support/routes";
import { claudeConfig, modelView, snapshotWithConfig } from "../support/snapshots";

function configWithModel() {
  const config = claudeConfig();
  const claude = config.providers.claude!;
  return {
    ...config,
    providers: {
      claude: {
        ...claude,
        models: {
          sonnet: modelView({ name: "Claude Sonnet", kind: "llm" }),
        },
      },
    },
  };
}

describe("provider models sub-editor", () => {
  test("lists a provider's models by display name and type", () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      routePath: "/config/provider/edit/claude",
      snapshot: snapshotWithConfig(configWithModel()),
    });

    expect(screen.getByRole("heading", { name: "Models" })).toBeTruthy();
    expect(screen.getByText("Claude Sonnet")).toBeTruthy();
    expect(screen.getByLabelText("Edit model sonnet")).toBeTruthy();
  });

  test("creates a model in the provider draft and dispatches only when the provider saves", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      routePath: "/config/provider/edit/claude",
      snapshot: snapshotWithConfig(configWithModel()),
    });

    fireEvent.click(screen.getByRole("button", { name: "Add model" }));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/edit/claude/models/create"));
    const createDialog = screen.getByRole("dialog", { name: "Create model dialog" });
    expect(createDialog).toBeTruthy();
    expect(screen.getByLabelText("Models for claude").contains(createDialog)).toBe(false);
    fireEvent.input(screen.getByLabelText("Key"), { target: { value: "haiku" } });
    fireEvent.input(screen.getByLabelText("Display name"), { target: { value: "Claude Haiku" } });
    fireEvent.change(screen.getAllByLabelText("Type")[1]!, { target: { value: "llm" } });
    fireEvent.submit(screen.getByLabelText("Create model"));

    await waitFor(() => expect(screen.getByText("Claude Haiku")).toBeTruthy());
    expect(commands.some((command) => command.type === "updateProvider")).toBe(false);

    fireEvent.submit(screen.getByLabelText("Edit provider"));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider"));

    const types = commands.map((command) => command.type);
    expect(types).toContain("updateProvider");
    expect(types).not.toContain("addProvider");

    const update = commands.find((command) => command.type === "updateProvider");
    expect(update).toMatchObject({ type: "updateProvider", originalName: "claude" });
    if (update && update.type === "updateProvider") {
      // Whole provider carried, including both the existing and the new model.
      expect(Object.keys(update.provider.models)).toEqual(
        expect.arrayContaining(["sonnet", "haiku"]),
      );
      expect(update.provider.models.haiku).toMatchObject({ name: "Claude Haiku", kind: "llm" });
    }
  });

  test("requires a model type before saving and indicates when it is missing", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      routePath: "/config/provider/edit/claude",
      snapshot: snapshotWithConfig(configWithModel()),
    });

    fireEvent.click(screen.getByRole("button", { name: "Add model" }));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/edit/claude/models/create"));
    fireEvent.input(screen.getByLabelText("Key"), { target: { value: "mystery" } });
    // Type left at the empty default.
    fireEvent.submit(screen.getByLabelText("Create model"));

    await Promise.resolve();
    expect(commands.some((command) => command.type === "updateProvider")).toBe(false);
    expect(screen.getByText("A model type is required.")).toBeTruthy();
  });

  test("edits a model in the provider draft and dispatches only when the provider saves", async () => {
    const commands: ClientCommand[] = [];
    const config = configWithModel();
    const catalogMatch = {
      providerId: "anthropic",
      modelId: "claude-sonnet-4-5",
      upstreamProviderId: null,
      upstreamModelId: null,
      source: "manual" as const,
    };
    config.providers.claude!.models.sonnet = modelView({
      name: "Claude Sonnet",
      kind: "llm",
      catalogMatch,
    });

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      routePath: "/config/provider/edit/claude",
      snapshot: snapshotWithConfig(config),
    });

    fireEvent.click(screen.getByLabelText("Edit model sonnet"));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/edit/claude/models/edit/sonnet"));
    const editDialog = screen.getByRole("dialog", { name: "Edit model dialog" });
    expect(editDialog).toBeTruthy();
    expect(screen.getByLabelText("Models for claude").contains(editDialog)).toBe(false);
    fireEvent.input(screen.getByLabelText("Key"), { target: { value: "opus" } });
    fireEvent.input(screen.getByLabelText("Display name"), { target: { value: "Claude Sonnet 4" } });
    fireEvent.change(screen.getAllByLabelText("Type")[1]!, { target: { value: "embedding" } });
    fireEvent.submit(screen.getByLabelText("Edit model"));

    await waitFor(() => expect(screen.getByText("Claude Sonnet 4")).toBeTruthy());
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/edit/claude"));
    expect(screen.getByText("embedding")).toBeTruthy();
    expect(screen.queryByLabelText("Edit model sonnet")).toBeNull();
    expect(screen.getByLabelText("Edit model opus")).toBeTruthy();
    expect(commands.some((command) => command.type === "updateProvider")).toBe(false);

    fireEvent.submit(screen.getByLabelText("Edit provider"));

    const update = commands.find((command) => command.type === "updateProvider");
    expect(update).toMatchObject({ type: "updateProvider", originalName: "claude" });
    if (update && update.type === "updateProvider") {
      expect(Object.keys(update.provider.models)).not.toContain("sonnet");
      expect(update.provider.models.opus).toMatchObject({ name: "Claude Sonnet 4", kind: "embedding" });
      expect(update.provider.models.opus?.catalogMatch).toEqual(catalogMatch);
    }
  });

  test("selects a suggested catalog match for a model draft", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      routePath: "/config/provider/edit/claude/models/edit/sonnet",
      snapshot: snapshotWithConfig(configWithModel()),
      suggestCatalogMatches: async (request) => {
        expect(request).toMatchObject({ providerKey: "claude", modelKey: "sonnet" });
        return {
          stale: false,
          matches: [{
            model: { providerId: "gateway", modelId: "anthropic/claude-sonnet-4-5" },
            providerName: "Gateway",
            modelName: "Claude Sonnet 4.5",
            confidence: "high",
            reasons: ["upstream model id"],
            upstreamModel: { providerId: "anthropic", modelId: "claude-sonnet-4-5" },
          }],
        };
      },
    });

    fireEvent.click(screen.getByRole("button", { name: "Suggest" }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Claude Sonnet 4\.5/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /Claude Sonnet 4\.5/ }));

    expect(screen.getAllByText("gateway/anthropic/claude-sonnet-4-5").length).toBeGreaterThan(0);
    expect(screen.getByText("anthropic/claude-sonnet-4-5")).toBeTruthy();

    fireEvent.submit(screen.getByLabelText("Edit model"));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/edit/claude"));
    fireEvent.submit(screen.getByLabelText("Edit provider"));

    const update = commands.find((command) => command.type === "updateProvider");
    expect(update).toMatchObject({ type: "updateProvider", originalName: "claude" });
    if (update && update.type === "updateProvider") {
      expect(update.provider.models.sonnet?.catalogMatch).toEqual({
        providerId: "gateway",
        modelId: "anthropic/claude-sonnet-4-5",
        upstreamProviderId: "anthropic",
        upstreamModelId: "claude-sonnet-4-5",
        source: "manual",
      });
    }
  });

  test("applies catalog search facts to blank model fields", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      routePath: "/config/provider/edit/claude/models/create",
      snapshot: snapshotWithConfig(configWithModel()),
      searchCatalogModels: async (request) => {
        expect(request.query).toBe("haiku");
        return {
          stale: true,
          models: [{
            model: { providerId: "anthropic", modelId: "claude-haiku-4-5" },
            providerName: "Anthropic",
            modelName: "Claude Haiku 4.5",
            family: "claude",
            status: null,
            limits: { context: 200000, output: 8192 },
            cost: { inputPerMillionTokens: 1, outputPerMillionTokens: 5, currency: "USD" },
            capabilities: { tools: true, image: true, audio: false },
            reasoning: { supported: true, effort: "effort", efforts: ["low", "medium", "high"] },
          }],
        };
      },
    });

    fireEvent.input(screen.getByLabelText("Key"), { target: { value: "haiku" } });
    fireEvent.input(screen.getByLabelText("Search catalog"), { target: { value: "haiku" } });
    fireEvent.click(screen.getByRole("button", { name: "Search" }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Claude Haiku 4\.5/ })).toBeTruthy());
    expect(screen.getByText("Catalog data is from a stale cache.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Claude Haiku 4\.5/ }));

    expect(screen.getByLabelText("Display name")).toHaveProperty("value", "Claude Haiku 4.5");
    expect(screen.getByLabelText("Context limit")).toHaveProperty("value", "200000");
    expect(screen.getByLabelText("Output limit")).toHaveProperty("value", "8192");
    expect(screen.getByLabelText("Reasoning efforts")).toHaveProperty("value", "low, medium, high");
    expect(screen.getByLabelText("Supports tools")).toHaveProperty("checked", true);
    expect(screen.getByLabelText("Supports image input")).toHaveProperty("checked", true);

    fireEvent.submit(screen.getByLabelText("Create model"));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/edit/claude"));
    fireEvent.submit(screen.getByLabelText("Edit provider"));

    const update = commands.find((command) => command.type === "updateProvider");
    expect(update).toMatchObject({ type: "updateProvider", originalName: "claude" });
    if (update && update.type === "updateProvider") {
      expect(update.provider.models.haiku).toMatchObject({
        name: "Claude Haiku 4.5",
        kind: "llm",
        limits: { context: 200000, output: 8192 },
        capabilities: { tools: true, image: true },
        catalogMatch: {
          providerId: "anthropic",
          modelId: "claude-haiku-4-5",
          upstreamProviderId: null,
          upstreamModelId: null,
          source: "applied",
        },
      });
    }
  });

  test("does not overwrite explicit disabled capabilities when applying catalog facts", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      routePath: "/config/provider/edit/claude/models/edit/sonnet",
      snapshot: snapshotWithConfig(configWithModel()),
      searchCatalogModels: async () => ({
        stale: false,
        models: [{
          model: { providerId: "gateway", modelId: "anthropic/claude-sonnet-4-5" },
          providerName: "Gateway",
          modelName: "Claude Sonnet 4.5",
          family: "claude",
          status: null,
          limits: { context: 200000, output: 8192 },
          cost: null,
          capabilities: { tools: true, image: true, audio: false },
          reasoning: null,
        }],
      }),
    });

    expect(screen.getByLabelText("Supports tools")).toHaveProperty("checked", false);
    expect(screen.getByLabelText("Supports image input")).toHaveProperty("checked", false);
    fireEvent.input(screen.getByLabelText("Search catalog"), { target: { value: "sonnet" } });
    fireEvent.click(screen.getByRole("button", { name: "Search" }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Claude Sonnet 4\.5/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /Claude Sonnet 4\.5/ }));

    expect(screen.getByLabelText("Supports tools")).toHaveProperty("checked", false);
    expect(screen.getByLabelText("Supports image input")).toHaveProperty("checked", false);

    fireEvent.submit(screen.getByLabelText("Edit model"));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/edit/claude"));
    fireEvent.submit(screen.getByLabelText("Edit provider"));

    const update = commands.find((command) => command.type === "updateProvider");
    expect(update).toMatchObject({ type: "updateProvider", originalName: "claude" });
    if (update && update.type === "updateProvider") {
      expect(update.provider.models.sonnet?.capabilities).toEqual({ tools: false, image: false });
      expect(update.provider.models.sonnet?.catalogMatch).toEqual({
        providerId: "gateway",
        modelId: "anthropic/claude-sonnet-4-5",
        upstreamProviderId: "anthropic",
        upstreamModelId: "claude-sonnet-4-5",
        source: "applied",
      });
    }
  });

  test("deletes a model in the provider draft and dispatches only when the provider saves", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      routePath: "/config/provider/edit/claude",
      snapshot: snapshotWithConfig(configWithModel()),
    });

    fireEvent.click(screen.getByLabelText("Delete model sonnet"));
    expect(screen.getByLabelText("Delete model sonnet confirmation")).toBeTruthy();
    fireEvent.click(screen.getByLabelText("Cancel delete model sonnet"));
    expect(screen.getByText("Claude Sonnet")).toBeTruthy();

    fireEvent.click(screen.getByLabelText("Delete model sonnet"));
    expect(screen.getByLabelText("Delete model sonnet confirmation")).toBeTruthy();
    fireEvent.click(screen.getByLabelText("Confirm delete model sonnet"));

    await waitFor(() => expect(screen.queryByText("Claude Sonnet")).toBeNull());
    expect(screen.queryByLabelText("Edit model sonnet")).toBeNull();
    expect(commands.some((command) => command.type === "updateProvider")).toBe(false);

    fireEvent.submit(screen.getByLabelText("Edit provider"));

    const update = commands.find((command) => command.type === "updateProvider");
    expect(update).toMatchObject({ type: "updateProvider", originalName: "claude" });
    if (update && update.type === "updateProvider") {
      expect(Object.keys(update.provider.models)).not.toContain("sonnet");
    }
  });

  test("stages models for a new provider before the provider exists", async () => {
    const commands: ClientCommand[] = [];

    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      routePath: "/config/provider/create",
      snapshot: snapshotWithConfig(claudeConfig()),
    });

    fireEvent.input(screen.getByLabelText("Name"), { target: { value: "openai" } });
    fireEvent.change(screen.getByLabelText("Type"), { target: { value: "openAi" } });
    fireEvent.input(screen.getByLabelText("URL"), { target: { value: "https://api.openai.com" } });
    fireEvent.click(screen.getByRole("button", { name: "Add model" }));
    await waitFor(() => expect(testHistory().get()).toBe("/config/provider/create/models/create"));
    fireEvent.input(screen.getByLabelText("Key"), { target: { value: "gpt4o" } });
    fireEvent.input(screen.getByLabelText("Display name"), { target: { value: "GPT-4o" } });
    fireEvent.change(screen.getAllByLabelText("Type")[1]!, { target: { value: "llm" } });
    fireEvent.submit(screen.getByLabelText("Create model"));

    await waitFor(() => expect(screen.getByText("GPT-4o")).toBeTruthy());
    expect(commands.some((command) => command.type === "addProvider")).toBe(false);

    fireEvent.submit(screen.getByLabelText("Create provider"));
    const add = commands.find((command) => command.type === "addProvider");
    expect(add).toMatchObject({ type: "addProvider" });
    if (add && add.type === "addProvider") {
      expect(add.provider.models.gpt4o).toMatchObject({ name: "GPT-4o", kind: "llm" });
    }
  });
});
