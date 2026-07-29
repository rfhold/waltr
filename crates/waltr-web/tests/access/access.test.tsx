import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { afterEach, describe, expect, test, vi } from "vitest";

import type { AuthAttemptSnapshot, ClientCommand, ClientSnapshot } from "../../src/generated/waltr-client";
import { AccessRoute, waltrLoginUrl, safeLocalReturnPath } from "../../src/routes/access/AccessRoute";
import { renderWithAppState } from "../support/render";
import { ShellRouteWithChildren, testHistory } from "../support/routes";
import { operationSnapshot, snapshotWithConfig } from "../support/snapshots";

afterEach(() => {
  vi.unstubAllGlobals();
  window.history.replaceState({}, "", "/");
});

describe("Waltr access", () => {
  test("starts login with the returned attempt ID and preserves the fragment", async () => {
    window.history.replaceState({}, "", "/sessions/session-a?panel=activity#turn-3");
    const commands: ClientCommand[] = [];
    const navigate = vi.fn();
    const started = accessSnapshot("loginInProgress", "attempt-1");

    renderWithAppState(() => <AccessRoute navigate={navigate} />, {
      snapshot: accessSnapshot("loginRequired"),
      commands,
      onDispatch: (command, snapshot, setSnapshot) => {
        if (command.type === "startWaltrLogin") {
          setSnapshot(started);
          return started;
        }
        return snapshot!;
      },
    });

    const login = screen.getByRole("link", { name: "Sign in" });
    expect(login.getAttribute("href")).toBe("/oidc/login?return_to=%2Fsessions%2Fsession-a%3Fpanel%3Dactivity%23turn-3");
    fireEvent.click(login);

    await waitFor(() => expect(commands).toEqual([{ type: "startWaltrLogin" }]));
    expect(navigate).toHaveBeenCalledWith("/oidc/login?return_to=%2Fsessions%2Fsession-a%3Fpanel%3Dactivity%23turn-3");
  });

  test("reports navigation failure against its own attempt", async () => {
    const commands: ClientCommand[] = [];
    const started = accessSnapshot("loginInProgress", "attempt-failed");
    const failed = accessSnapshot("loginFailed", null, "Unable to open the sign-in page.");
    renderWithAppState(() => <AccessRoute navigate={() => Promise.reject(new Error("blocked"))} />, {
      snapshot: accessSnapshot("loginRequired"),
      commands,
      onDispatch: (command, snapshot, setSnapshot) => {
        if (command.type === "startWaltrLogin") {
          setSnapshot(started);
          return started;
        }
        if (command.type === "failWaltrLogin") {
          setSnapshot(failed);
          return failed;
        }
        return snapshot!;
      },
    });

    fireEvent.click(screen.getByRole("link", { name: "Sign in" }));

    await waitFor(() => expect(commands).toContainEqual({
      type: "failWaltrLogin",
      attemptId: "attempt-failed",
      error: "Unable to open the sign-in page.",
    }));
    expect(screen.getByRole("heading", { name: "Login did not complete" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "Try again" })).toBeTruthy();
  });

  test("ignores a stale navigation failure after a newer attempt starts", async () => {
    let rejectFirst!: (error: Error) => void;
    const firstNavigation = new Promise<void>((_resolve, reject) => { rejectFirst = reject; });
    const navigate = vi.fn()
      .mockReturnValueOnce(firstNavigation)
      .mockResolvedValueOnce(undefined);
    const commands: ClientCommand[] = [];
    let setSnapshot!: (snapshot: ClientSnapshot) => void;
    let attempt = 0;
    renderWithAppState(() => <AccessRoute navigate={navigate} />, {
      snapshot: accessSnapshot("loginRequired"),
      commands,
      onSnapshotSetter: (setter) => { setSnapshot = setter; },
      onDispatch: (command, snapshot, publish) => {
        if (command.type === "startWaltrLogin") {
          attempt += 1;
          const started = accessSnapshot("loginInProgress", `attempt-${attempt}`);
          publish(started);
          return started;
        }
        return snapshot!;
      },
    });

    fireEvent.click(screen.getByRole("link", { name: "Sign in" }));
    await waitFor(() => expect(navigate).toHaveBeenCalledTimes(1));

    setSnapshot(accessSnapshot("loginRequired"));
    fireEvent.click(screen.getByRole("link", { name: "Sign in" }));
    await waitFor(() => expect(navigate).toHaveBeenCalledTimes(2));
    rejectFirst(new Error("late failure"));

    await Promise.resolve();
    expect(commands.filter((command) => command.type === "failWaltrLogin")).toEqual([]);
    expect(commands.filter((command) => command.type === "startWaltrLogin")).toHaveLength(2);
  });

  test("cancels the active attempt with its ID", async () => {
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <AccessRoute navigate={vi.fn()} />, {
      snapshot: accessSnapshot("loginInProgress", "attempt-cancel"),
      commands,
    });

    fireEvent.click(screen.getByRole("button", { name: "Cancel login" }));

    await waitFor(() => expect(commands).toContainEqual({ type: "cancelWaltrLogin", attemptId: "attempt-cancel" }));
  });

  test("validates encoded return paths through bounded decoding", () => {
    expect(safeLocalReturnPath(locationParts("/config/provider", "?edit=1", "#model"))).toBe("/config/provider?edit=1#model");
    expect(safeLocalReturnPath(locationParts("//evil.example/path"))).toBe("/");
    expect(safeLocalReturnPath(locationParts("/%2Fevil.example"))).toBe("/");
    expect(safeLocalReturnPath(locationParts("/%252Fevil.example"))).toBe("/");
    expect(safeLocalReturnPath(locationParts("/%5Cevil.example"))).toBe("/");
    expect(safeLocalReturnPath(locationParts("/%255Cevil.example"))).toBe("/");
    expect(safeLocalReturnPath(locationParts("/safe", "?value=%0Aadmin"))).toBe("/");
    expect(safeLocalReturnPath(locationParts("/safe", "?value=%C2%80admin"))).toBe("/");
    expect(safeLocalReturnPath(locationParts("/safe", "?bad=%zz"))).toBe("/");
    expect(safeLocalReturnPath(locationParts("/https:%2F%2Fevil.example"))).toBe("/");
    expect(safeLocalReturnPath(locationParts("/%2525252520"))).toBe("/");
    expect(waltrLoginUrl(locationParts("//evil.example"))).toBe("/oidc/login?return_to=%2F");
  });

  test("session expiry replaces protected content and provider OAuth", async () => {
    let setSnapshot!: (snapshot: ClientSnapshot) => void;
    const authenticated = outboundAuthSnapshot({ target: { kind: "provider", key: "claude" } });
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/",
      snapshot: authenticated,
      explicitAuthTarget: { kind: "provider", key: "claude" },
      onSnapshotSetter: (setter) => { setSnapshot = setter; },
    });

    expect(screen.getByRole("dialog", { name: "OAuth authorization" })).toBeTruthy();
    setSnapshot(accessSnapshot("loginRequired"));

    await waitFor(() => expect(screen.getByRole("heading", { name: "Sign in to continue" })).toBeTruthy());
    expect(screen.queryByLabelText("Primary navigation")).toBeNull();
    expect(screen.queryByRole("dialog", { name: "OAuth authorization" })).toBeNull();
  });

  test.each([
    ["pending", "Waiting for the server callback. This dialog updates automatically."],
    ["completing", "Completing authorization."],
    ["succeeded", "Authorization succeeded."],
    ["failed", "Authorization failed."],
    ["expired", "Authorization expired."],
    ["canceled", "Authorization canceled."],
  ])("renders the shared outbound OAuth %s lifecycle", (lifecycle, message) => {
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions",
      snapshot: outboundAuthSnapshot({ lifecycle, failureReason: lifecycle === "failed" ? "safe failure" : "" }),
      commands,
      explicitAuthTarget: { kind: "mcpServer", key: "remote" },
    });

    expect(screen.getByText(message)).toBeTruthy();
    expect(screen.getByText("Server callback")).toBeTruthy();
    expect(screen.getByText("Dynamic")).toBeTruthy();
    expect(screen.getByText("https://auth.example/authorize")).toBeTruthy();
    if (["succeeded", "failed", "expired", "canceled"].includes(lifecycle)) {
      expect(screen.queryByRole("link", { name: "Open authorization URL" })).toBeNull();
      expect(screen.getByRole("button", { name: "Dismiss" })).toBeTruthy();
    } else {
      expect(screen.getByRole("link", { name: "Open authorization URL" }).getAttribute("href")).toBe("https://auth.example/authorize");
      expect(screen.getByRole("button", { name: "Cancel" })).toBeTruthy();
    }
    expect(screen.queryByPlaceholderText("Paste the final redirect URL or authorization code")).toBeNull();
    expect(commands.some((command) => command.type === "pollAuthAttempt")).toBe(false);
  });

  test("presents an explicitly initiated OAuth attempt without a session requirement", () => {
    const snapshot = outboundAuthSnapshot({ attemptId: "explicit-attempt" });

    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/config/provider",
      snapshot,
      explicitAuthTarget: { kind: "mcpServer", key: "remote" },
    });

    expect(screen.getByRole("dialog", { name: "OAuth authorization" })).toBeTruthy();
    expect(screen.getByText("Authenticate MCP server remote.")).toBeTruthy();
    expect(screen.queryByText(/continue agent work/i)).toBeNull();
  });

  test("shows preregistered MCP attempts and omits registration for providers", () => {
    const mcpSnapshot = outboundAuthSnapshot({ registrationMethod: "preregistered" });
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions",
      snapshot: mcpSnapshot,
      explicitAuthTarget: { kind: "mcpServer", key: "remote" },
    });
    expect(screen.getByText("Preregistered")).toBeTruthy();

    const providerSnapshot = outboundAuthSnapshot({
      target: { kind: "provider", key: "claude" },
      registrationMethod: "preregistered",
    });
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions",
      snapshot: providerSnapshot,
      explicitAuthTarget: { kind: "provider", key: "claude" },
    });
    const providerDialog = screen.getAllByRole("dialog", { name: "OAuth authorization" }).at(-1)!;
    expect(providerDialog.textContent).not.toContain("Registration");
    expect(providerDialog.textContent).not.toContain("Preregistered");
  });

  test("does not render an active attempt unrelated to the explicit target", () => {
    const snapshot = outboundAuthSnapshot({ attemptId: "unrelated-attempt" });
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions",
      snapshot,
      explicitAuthTarget: { kind: "provider", key: "claude" },
    });

    expect(screen.queryByRole("dialog", { name: /OAuth/ })).toBeNull();
  });

  test("explicit dismissal and cancellation clear local presentation origin", async () => {
    const terminal = outboundAuthSnapshot({ lifecycle: "succeeded", attemptId: "explicit-terminal" });
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions",
      snapshot: terminal,
      explicitAuthTarget: { kind: "mcpServer", key: "remote" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "OAuth authorization" })).toBeNull());

    const pending = outboundAuthSnapshot({ lifecycle: "pending", attemptId: "explicit-pending" });
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions",
      snapshot: pending,
      explicitAuthTarget: { kind: "mcpServer", key: "remote" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "OAuth authorization" })).toBeNull());
  });

  test("explicit manual completion clears local presentation origin", async () => {
    const commands: ClientCommand[] = [];
    const snapshot = outboundAuthSnapshot({
      completionMode: "manual",
      lifecycle: "pending",
      attemptId: "explicit-manual",
    });
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions",
      snapshot,
      commands,
      explicitAuthTarget: { kind: "mcpServer", key: "remote" },
    });

    fireEvent.input(screen.getByPlaceholderText("Paste the final redirect URL or authorization code"), { target: { value: "callback-code" } });
    fireEvent.click(screen.getByRole("button", { name: "Complete OAuth" }));

    await waitFor(() => expect(commands).toContainEqual({ type: "completeAuth", attemptId: "explicit-manual", input: "callback-code" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "OAuth authorization" })).toBeNull());
  });

  test("rejects unsafe authorization URLs and bounds sanitized failure text", () => {
    const failure = `line\u0000break${"x".repeat(600)}`;
    renderWithAppState(() => <ShellRouteWithChildren />, {
      snapshot: outboundAuthSnapshot({
        lifecycle: "failed",
        authUrl: "javascript:alert(document.domain)",
        failureReason: failure,
      }),
      explicitAuthTarget: { kind: "mcpServer", key: "remote" },
    });

    expect(screen.queryByRole("link", { name: "Open authorization URL" })).toBeNull();
    expect(screen.getByText("Authorization URL unavailable.")).toBeTruthy();
    expect(screen.queryByText("javascript:alert(document.domain)")).toBeNull();
    const renderedFailure = screen.getAllByRole("alert").map((alert) => alert.textContent).find((text) => text?.startsWith("line break"));
    expect(renderedFailure).not.toContain("\u0000");
    expect(renderedFailure!.length).toBe(500);
  });

  test("does not flash the protected shell while access is unresolved", () => {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions",
      snapshot: accessSnapshot("unknownChecking"),
    });

    expect(screen.getByLabelText("Waltr splash screen")).toBeTruthy();
    expect(screen.queryByLabelText("Primary navigation")).toBeNull();
  });

  test("an authenticated callback return restores the intended shell route", async () => {
    let setSnapshot!: (snapshot: ClientSnapshot) => void;
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions",
      snapshot: accessSnapshot("unknownChecking"),
      onSnapshotSetter: (setter) => { setSnapshot = setter; },
    });

    expect(screen.getByLabelText("Waltr splash screen")).toBeTruthy();
    setSnapshot(accessSnapshot("authenticated"));

    await waitFor(() => expect(testHistory().get()).toBe("/sessions"));
    await waitFor(() => expect(screen.getByLabelText("Primary navigation")).toBeTruthy());
  });

  test.each([
    ["HTTP", vi.fn().mockResolvedValue({ ok: false })],
    ["network", vi.fn().mockRejectedValue(new Error("offline"))],
  ])("%s logout failure preserves authenticated shell state", async (_kind, fetchMock) => {
    vi.stubGlobal("fetch", fetchMock);
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/config/provider",
      snapshot: accessSnapshot("authenticated"),
      commands,
    });

    fireEvent.click(screen.getByRole("button", { name: "Sign out" }));

    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(screen.getByLabelText("Primary navigation")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Sign out" })).toBeTruthy();
    expect(commands.some((command) => command.type === "logoutWaltr")).toBe(false);
  });

  test("successful logout revokes the cookie before clearing local state", async () => {
    let resolveLogout!: (response: { ok: boolean }) => void;
    const request = new Promise<{ ok: boolean }>((resolve) => { resolveLogout = resolve; });
    const fetchMock = vi.fn().mockReturnValue(request);
    vi.stubGlobal("fetch", fetchMock);
    const commands: ClientCommand[] = [];
    const signedOut = accessSnapshot("loginRequired");
    renderWithAppState(() => <ShellRouteWithChildren />, {
      snapshot: accessSnapshot("authenticated"),
      commands,
      onDispatch: (command, snapshot, setSnapshot) => {
        if (command.type === "logoutWaltr") {
          setSnapshot(signedOut);
          return signedOut;
        }
        return snapshot!;
      },
    });

    fireEvent.click(screen.getByRole("button", { name: "Sign out" }));
    expect(screen.getByRole("button", { name: "Signing out..." })).toHaveProperty("disabled", true);
    expect(commands.some((command) => command.type === "logoutWaltr")).toBe(false);
    resolveLogout({ ok: true });

    await waitFor(() => expect(commands).toContainEqual({ type: "logoutWaltr" }));
    expect(fetchMock).toHaveBeenCalledWith("/oidc/logout", { method: "POST", credentials: "same-origin" });
    await waitFor(() => expect(screen.queryByLabelText("Primary navigation")).toBeNull());
  });

  test("reloads after local logout fails behind successful server logout", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true }));
    const reload = vi.fn();
    renderWithAppState(() => <ShellRouteWithChildren reload={reload} />, {
      snapshot: accessSnapshot("authenticated"),
      onDispatch: (command, snapshot) => {
        if (command.type === "logoutWaltr") {
          throw new Error("reducer unavailable");
        }
        return snapshot!;
      },
    });

    fireEvent.click(screen.getByRole("button", { name: "Sign out" }));

    await waitFor(() => expect(reload).toHaveBeenCalledOnce());
    expect(screen.getByRole("alert").textContent).toContain("Reloading");
    expect(screen.getByLabelText("Primary navigation")).toBeTruthy();
  });
});

function accessSnapshot(
  status: ClientSnapshot["access"]["status"],
  activeAttemptId: string | null = null,
  error: string | null = null,
) {
  const snapshot = snapshotWithConfig(
    { etag: status === "authenticated" ? "v1" : "", providers: {}, mcpServers: {}, runtimeDefaults: null },
    { ready: status === "authenticated" },
  );
  snapshot.access = { status, error, activeAttemptId };
  if (status !== "authenticated") {
    snapshot.config.current = null;
    snapshot.config.watch = operationSnapshot("idle");
  }
  return snapshot;
}

function locationParts(pathname: string, search = "", hash = "") {
  return { pathname, search, hash };
}

function outboundAuthSnapshot(overrides: Partial<AuthAttemptSnapshot> = {}) {
  const snapshot = accessSnapshot("authenticated");
  snapshot.auth.activeAttempt = {
    target: { kind: "mcpServer", key: "remote" },
    attemptId: "attempt-1",
    authUrl: "https://auth.example/authorize",
    verificationUrl: "",
    userCode: "",
    expiresAtMs: 1_800_000_000_000n,
    pollIntervalSeconds: 1,
    messages: ["Open the authorization page."],
    completionMode: "serverCallback",
    registrationMethod: "dynamic",
    lifecycle: "pending",
    failureReason: "",
    ...overrides,
  };
  return snapshot;
}
