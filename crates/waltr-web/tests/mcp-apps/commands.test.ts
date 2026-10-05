import { createRoot, createSignal } from "solid-js";
import { afterEach, describe, expect, test, vi } from "vitest";
import { App } from "@modelcontextprotocol/ext-apps";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import type { JSONRPCMessage } from "@modelcontextprotocol/sdk/types.js";
import { AppHost } from "../../src/mcp-apps/AppHost";
import { appPayload, createAppCommands, hasLinkedApp } from "../../src/mcp-apps/commands";
import type { ClientCommand, ClientSnapshot, McpAppView } from "../../src/generated/waltr-client";
import { emptyConfig, snapshotWithConfig } from "../support/snapshots";

export const view: McpAppView = { appId: "lease", html: "<p>app</p>", toolJson: JSON.stringify({ name: "status", inputSchema: { type: "object" } }), argumentsJson: '{"id":1}', resultJson: '{"content":[{"type":"text","text":"original"}]}', resourceMetaJson: "{}" };
afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); });

function setup(timeout = 30_000) {
  const commands: ClientCommand[] = [];
  let state = snapshotWithConfig(emptyConfig(""));
  return createRoot(dispose => {
    const [snapshot, setSnapshot] = createSignal(state);
    const bridge = createAppCommands(snapshot, async command => { commands.push(command); return state; }, timeout);
    return { bridge, commands, dispose, update(next: ClientSnapshot) { state = next; setSnapshot(next); }, get state() { return state; } };
  });
}
function opened(state: ClientSnapshot, id: string) {
  return { ...state, apps: { current: { lease: view }, requests: { [id]: { kind: "open" as const, status: "ready", appId: "lease", error: null, resultJson: null } } } };
}

describe("shared Apps command bridge", () => {
  test("accepts the captured Rust app declaration and delivers SDK tool input/result unchanged", async () => {
    const tool = { _meta: { ui: { resourceUri: "ui://fixtures/observation" } }, annotations: null, inputSchema: { additionalProperties: false, properties: {}, type: "object" }, name: "status", outputSchema: null };
    const result = { _meta: { "io.modelcontextprotocol/serverInfo": { name: "waltr", version: "0.1.0" } }, content: [{ text: "Waltr is ready", type: "text" }], resultType: "complete", structuredContent: { observedAtUnixMs: 1791036279151, ready: true } };
    const wire = { ...view, toolJson: JSON.stringify(tool), argumentsJson: "{}", resultJson: JSON.stringify(result) };
    const payload = appPayload(wire);
    expect(payload.tool).toEqual({ _meta: tool._meta, inputSchema: tool.inputSchema, name: "status" });
    expect(payload.arguments).toEqual({});
    expect(payload.result).toEqual(result);
    expect(JSON.parse(wire.toolJson)).toEqual(tool);
    const extended = { ...result, futureField: { opaque: [1, { untouched: true }] }, _meta: { ...result._meta, privateExtension: { opaque: ["kept", null] } }, structuredContent: { ...result.structuredContent, futureField: [null, { untouched: true }] } };
    expect(appPayload({ ...wire, resultJson: JSON.stringify(extended) }).result).toEqual(extended);
    const settings = (window as unknown as { happyDOM: { settings: { disableIframePageLoading: boolean } } }).happyDOM.settings;
    const previous = settings.disableIframePageLoading;
    settings.disableIframePageLoading = true;
    const create = document.createElement.bind(document);
    vi.spyOn(document, "createElement").mockImplementation(((name: string) => {
      const element = create(name);
      if (name === "iframe") element.setAttribute("srcdoc", "");
      return element;
    }) as typeof document.createElement);
    const post = vi.fn();
    const frameWindow = { postMessage: post } as unknown as Window;
    vi.spyOn(HTMLIFrameElement.prototype, "contentWindow", "get").mockReturnValue(frameWindow);
    const container = create("div");
    document.body.append(container);
    const host = new AppHost(container, { ...payload, html: wire.html, containerUrl: "https://container.example/container.html", theme: "dark", platform: "web", containerDimensions: { width: 480, height: 360 }, onCallTool: async () => ({ content: [] }) });
    const sdkApp = new App({ name: "Captured app fixture", version: "1" }, {}, { autoResize: false });
    const inputs: unknown[] = [], results: unknown[] = [];
    sdkApp.ontoolinput = params => { inputs.push(params.arguments); };
    sdkApp.ontoolresult = value => { results.push(value); };
    sdkApp.onteardown = async () => ({});
    const generation = new URLSearchParams(new URL(host.iframe.src).hash.slice(1)).get("generation");
    const send = (message: JSONRPCMessage) => window.dispatchEvent(new MessageEvent("message", { source: frameWindow, origin: "https://container.example", data: { generation, message } }));
    const transport: Transport = { start: async () => {}, close: async () => {}, send: async message => { send(message); } };
    post.mockImplementation((data: { message?: JSONRPCMessage }) => { if (data.message) queueMicrotask(() => transport.onmessage?.(data.message!)); });
    try {
      send({ jsonrpc: "2.0", method: "ui/notifications/sandbox-proxy-ready" });
      await sdkApp.connect(transport);
      await host.ready;
      await vi.waitFor(() => expect(results).toHaveLength(1));
      expect(inputs).toEqual([{}]);
      expect(results).toEqual([result]);
    } finally {
      await host.dispose();
      await sdkApp.close();
      container.remove();
      settings.disableIframePageLoading = previous;
    }
  });
  test("normalizes only absent Rust Option declaration fields", () => {
    const declaration = { name: "status", inputSchema: { type: "object" }, title: null, description: null, annotations: null, outputSchema: null, _meta: null, icons: [] };
    expect(appPayload({ ...view, toolJson: JSON.stringify(declaration) }).tool).toEqual({ name: "status", inputSchema: { type: "object" }, icons: [] });
    const populated = { ...declaration, title: "Status", description: "Checks readiness", annotations: { readOnlyHint: true }, outputSchema: { type: "object", properties: { ready: { type: "boolean" } } }, _meta: { ui: { resourceUri: "ui://fixtures/observation" }, extension: { kept: true } } };
    expect(appPayload({ ...view, toolJson: JSON.stringify(populated) }).tool).toEqual(populated);
  });
  test.each([
    { title: 1 }, { description: false }, { annotations: "invalid" }, { annotations: { readOnlyHint: "invalid" } },
    { outputSchema: { type: "array" } }, { _meta: [] }, { icons: null }, { execution: null },
    { name: null }, { inputSchema: null }, { inputSchema: { type: "array" } }, { inputSchema: { type: "object", properties: [] } },
  ])("rejects malformed non-null declaration fields %j", (fields) => {
    expect(() => appPayload({ ...view, toolJson: JSON.stringify({ name: "status", inputSchema: { type: "object" }, ...fields }) })).toThrow();
  });
  test("only offers versioned linked metadata and validates host payload", () => {
    expect(hasLinkedApp('{"mcp_app":{"version":1,"resource_uri":"ui://status"}}')).toBe(true);
    for (const input of ["bad", "{}", '{"mcp_app":{"version":2,"resource_uri":"ui://status"}}', '{"mcp_app":{"version":1,"resource_uri":"https://status"}}']) expect(hasLinkedApp(input)).toBe(false);
    expect(appPayload(view).tool.name).toBe("status");
    expect(() => appPayload({ ...view, toolJson: '{"name":"status"}' })).toThrow();
    expect(() => appPayload({ ...view, argumentsJson: "[]" })).toThrow();
    expect(() => appPayload({ ...view, resultJson: "{}" })).toThrow();
  });
  test("correlates open and refresh without replacing the recorded result", async () => {
    const s = setup();
    const open = s.bridge.open("session", "call");
    const command = s.commands[0] as Extract<ClientCommand, { type: "openSessionMcpApp" }>;
    expect(command).toMatchObject({ type: "openSessionMcpApp", sessionId: "session", callId: "call" });
    s.update(opened(s.state, command.requestId));
    expect(await open).toEqual(view);
    const call = s.bridge.call("lease", "status", { id: 1 }, new AbortController().signal);
    const refresh = s.commands[1] as Extract<ClientCommand, { type: "callSessionMcpAppTool" }>;
    expect(refresh.requestId).not.toBe(command.requestId);
    s.update({ ...s.state, apps: { ...s.state.apps, requests: { ...s.state.apps.requests, [refresh.requestId]: { kind: "call", status: "ready", appId: "lease", error: null, resultJson: '{"content":[{"type":"text","text":"new"}]}' } } } });
    expect(await call).toMatchObject({ content: [{ text: "new" }] });
    expect(s.state.apps.current.lease?.resultJson).toBe(view.resultJson);
    s.dispose();
    expect(s.commands.at(-1)).toEqual({ type: "closeSessionMcpApp", appId: "lease" });
  });
  test("pending open cancellation uses the same request ID on disposal", async () => {
    const s = setup();
    const pending = s.bridge.open("session", "call");
    const rejection = expect(pending).rejects.toThrow("cancelled");
    const command = s.commands[0] as Extract<ClientCommand, { type: "openSessionMcpApp" }>;
    s.dispose();
    await rejection;
    expect(s.commands.at(-1)).toEqual({ type: "cancelSessionMcpAppOpen", requestId: command.requestId });
    s.update(opened(s.state, command.requestId));
    await expect(s.bridge.open("session", "other")).rejects.toThrow("unavailable");
  });
  test("abort revokes the entire lease, rather than claiming individual cancellation", async () => {
    const s = setup();
    const opening = s.bridge.open("session", "call");
    s.update(opened(s.state, (s.commands[0] as Extract<ClientCommand, { type: "openSessionMcpApp" }>).requestId));
    await opening;
    const controller = new AbortController();
    const pending = s.bridge.call("lease", "status", {}, controller.signal);
    const rejection = expect(pending).rejects.toThrow("cancelled");
    controller.abort();
    await rejection;
    expect(s.commands.at(-1)).toEqual({ type: "closeSessionMcpApp", appId: "lease" });
    s.dispose();
  });
  test("open timeout is bounded and cancels the outstanding request", async () => {
    vi.useFakeTimers();
    const s = setup(20);
    const rejection = expect(s.bridge.open("session", "call")).rejects.toThrow("cancelled");
    await vi.advanceTimersByTimeAsync(21);
    await rejection;
    expect(s.commands.at(-1)?.type).toBe("cancelSessionMcpAppOpen");
    s.dispose();
  });
});
