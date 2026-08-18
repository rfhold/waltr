import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import type { ClientCommand, ConfigView } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren, testHistory } from "../support/routes";
import { claudeConfig, snapshotWithConfig } from "../support/snapshots";

describe("provider config", () => {
  test("round-trips every public-client OAuth setting and preserves settings while disabled", async () => {
    const commands: ClientCommand[] = [];
    const config: ConfigView = claudeConfig();
    config.providers.claude = {
      ...config.providers.claude!,
      oauthCompatible: true,
      oauthSettings: {
        name: "Claude OAuth",
        authorizeUrl: "https://id.example/authorize",
        tokenUrl: "https://id.example/token",
        clientId: "public-client",
        redirectUri: "http://127.0.0.1/callback",
        scopes: ["openid", "profile"],
        tokenRequestStyle: "json",
        sendStateInTokenRequest: true,
        authUrlParams: [{ key: "audience", value: "waltr" }],
        jwtMetadata: [{ token: "id", metadataKey: "subject", claimNames: ["sub", "uid"], required: true }],
      },
    };

    renderWithAppState(() => <ConfigRouteWithChildren />, { commands, snapshot: snapshotWithConfig(config) });
    fireEvent.click(screen.getByLabelText("Edit claude"));

    await waitFor(() => expect(screen.getByLabelText("OAuth name")).toBeTruthy());
    expect((screen.getByLabelText("Authorization URL") as HTMLInputElement).value).toBe("https://id.example/authorize");
    expect((screen.getByLabelText("Token URL") as HTMLInputElement).value).toBe("https://id.example/token");
    expect((screen.getByLabelText("Client ID") as HTMLInputElement).value).toBe("public-client");
    expect((screen.getByLabelText("Redirect URI") as HTMLInputElement).value).toBe("http://127.0.0.1/callback");
    expect((screen.getByLabelText("Scopes") as HTMLInputElement).value).toBe("openid profile");
    expect((screen.getByLabelText("Token request style") as HTMLSelectElement).value).toBe("json");
    expect((screen.getByLabelText("Send state during code exchange") as HTMLInputElement).checked).toBe(true);
    expect((screen.getByLabelText("Parameter 1 key") as HTMLInputElement).value).toBe("audience");
    expect((screen.getByLabelText("Mapping 1 metadata key") as HTMLInputElement).value).toBe("subject");

    fireEvent.click(screen.getByLabelText("OAuth compatible"));
    expect(screen.getByLabelText("OAuth name")).toBeTruthy();
    fireEvent.input(screen.getByLabelText("Scopes"), { target: { value: "openid,email" } });
    fireEvent.submit(screen.getByLabelText("Edit provider"));

    await waitFor(() => expect(commands.some((command) => command.type === "updateProvider")).toBe(true));
    const update = commands.find((command): command is Extract<ClientCommand, { type: "updateProvider" }> => command.type === "updateProvider")!;
    expect(update.provider.oauthCompatible).toBe(false);
    expect(update.provider.oauthSettings).toMatchObject({
      name: "Claude OAuth",
      scopes: ["openid", "email"],
      tokenRequestStyle: "json",
      sendStateInTokenRequest: true,
      authUrlParams: [{ key: "audience", value: "waltr" }],
      jwtMetadata: [{ token: "id", metadataKey: "subject", claimNames: ["sub", "uid"], required: true }],
    });
    expect(document.body.textContent?.toLowerCase()).not.toContain("client secret");
  });

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

  test("keeps the form-open resource baseline after a watched update", async () => {
    const commands: ClientCommand[] = [];
    const initial = snapshotWithConfig(claudeConfig());
    let publishSnapshot: ((snapshot: typeof initial) => void) | undefined;
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      snapshot: initial,
      onSnapshotSetter: (setter) => {
        publishSnapshot = setter;
      },
    });

    fireEvent.click(screen.getByLabelText("Edit claude"));
    await waitFor(() => expect(screen.getByRole("dialog", { name: "Edit provider dialog" })).toBeTruthy());
    const remote = snapshotWithConfig(claudeConfig());
    remote.config.current!.providers.claude!.url = "https://remote.example/messages";
    remote.config.resourceBaselines["providers/claude.yaml"]!.persistedEtag = "remote-etag";
    publishSnapshot!(remote);
    fireEvent.input(screen.getByLabelText("URL"), { target: { value: "https://draft.example/messages" } });
    fireEvent.submit(screen.getByLabelText("Edit provider"));

    await waitFor(() => expect(commands.some((command) => command.type === "updateProvider")).toBe(true));
    const update = commands.find((command): command is Extract<ClientCommand, { type: "updateProvider" }> => command.type === "updateProvider")!;
    expect(update.baseline.persistedEtag).toBe(initial.config.resourceBaselines["providers/claude.yaml"]!.persistedEtag);
    expect(update.original.url).toBe("https://api.anthropic.com/v1/messages");
    expect(update.provider.url).toBe("https://draft.example/messages");
  });
});
