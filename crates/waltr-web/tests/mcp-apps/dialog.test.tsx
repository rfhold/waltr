import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { AppDialog } from "../../src/mcp-apps/AppDialog";
import { acceptNativeAppsBootstrap } from "../../src/mcp-apps/policy";
import type { ClientCommand, ClientSnapshot, McpAppView } from "../../src/generated/waltr-client";
import { renderWithAppState } from "../support/render";
import { emptyConfig, snapshotWithConfig } from "../support/snapshots";

const spies = vi.hoisted(() => ({ options: vi.fn(), dispose: vi.fn(async () => {}), ready: vi.fn(async () => {}) }));
vi.mock("../../src/mcp-apps/AppHost", () => ({ AppHost: class {
  ready = spies.ready();
  dispose = spies.dispose;
  constructor(container: HTMLElement, options: unknown) { spies.options(options); const frame = document.createElement("iframe"); frame.title = "MCP App"; container.append(frame); }
} }));
const view: McpAppView = {
  appId: "lease", html: "<p>status</p>",
  toolJson: JSON.stringify({ _meta: { ui: { resourceUri: "ui://fixtures/observation" } }, annotations: null, inputSchema: { additionalProperties: false, properties: {}, type: "object" }, name: "status", outputSchema: null }),
  argumentsJson: "{}",
  resultJson: JSON.stringify({ _meta: { "io.modelcontextprotocol/serverInfo": { name: "waltr", version: "0.1.0" } }, content: [{ text: "Waltr is ready", type: "text" }], resultType: "complete", structuredContent: { observedAtUnixMs: 1791036279151, ready: true } }),
  resourceMetaJson: "{}",
};
beforeEach(() => { vi.clearAllMocks(); vi.stubEnv("VITE_WALTR_MCP_APP_CONTAINER_URL", "https://container.example/container.html"); });
afterEach(() => {
  acceptNativeAppsBootstrap(undefined);
  vi.restoreAllMocks();
  document.head.querySelectorAll('meta[name="waltr-mcp-apps-container-url"]').forEach(marker => marker.remove());
  vi.unstubAllEnvs();
});

function containerMarker(value: string | null) {
  const marker = document.createElement("meta");
  marker.name = "waltr-mcp-apps-container-url";
  if (value !== null) marker.content = value;
  document.head.append(marker);
}

describe.skipIf(__WALTR_BUILD_PLATFORM__ !== "browser")("Apps dialog", () => {
  test("opens through shared commands and revokes on unmount", async () => {
    const commands: ClientCommand[] = [];
    const rendered = renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={() => {}} />, {
      commands, snapshot: snapshotWithConfig(emptyConfig("")),
      onDispatch(command, state) {
        if (command.type !== "openSessionMcpApp") return state!;
        return { ...state!, apps: { current: { lease: view }, requests: { [command.requestId]: { kind: "open", status: "ready", appId: "lease", error: null, resultJson: null } } } };
      },
    });
    await waitFor(() => expect(spies.options).toHaveBeenCalledOnce());
    expect(screen.getByRole("dialog").textContent).toContain("Close app");
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Close app" }));
    const options = spies.options.mock.calls[0][0] as Record<string, unknown>;
    expect(options).not.toHaveProperty("source");
    expect(options).not.toHaveProperty("resourceMetaJson");
    expect(options.platform).toBe("web");
    expect(options.containerUrl).toBe("https://container.example/container.html");
    expect(options.tool).toEqual({ _meta: { ui: { resourceUri: "ui://fixtures/observation" } }, inputSchema: { additionalProperties: false, properties: {}, type: "object" }, name: "status" });
    expect(options.arguments).toEqual({});
    expect(options.result).toEqual(JSON.parse(view.resultJson));
    rendered.unmount();
    expect(spies.dispose).toHaveBeenCalledOnce();
    expect(commands.at(-1)).toEqual({ type: "closeSessionMcpApp", appId: "lease" });
  });
  test("session switch cancels a pending open and prevents late rendering", async () => {
    const commands: ClientCommand[] = [];
    const close = vi.fn();
    let switchSession!: () => void;
    const rendered = renderWithAppState(() => {
      const [id, setId] = createSignal("s");
      switchSession = () => setId("other");
      return <AppDialog sessionId={id()} callId="c" onClose={close} />;
    }, { commands, snapshot: snapshotWithConfig(emptyConfig("")) });
    switchSession();
    await waitFor(() => expect(commands.at(-1)?.type).toBe("cancelSessionMcpAppOpen"));
    expect(close).toHaveBeenCalled();
    expect(spies.options).not.toHaveBeenCalled();
    rendered.unmount();
  });
  test("logout closes a pending open", async () => {
    const commands: ClientCommand[] = [];
    let setSnapshot!: (snapshot: ClientSnapshot) => void;
    const close = vi.fn();
    const state = snapshotWithConfig(emptyConfig(""));
    renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={close} />, { commands, snapshot: state, onSnapshotSetter: setter => { setSnapshot = setter; } });
    setSnapshot({ ...state, access: { ...state.access, status: "loginRequired" } });
    await waitFor(() => expect(close).toHaveBeenCalled());
    expect(commands.at(-1)?.type).toBe("cancelSessionMcpAppOpen");
  });
  test("nonpersisted pagehide disposes, while BFCache pagehide retains the app", () => {
    const commands: ClientCommand[] = [];
    const close = vi.fn();
    renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={close} />, { commands, snapshot: snapshotWithConfig(emptyConfig("")) });
    const persisted = new Event("pagehide");
    Object.defineProperty(persisted, "persisted", { value: true });
    window.dispatchEvent(persisted);
    expect(close).not.toHaveBeenCalled();
    const unload = new Event("pagehide");
    Object.defineProperty(unload, "persisted", { value: false });
    window.dispatchEvent(unload);
    expect(close).toHaveBeenCalledOnce();
    expect(commands.at(-1)?.type).toBe("cancelSessionMcpAppOpen");
  });
  test("focus trap includes the titled iframe", async () => {
    renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={() => {}} />, {
      snapshot: snapshotWithConfig(emptyConfig("")), onDispatch(command, state) {
        if (command.type !== "openSessionMcpApp") return state!;
        return { ...state!, apps: { current: { lease: view }, requests: { [command.requestId]: { kind: "open", status: "ready", appId: "lease", error: null, resultJson: null } } } };
      },
    });
    const frame = await screen.findByTitle("MCP App");
    Object.defineProperty(frame, "offsetParent", { get: () => frame.parentElement });
    const close = screen.getByRole("button", { name: "Close app" });
    Object.defineProperty(close, "offsetParent", { get: () => close.parentElement });
    close.focus();
    fireEvent.keyDown(close, { key: "Tab" });
    expect(document.activeElement).toBe(frame);
    fireEvent.keyDown(frame, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(close);
  });
  test("bounds failures without exposing backend payloads", async () => {
    renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={() => {}} />, {
      snapshot: snapshotWithConfig(emptyConfig("")), onDispatch: async () => { throw new Error("private endpoint/token"); },
    });
    await screen.findByText(/App unavailable/);
    expect(screen.getByRole("dialog").textContent).not.toContain("private endpoint");
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
  });
  test.each([undefined, "not a URL", "javascript:alert(1)", "same-origin"])("rejects unsafe app container configuration %s before opening a lease", (value) => {
    vi.stubEnv("VITE_WALTR_MCP_APP_CONTAINER_URL", value === "same-origin" ? `${window.location.origin}/container.html` : value);
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={() => {}} />, { commands, snapshot: snapshotWithConfig(emptyConfig("")) });
    expect(screen.getByRole("status").textContent).toContain("set VITE_WALTR_MCP_APP_CONTAINER_URL");
    expect(commands).toEqual([]);
    expect(spies.options).not.toHaveBeenCalled();
  });
  test.each([undefined, "https://stale.example/container.html"])("uses the runtime app container marker ahead of build configuration %s", async (buildUrl) => {
    vi.stubEnv("VITE_WALTR_MCP_APP_CONTAINER_URL", buildUrl);
    const runtimeUrl = "http://127.0.0.1:51237/container.html";
    containerMarker(runtimeUrl);
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={() => {}} />, {
      commands, snapshot: snapshotWithConfig(emptyConfig("")), onDispatch(command, state) {
        if (command.type !== "openSessionMcpApp") return state!;
        return { ...state!, apps: { current: { lease: view }, requests: { [command.requestId]: { kind: "open", status: "ready", appId: "lease", error: null, resultJson: null } } } };
      },
    });
    await waitFor(() => expect(spies.options).toHaveBeenCalledOnce());
    expect(spies.options.mock.calls[0][0].containerUrl).toBe(runtimeUrl);
    expect(commands[0].type).toBe("openSessionMcpApp");
    expect(screen.queryByText(/not configured correctly/)).toBeNull();
  });
  test.each([null, "", "not a URL", "same-origin", "duplicate"])("rejects runtime marker %s without falling back to a valid build URL", (value) => {
    containerMarker(value === "same-origin" ? `${window.location.origin}/container.html` : value === "duplicate" ? "http://127.0.0.1:51237/container.html" : value);
    if (value === "duplicate") containerMarker("http://127.0.0.1:51237/container.html");
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={() => {}} />, { commands, snapshot: snapshotWithConfig(emptyConfig("")) });
    expect(screen.getByRole("status").textContent).toContain("Check the server-provided app container URL");
    expect(commands).toEqual([]);
    expect(spies.options).not.toHaveBeenCalled();
  });
  test("explains a local app container initialization timeout and closes the lease", async () => {
    spies.ready.mockRejectedValueOnce(new Error("App initialization timed out"));
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={() => {}} />, {
      commands, snapshot: snapshotWithConfig(emptyConfig("")), onDispatch(command, state) {
        if (command.type !== "openSessionMcpApp") return state!;
        return { ...state!, apps: { current: { lease: view }, requests: { [command.requestId]: { kind: "open", status: "ready", appId: "lease", error: null, resultJson: null } } } };
      },
    });
    await screen.findByText(/App container did not finish loading/);
    expect(commands.at(-1)).toEqual({ type: "closeSessionMcpApp", appId: "lease" });
    expect(spies.dispose).toHaveBeenCalledOnce();
  });
  test("does not misclassify an RPC failure as a local app container timeout", async () => {
    renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={() => {}} />, {
      snapshot: snapshotWithConfig(emptyConfig("")), onDispatch: async () => { throw new Error("App initialization timed out"); },
    });
    await screen.findByText(/App unavailable/);
    expect(screen.queryByText(/App container did not finish loading/)).toBeNull();
  });
});

describe.skipIf(__WALTR_BUILD_PLATFORM__ === "browser")("native Apps capability", () => {
  test("issues no open command or host construction", () => {
    containerMarker("http://127.0.0.1:51237/container.html");
    const commands: ClientCommand[] = [];
    renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={() => {}} />, { commands, snapshot: snapshotWithConfig(emptyConfig("")) });
    expect(screen.getByRole("dialog").textContent).toContain("unavailable on this platform");
    expect(commands).toEqual([]);
    expect(spies.options).not.toHaveBeenCalled();
  });
  test("ordinary packaged startup opens an app without a diagnostic build or validation flag", async () => {
    vi.spyOn(window, "location", "get").mockReturnValue(new URL("tauri://localhost/index.html") as unknown as Location);
    acceptNativeAppsBootstrap({ containerUrl: "http://127.0.0.1:51237/container.html" });
    const commands: ClientCommand[] = [];
    const rendered = renderWithAppState(() => <AppDialog sessionId="s" callId="c" onClose={() => {}} />, {
      commands, snapshot: snapshotWithConfig(emptyConfig("")),
      onDispatch(command, state) {
        if (command.type !== "openSessionMcpApp") return state!;
        return { ...state!, apps: { current: { lease: view }, requests: { [command.requestId]: { kind: "open", status: "ready", appId: "lease", error: null, resultJson: null } } } };
      },
    });
    await waitFor(() => expect(spies.options).toHaveBeenCalledOnce());
    expect(spies.options.mock.calls[0][0]).toMatchObject({ containerUrl: "http://127.0.0.1:51237/container.html", platform: "desktop" });
    expect(commands[0].type).toBe("openSessionMcpApp");
    rendered.unmount();
    expect(commands.at(-1)).toEqual({ type: "closeSessionMcpApp", appId: "lease" });
  });
});
