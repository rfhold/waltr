import { fireEvent, screen } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import type { AuthStatusSnapshot, ClientSnapshot, ConfigView } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { ConfigRouteWithChildren, ShellRouteWithChildren } from "../support/routes";
import { claudeConfig, mcpServerView, snapshotWithConfig } from "../support/snapshots";

describe("OAuth config presentation", () => {
  test("provider public-client editor and credential reset flow", async () => {
    const snapshot = providerSnapshot();
    renderWithAppState(() => <ConfigRouteWithChildren />, {
      routePath: "/config/provider/edit/claude",
      snapshot,
    });

    const form = await screen.findByLabelText("Edit provider");
    const oauthEditor = form.querySelector<HTMLElement>(".config__form-fieldset")!;
    expect(stableControls(oauthEditor)).toMatchSnapshot("populated provider public-client editor");

    fireEvent.click(screen.getByRole("button", { name: "Reset" }));
    const authPanel = screen.getByLabelText("Authentication for claude");
    expect(stableAuthPanel(authPanel)).toMatchSnapshot("provider connected status with reset confirmation");

    expect(form.textContent?.toLowerCase()).not.toContain("client secret");
    expect(authPanel.textContent).not.toContain("Registration");
  });

  test("provider retained status after auth-list failure", async () => {
    const snapshot = providerSnapshot();
    snapshot.auth.list = { status: "failed", error: "Status service unavailable.", key: "status-list-1" };
    renderWithAppState(() => <ConfigRouteWithChildren />, { snapshot });

    const authPanel = await screen.findByLabelText("Authentication for claude");
    expect(stableAuthPanel(authPanel)).toMatchSnapshot("retained provider status with stale warning and retry");
    expect(screen.getByRole("button", { name: "Retry status" })).toBeTruthy();
  });

  test("MCP expired credential status and registration", async () => {
    const snapshot = mcpSnapshot();
    renderWithAppState(() => <ConfigRouteWithChildren />, { snapshot });

    fireEvent.click(screen.getByRole("tab", { name: "MCP Servers" }));
    const authPanel = await screen.findByLabelText("Authentication for remote");
    expect(stableAuthPanel(authPanel)).toMatchSnapshot("expired preregistered MCP credential actions");

    expect(screen.queryByLabelText(/registration/i)).toBeNull();
    expect(screen.queryByLabelText(/client metadata/i)).toBeNull();
  });

  test("explicit config-originated manual provider dialog", async () => {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/config/provider",
      snapshot: providerSnapshot("notConfigured"),
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
              messages: ["Authorize in the provider window, then return here."],
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
    expect(dialog.querySelector(".oauth-dialog__body")).toMatchSnapshot("explicit provider manual authorization dialog");

    expect(dialog.textContent).not.toContain("Registration");
    expect(dialog.textContent).not.toContain("continue agent work");
    const input = screen.getByLabelText("Final redirect URL or code");
    const footer = dialog.querySelector(".config__form-actions")!;
    expect(input.closest(".ui-field")).toBeTruthy();
    expect(footer.contains(screen.getByRole("button", { name: "Complete OAuth" }))).toBe(true);
    expect(footer.contains(screen.getByRole("button", { name: "Cancel" }))).toBe(true);
  });

});

function providerSnapshot(kind = "oauthConfigured") {
  const config: ConfigView = claudeConfig();
  config.providers.claude = {
    ...config.providers.claude!,
    oauthCompatible: true,
    oauthSettings: {
      name: "Claude OAuth",
      authorizeUrl: "https://id.example/authorize",
      tokenUrl: "https://id.example/token",
      clientId: "waltr-browser-public",
      redirectUri: "http://127.0.0.1:8765/callback",
      scopes: ["openid", "profile", "offline_access"],
      tokenRequestStyle: "formUrlencoded",
      authUrlParams: [{ key: "audience", value: "https://api.example" }],
      jwtMetadata: [{ token: "id", metadataKey: "subject", claimNames: ["sub", "uid"], required: true }],
      sendStateInTokenRequest: true,
    },
  };
  const snapshot = snapshotWithConfig(config);
  snapshot.auth.statuses["provider:claude"] = authStatus("provider", "claude", kind, {
    canRefresh: kind === "oauthConfigured",
  });
  return snapshot;
}

function mcpSnapshot() {
  const config = claudeConfig();
  config.mcpServers.remote = mcpServerView({ name: "remote", kind: "sse", url: "https://mcp.example" });
  const snapshot = snapshotWithConfig(config);
  snapshot.auth.statuses["mcp:remote"] = authStatus("mcpServer", "remote", "expired", {
    expiresAtMs: 1_800_000_000_000n,
    registrationMethod: "preregistered",
    canRefresh: true,
  });
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
    supportedMethods: ["oauthManual"],
    expiresAtMs: kind === "oauthConfigured" ? 1_800_000_000_000n : 0n,
    metadata: {},
    message: "",
    registrationMethod: "",
    canRefresh: false,
    ...overrides,
  };
}

function stableAuthPanel(panel: HTMLElement) {
  const clone = panel.cloneNode(true) as HTMLElement;
  for (const term of clone.querySelectorAll("dt")) {
    if (term.textContent === "Expires") {
      const value = term.nextElementSibling;
      if (value) value.textContent = "[expiration timestamp]";
    }
  }
  return clone;
}

function stableControls(container: HTMLElement) {
  const clone = container.cloneNode(true) as HTMLElement;
  const sourceControls = container.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>("input, select, textarea");
  const clonedControls = clone.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>("input, select, textarea");
  sourceControls.forEach((source, index) => {
    const target = clonedControls[index];
    if (!target) return;
    if (source instanceof HTMLInputElement && ["checkbox", "radio"].includes(source.type)) {
      if (source.checked) target.setAttribute("checked", "");
      else target.removeAttribute("checked");
      return;
    }
    if (source instanceof HTMLSelectElement && target instanceof HTMLSelectElement) {
      for (const option of target.options) option.toggleAttribute("selected", option.value === source.value);
      return;
    }
    target.setAttribute("value", source.value);
  });
  return stableAuthPanel(clone);
}
