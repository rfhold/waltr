import { fireEvent, render, screen, waitFor } from "@solidjs/testing-library";
import { createSignal, Show } from "solid-js";
import { describe, expect, test } from "vitest";

import type { AuthStatusSnapshot, ClientCommand, ClientSnapshot } from "../../src/generated/waltr-client";
import { AppStateProvider } from "../../src/app/state";
import { OAuthDialog } from "../../src/routes/auth/OAuthDialog";
import type { ClientAdapter } from "../../src/platform/client";
import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren, ShellRouteWithChildren } from "../support/routes";
import { claudeConfig, mcpServerView, snapshotWithConfig } from "../support/snapshots";

describe("config authentication status", () => {
  test.each([
    ["oauthConfigured", "Connected"],
    ["apiKeyConfigured", "API key configured"],
    ["expired", "Expired"],
    ["authorizationRequired", "Authorization required"],
    ["unsupported", "Unsupported"],
  ])("renders %s provider status", (kind, label) => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      snapshot: providerAuthSnapshot(authStatus("provider", "claude", kind, { canRefresh: kind === "expired" })),
    });

    expect(screen.getByText(label)).toBeTruthy();
    if (["oauthConfigured", "expired", "authorizationRequired"].includes(kind)) expect(screen.getByRole("button", { name: "Reauthorize" })).toBeTruthy();
    if (kind === "apiKeyConfigured" || kind === "unsupported") expect(screen.queryByRole("button", { name: /Authorize|Reauthorize/ })).toBeNull();
    if (kind === "expired") expect(screen.getByRole("button", { name: "Force refresh" })).toBeTruthy();
  });

  test("shows MCP registration status without exposing registration configuration", async () => {
    const config = claudeConfig();
    config.mcpServers.remote = mcpServerView({ name: "remote", kind: "streamableHttp", url: "https://mcp.example" });
    const snapshot = snapshotWithConfig(config);
    snapshot.auth.statuses["mcp:remote"] = authStatus("mcpServer", "remote", "oauthConfigured", { registrationMethod: "preregistered" });

    renderWithAppState(() => <ConfigRouteWithChildren />, { snapshot });
    fireEvent.click(screen.getByRole("tab", { name: "MCP Servers" }));

    expect(screen.getByText("Preregistered")).toBeTruthy();
    fireEvent.click(screen.getByLabelText("Edit remote"));
    await waitFor(() => expect(screen.getByRole("dialog", { name: "Edit MCP server dialog" })).toBeTruthy());
    expect(screen.queryByLabelText(/registration/i)).toBeNull();
    expect(screen.queryByLabelText(/client metadata/i)).toBeNull();
  });

  test("shows bounded status-list failures, retained-state warning, and retry", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = providerAuthSnapshot(authStatus("provider", "claude", "oauthConfigured"));
    snapshot.auth.list = { status: "failed", error: `status\u0000failure${"x".repeat(600)}`, key: "list-1" };

    renderWithAppState(() => <ConfigRouteWithChildren />, { commands, snapshot });

    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("status failure");
    expect(alert.textContent).toContain("Authentication statuses may be stale.");
    expect(alert.textContent).not.toContain("\u0000");
    expect(alert.textContent!.length).toBeLessThan(550);
    fireEvent.click(screen.getByRole("button", { name: "Retry status" }));
    await waitFor(() => expect(commands.filter((command) => command.type === "requestAuthStatuses").length).toBeGreaterThan(1));
  });
});

describe("config authentication actions", () => {
  test("restores a handed-off pending dialog and preserves its draft across dialog remount", async () => {
    const snapshot = providerAuthSnapshot(authStatus("provider", "claude", "notConfigured"));
    snapshot.auth.activeAttempt = {
      target: { kind: "provider", key: "claude" },
      attemptId: "handed-off-attempt",
      authUrl: "",
      verificationUrl: "",
      userCode: "",
      expiresAtMs: 1_800_000_000_000n,
      pollIntervalSeconds: 1,
      messages: [],
      completionMode: "manual",
      registrationMethod: "",
      lifecycle: "pending",
      failureReason: "",
    };
    let listener: ((snapshot: ClientSnapshot) => void) | undefined;
    const adapter: ClientAdapter = {
      snapshot: () => snapshot,
      start: async () => undefined,
      subscribe: (next) => {
        listener = next;
        return () => undefined;
      },
      dispatch: async () => snapshot,
      dispose: () => undefined,
      openAuthorizationAttempt: async () => undefined,
    };
    const Harness = () => {
      const [mounted, setMounted] = createSignal(true);
      return <>
        <button type="button" onClick={() => setMounted(false)}>Unmount OAuth</button>
        <button type="button" onClick={() => setMounted(true)}>Remount OAuth</button>
        <Show when={mounted()}><OAuthDialog /></Show>
      </>;
    };
    render(() => (
      <AppStateProvider loadClientAdapter={async () => adapter}>
        <Harness />
      </AppStateProvider>
    ));

    const dialog = await screen.findByRole("dialog", { name: "OAuth authorization" });
    expect(dialog.textContent).toContain("Authenticate provider claude.");
    const input = screen.getByLabelText("Final redirect URL or code") as HTMLTextAreaElement;
    fireEvent.input(input, { target: { value: "draft callback value" } });

    listener?.({ ...snapshot, auth: { ...snapshot.auth, activeAttempt: { ...snapshot.auth.activeAttempt! } } });
    fireEvent.click(screen.getByRole("button", { name: "Unmount OAuth" }));
    expect(screen.queryByRole("dialog", { name: "OAuth authorization" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Remount OAuth" }));
    expect((await screen.findByLabelText("Final redirect URL or code") as HTMLTextAreaElement).value)
      .toBe("draft callback value");

    listener?.({ ...snapshot, auth: { ...snapshot.auth, activeAttempt: null } });
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "OAuth authorization" })).toBeNull());
    listener?.({ ...snapshot, auth: { ...snapshot.auth, activeAttempt: { ...snapshot.auth.activeAttempt! } } });
    await Promise.resolve();
    expect(screen.queryByRole("dialog", { name: "OAuth authorization" })).toBeNull();
  });

  test("does not transfer presentation or draft to a same-target replacement attempt", async () => {
    const snapshot = providerAuthSnapshot(authStatus("provider", "claude", "notConfigured"));
    snapshot.auth.activeAttempt = {
      target: { kind: "provider", key: "claude" },
      attemptId: "attempt-old",
      authUrl: "https://auth.example/old",
      verificationUrl: "",
      userCode: "",
      expiresAtMs: 1_800_000_000_000n,
      pollIntervalSeconds: 1,
      messages: [],
      completionMode: "manual",
      registrationMethod: "",
      lifecycle: "pending",
      failureReason: "",
    };
    let listener: ((snapshot: ClientSnapshot) => void) | undefined;
    const adapter: ClientAdapter = {
      snapshot: () => snapshot,
      start: async () => undefined,
      subscribe: (next) => {
        listener = next;
        return () => undefined;
      },
      dispatch: async () => snapshot,
      dispose: () => undefined,
    };
    render(() => (
      <AppStateProvider loadClientAdapter={async () => adapter}>
        <OAuthDialog />
      </AppStateProvider>
    ));

    const input = await screen.findByLabelText("Final redirect URL or code") as HTMLTextAreaElement;
    fireEvent.input(input, { target: { value: "old attempt draft" } });
    listener?.({
      ...snapshot,
      auth: {
        ...snapshot.auth,
        activeAttempt: {
          ...snapshot.auth.activeAttempt!,
          attemptId: "attempt-new",
          authUrl: "https://auth.example/new",
        },
      },
    });

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "OAuth authorization" })).toBeNull());
    expect(screen.queryByText("https://auth.example/new")).toBeNull();
  });

  test("marks config authorization explicit and presents its attempt without a requirement", async () => {
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/config/provider",
      commands,
      snapshot: providerAuthSnapshot(authStatus("provider", "claude", "notConfigured")),
      onDispatch: (command, snapshot, setSnapshot) => {
        if (command.type !== "beginAuth") return snapshot!;
        const next: ClientSnapshot = {
          ...snapshot!,
          auth: {
            ...snapshot!.auth,
            activeAttempt: {
              target: command.target,
              attemptId: "config-attempt",
              authUrl: "https://auth.example/authorize",
              verificationUrl: "",
              userCode: "",
              expiresAtMs: 1_800_000_000_000n,
              pollIntervalSeconds: 1,
              messages: [],
              completionMode: "manual",
              registrationMethod: "",
              lifecycle: "pending",
              failureReason: "",
            },
          },
        };
        setSnapshot(next);
        return next;
      },
    });

    fireEvent.click(await screen.findByRole("button", { name: "Authorize" }));
    const dialog = await screen.findByRole("dialog", { name: "OAuth authorization" });
    expect(dialog.classList.contains("panel")).toBe(true);
    expect(screen.getByText("Authenticate provider claude.")).toBeTruthy();
    expect(screen.getByText("Manual")).toBeTruthy();
    const completionInput = screen.getByLabelText("Final redirect URL or code") as HTMLTextAreaElement;
    expect(completionInput.value).toBe("");
    expect(completionInput.placeholder).toBe("Paste the final redirect URL or authorization code");
    expect(completionInput.closest(".ui-field")).toBeTruthy();
    const footer = dialog.querySelector(".config__form-actions")!;
    expect(footer.contains(screen.getByRole("link", { name: "Open authorization URL" }))).toBe(true);
    expect(footer.contains(screen.getByRole("button", { name: "Complete OAuth" }))).toBe(true);
    expect(footer.contains(screen.getByRole("button", { name: "Cancel" }))).toBe(true);
    expect(completionInput.closest(".ui-field")!.contains(screen.getByRole("button", { name: "Complete OAuth" }))).toBe(false);
    expect(commands).toContainEqual({ type: "beginAuth", target: { kind: "provider", key: "claude" } });
  });

  test.each([
    ["notConfigured", "Authorize"],
    ["oauthConfigured", "Reauthorize"],
  ])("dispatches %s authorization from the matching target", async (kind, button) => {
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      snapshot: providerAuthSnapshot(authStatus("provider", "claude", kind)),
    });

    fireEvent.click(screen.getByRole("button", { name: button }));
    await waitFor(() => expect(commands).toContainEqual({ type: "beginAuth", target: { kind: "provider", key: "claude" } }));
  });

  test("reports forced refresh failure and requests fresh statuses after success", async () => {
    const failedCommands: ClientCommand[] = [];
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands: failedCommands,
      snapshot: providerAuthSnapshot(authStatus("provider", "claude", "expired", { canRefresh: true })),
      onDispatch: (command, snapshot) => {
        if (command.type === "refreshAuth") throw new Error("refresh token rejected\u0000");
        return snapshot!;
      },
    });

    fireEvent.click(screen.getByRole("button", { name: "Force refresh" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("refresh token rejected"));
    expect(screen.getByRole("alert").textContent).not.toContain("\u0000");

    const successCommands: ClientCommand[] = [];
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands: successCommands,
      snapshot: providerAuthSnapshot(authStatus("provider", "claude", "expired", { canRefresh: true })),
    });
    fireEvent.click(screen.getAllByRole("button", { name: "Force refresh" }).at(-1)!);
    await waitFor(() => expect(successCommands.filter((command) => command.type === "requestAuthStatuses").length).toBeGreaterThan(1));
    expect(successCommands).toContainEqual({ type: "refreshAuth", target: { kind: "provider", key: "claude" } });
  });

  test("requires reset confirmation and honors cancellation", async () => {
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      commands,
      snapshot: providerAuthSnapshot(authStatus("provider", "claude", "oauthConfigured")),
    });

    fireEvent.click(screen.getByRole("button", { name: "Reset" }));
    fireEvent.click(screen.getByLabelText("Cancel reset authentication for claude"));
    expect(commands.some((command) => command.type === "logoutAuth")).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: "Reset" }));
    fireEvent.click(screen.getByLabelText("Confirm reset authentication for claude"));
    await waitFor(() => expect(commands).toContainEqual({ type: "logoutAuth", target: { kind: "provider", key: "claude" } }));
  });

  test("preserves provider drafts while credential actions run", async () => {
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      routePath: "/config/provider/edit/claude",
      snapshot: providerAuthSnapshot(authStatus("provider", "claude", "oauthConfigured", { canRefresh: true })),
    });

    await waitFor(() => expect(screen.getByRole("dialog", { name: "Edit provider dialog" })).toBeTruthy());
    fireEvent.input(screen.getByLabelText("URL"), { target: { value: "https://draft.example/v1" } });
    fireEvent.click(screen.getByRole("button", { name: "Force refresh" }));
    await waitFor(() => expect((screen.getByLabelText("URL") as HTMLInputElement).value).toBe("https://draft.example/v1"));
  });

  test("preserves an MCP draft across new snapshot identities", async () => {
    let setSnapshot!: (snapshot: ClientSnapshot) => void;
    const initial = mcpAuthSnapshot("https://mcp.example/original");
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      routePath: "/config/mcp",
      snapshot: initial,
      onSnapshotSetter: (setter) => { setSnapshot = setter; },
    });

    fireEvent.click(await screen.findByLabelText("Edit remote"));
    await waitFor(() => expect(screen.getByRole("dialog", { name: "Edit MCP server dialog" })).toBeTruthy());
    fireEvent.input(screen.getByLabelText("URL"), { target: { value: "https://mcp.example/draft" } });

    setSnapshot(mcpAuthSnapshot("https://mcp.example/new-authoritative"));
    await waitFor(() => expect(screen.getByText("Connected")).toBeTruthy());
    expect((screen.getByLabelText("URL") as HTMLInputElement).value).toBe("https://mcp.example/draft");
  });

  test("initializes a direct MCP edit route after config arrives without replacing its draft", async () => {
    let setSnapshot!: (snapshot: ClientSnapshot) => void;
    const loading = mcpAuthSnapshot("https://mcp.example/loading");
    loading.config.current = null;
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      routePath: "/config/mcp/edit/remote",
      snapshot: loading,
      onSnapshotSetter: (setter) => { setSnapshot = setter; },
    });

    expect(screen.queryByRole("dialog", { name: "Edit MCP server dialog" })).toBeNull();
    setSnapshot(mcpAuthSnapshot("https://mcp.example/initial"));
    await waitFor(() => expect(screen.getByRole("dialog", { name: "Edit MCP server dialog" })).toBeTruthy());
    fireEvent.input(screen.getByLabelText("URL"), { target: { value: "https://mcp.example/draft" } });

    setSnapshot(mcpAuthSnapshot("https://mcp.example/new-authoritative"));
    await waitFor(() => expect(screen.getByText("Connected")).toBeTruthy());
    expect((screen.getByLabelText("URL") as HTMLInputElement).value).toBe("https://mcp.example/draft");
  });
});

function providerAuthSnapshot(status: AuthStatusSnapshot): ClientSnapshot {
  const snapshot = snapshotWithConfig(claudeConfig());
  snapshot.auth.statuses[`provider:${status.target.key}`] = status;
  return snapshot;
}

function mcpAuthSnapshot(url: string): ClientSnapshot {
  const config = claudeConfig();
  config.mcpServers.remote = mcpServerView({ name: "remote", kind: "streamableHttp", url });
  const snapshot = snapshotWithConfig(config);
  snapshot.auth.statuses["mcp:remote"] = authStatus("mcpServer", "remote", "oauthConfigured", { registrationMethod: "preregistered" });
  return snapshot;
}

function authStatus(
  targetKind: "provider" | "mcpServer",
  key: string,
  kind: string,
  overrides: Partial<AuthStatusSnapshot> = {},
): AuthStatusSnapshot {
  return {
    target: { kind: targetKind, key },
    kind,
    supportedMethods: kind === "unsupported" ? [] : ["oauthManual"],
    expiresAtMs: kind === "oauthConfigured" || kind === "expired" ? 1_800_000_000_000n : 0n,
    metadata: {},
    message: kind === "authorizationRequired" ? "Authorization is required." : "",
    registrationMethod: "",
    canRefresh: false,
    ...overrides,
  };
}
