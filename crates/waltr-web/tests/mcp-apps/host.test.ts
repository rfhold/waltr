import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "@modelcontextprotocol/ext-apps";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import { AppHost, type AppHostOptions } from "../../src/mcp-apps/AppHost";
import { configuredContainerAddress, containerAddress } from "../../src/mcp-apps/policy";
import { startAppContainer } from "../../src/mcp-apps/container/container";

const hosts: AppHost[] = [];
beforeEach(() => {
  (window as unknown as { happyDOM: { settings: { disableIframePageLoading: boolean } } }).happyDOM.settings.disableIframePageLoading = true;
  const create = document.createElement.bind(document);
  vi.spyOn(document, "createElement").mockImplementation(((name: string) => {
    const element = create(name);
    // HappyDOM cannot execute the HTTP sandbox; suppress page loading entirely.
    if (name === "iframe") element.setAttribute("srcdoc", "");
    return element;
  }) as typeof document.createElement);
  vi.spyOn(HTMLIFrameElement.prototype, "contentWindow", "get").mockReturnValue({ postMessage: vi.fn() } as unknown as Window);
});
afterEach(async () => {
  await Promise.all(hosts.splice(0).map(host => host.dispose()));
  vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllEnvs(); vi.unstubAllGlobals();
  document.head.querySelectorAll('meta[name="waltr-mcp-apps-container-url"]').forEach(marker => marker.remove());
  document.body.replaceChildren();
});
function containerMarker(value: string) {
  const marker = document.createElement("meta");
  marker.name = "waltr-mcp-apps-container-url";
  marker.content = value;
  document.head.append(marker);
  return marker;
}
function setup(overrides: Partial<AppHostOptions> = {}) {
  const options: AppHostOptions = {
    containerUrl: "https://container.example/apps/container.html", html: "<!doctype html><p>app</p>",
    tool: { name: "status", inputSchema: { type: "object" } }, arguments: { original: true },
    result: { content: [{ type: "text", text: "original" }], structuredContent: { ready: true, observedAtUnixMs: 1 } },
    theme: "light", platform: "web", containerDimensions: { width: 480, height: 360 },
    onCallTool: vi.fn(async () => ({ content: [{ type: "text" as const, text: "refresh" }] })), ...overrides,
  };
  const host = new AppHost(document.body, options);
  hosts.push(host);
  void host.ready.catch(() => {});
  const generation = new URLSearchParams(new URL(host.iframe.src).hash.slice(1)).get("generation");
  const send = (message: unknown, origin = "https://container.example", source: MessageEventSource | null = host.iframe.contentWindow, token = generation) => {
    window.dispatchEvent(new MessageEvent("message", { source, origin, data: { generation: token, message } }));
  };
  const sent: Record<string, any>[] = [];
  const transport: Transport = {
    start: async () => {}, close: async () => {}, send: async message => { send(message); },
  };
  vi.spyOn(host.iframe.contentWindow!, "postMessage").mockImplementation((data: any) => {
    if (data.message) {
      sent.push(data.message);
      queueMicrotask(() => transport.onmessage?.(data.message));
    }
  });
  return { host, options, send, sent, transport };
}

describe("isolated app host", () => {
  it("bootstraps the container with exact packaged identity even when custom-scheme origin serializes null", () => {
    vi.spyOn(window.location, "href", "get").mockReturnValue("tauri://localhost/index.html");
    vi.spyOn(window.location, "origin", "get").mockReturnValue("null");
    const { host } = setup();
    const fragment = new URL(host.iframe.src).hash;
    expect(new URLSearchParams(fragment.slice(1)).get("hostOrigin")).toBe("tauri://localhost");
    vi.stubGlobal("location", { hash: fragment, origin: "https://container.example", pathname: "/apps/container.html" });
    vi.spyOn(history, "replaceState").mockImplementation(() => {});
    const post = vi.spyOn(parent, "postMessage").mockImplementation(() => {});
    const cleanup = startAppContainer();
    try { expect(post).toHaveBeenLastCalledWith(expect.objectContaining({ message: expect.objectContaining({ method: "ui/notifications/sandbox-proxy-ready" }) }), "tauri://localhost"); }
    finally { cleanup(); }
  });
  it.each(["tauri://remote/index.html", "tauri://localhost:123/index.html", "tauri://user@localhost/index.html", "tauri://user:secret@localhost/index.html", "tauri://localhost/index.html?x=y", "tauri://localhost/index.html#fragment"])("refuses to derive native identity from %s", href => {
    vi.spyOn(window.location, "href", "get").mockReturnValue(href);
    vi.spyOn(window.location, "origin", "get").mockReturnValue("null");
    expect(() => setup()).toThrow("Invalid packaged app host");
  });
  it("preserves the browser host origin in the container bootstrap", () => {
    const origin = window.location.origin;
    expect(new URLSearchParams(new URL(setup().host.iframe.src).hash.slice(1)).get("hostOrigin")).toBe(origin);
  });
  it("uses the current runtime marker without any build URL or fixed port", () => {
    vi.stubEnv("VITE_WALTR_MCP_APP_CONTAINER_URL", undefined);
    const marker = containerMarker("http://127.0.0.1:51237/container.html");
    const first = setup({ containerUrl: undefined }).host;
    expect(new URL(first.iframe.src).origin).toBe("http://127.0.0.1:51237");
    marker.content = "http://127.0.0.1:51238/container.html";
    const second = setup({ containerUrl: undefined }).host;
    expect(new URL(second.iframe.src).origin).toBe("http://127.0.0.1:51238");
    expect(new URL(first.iframe.src).origin).toBe("http://127.0.0.1:51237");
  });
  it("prefers runtime configuration over a stale build URL, while explicit callers remain validated", () => {
    vi.stubEnv("VITE_WALTR_MCP_APP_CONTAINER_URL", "https://stale.example/container.html");
    const marker = containerMarker("http://127.0.0.1:51237/container.html");
    expect(configuredContainerAddress().href).toBe(marker.content);
    marker.content = "invalid";
    expect(() => configuredContainerAddress()).toThrow();
    const { host } = setup({ containerUrl: "https://explicit.example/container.html" });
    expect(new URL(host.iframe.src).origin).toBe("https://explicit.example");
    expect(() => configuredContainerAddress("", "https://waltr.example")).toThrow();
    expect(() => configuredContainerAddress("https://waltr.example/container.html", "https://waltr.example")).toThrow();
  });
  it("keeps external build URL compatibility only when the runtime marker is absent", () => {
    vi.stubEnv("VITE_WALTR_MCP_APP_CONTAINER_URL", "https://external.example/container.html");
    expect(new URL(setup({ containerUrl: undefined }).host.iframe.src).origin).toBe("https://external.example");
    containerMarker("");
    expect(() => configuredContainerAddress()).toThrow();
  });
  it("rejects duplicate runtime markers and uses exact URL origins for loopback validation", () => {
    vi.stubEnv("VITE_WALTR_MCP_APP_CONTAINER_URL", "https://external.example/container.html");
    containerMarker("http://127.0.0.1:51237/container.html");
    expect(() => configuredContainerAddress(undefined, "http://127.0.0.1:51237")).toThrow();
    expect(configuredContainerAddress(undefined, "http://localhost:51237").origin).toBe("http://127.0.0.1:51237");
    expect(configuredContainerAddress(undefined, "http://127.0.0.1:51238").origin).toBe("http://127.0.0.1:51237");
    containerMarker("http://127.0.0.1:51237/container.html");
    expect(() => configuredContainerAddress()).toThrow("Ambiguous");
  });
  it("fails closed without a configured separate origin", () => {
    expect(() => containerAddress(undefined, "https://waltr.example")).toThrow();
    expect(() => containerAddress("https://waltr.example/apps", "https://waltr.example")).toThrow();
    expect(() => containerAddress("https://user:secret@container.example/", "https://waltr.example")).toThrow();
  });
  it("rejects wrong source, origin and generation; blocks preinitialized calls", async () => {
    const { send, sent, options } = setup();
    const ready = { jsonrpc: "2.0", method: "ui/notifications/sandbox-proxy-ready" };
    send(ready, "https://evil.example"); send(ready, undefined, window); send(ready, undefined, undefined, "stale");
    expect(sent).toEqual([]);
    send({ jsonrpc: "2.0", id: 12, method: "tools/call", params: { name: "status" } });
    expect(sent[0].error.code).toBe(-32000);
    expect(options.onCallTool).not.toHaveBeenCalled();
    send(ready);
    expect(sent[1].method).toBe("ui/notifications/sandbox-resource-ready");
    send(ready);
    expect(sent).toHaveLength(2);
  });
  it("interoperates with the independent App SDK and preserves the recorded call on refresh", async () => {
    const { host, send, sent, transport, options } = setup();
    const app = new App({ name: "Independent fixture", version: "1" }, {}, { autoResize: false });
    const inputs: unknown[] = [], results: unknown[] = [];
    app.ontoolinput = params => { inputs.push(params.arguments); };
    app.ontoolresult = result => { results.push(result); };
    app.onteardown = async () => ({});
    send({ jsonrpc: "2.0", method: "ui/notifications/sandbox-proxy-ready" });
    await app.connect(transport);
    await host.ready;
    await vi.waitFor(() => expect(results).toHaveLength(1));
    expect(inputs).toEqual([options.arguments]);
    expect(results).toEqual([options.result]);
    expect(app.getHostCapabilities()).toEqual({ serverTools: {}, sandbox: { permissions: {}, csp: { connectDomains: [], resourceDomains: [], frameDomains: [], baseUriDomains: [] } } });
    expect(await app.callServerTool({ name: "status", arguments: {} })).toEqual({ content: [{ type: "text", text: "refresh" }] });
    expect(inputs).toHaveLength(1); expect(results).toHaveLength(1);
    expect(options.result.content[0]).toEqual({ type: "text", text: "original" });
    const inputIndex = sent.findIndex(m => m.method === "ui/notifications/tool-input");
    expect(sent[inputIndex + 1].method).toBe("ui/notifications/tool-result");
    for (const method of ["resources/read", "prompts/get", "ui/message", "ui/update-model-context", "ui/open-link", "sampling/createMessage"]) {
      send({ jsonrpc: "2.0", id: `unsupported-${method}`, method, params: {} });
      expect(sent.at(-1)?.error.code).toBe(-32601);
    }
    send({ jsonrpc: "2.0", id: "display", method: "ui/request-display-mode", params: { mode: "fullscreen" } });
    await vi.waitFor(() => expect(sent.find(m => m.id === "display")?.result).toEqual({ mode: "inline" }));
    await host.dispose();
    expect(sent.some(m => m.method === "ui/resource-teardown")).toBe(true);
    expect(host.iframe.isConnected).toBe(false);
    await app.close();
  });
  it("bounds initialization timeout and cleans up listeners", async () => {
    vi.useFakeTimers();
    const { host, send, sent } = setup({ timeoutMs: 20 });
    const rejection = expect(host.ready).rejects.toThrow("timed out");
    await vi.advanceTimersByTimeAsync(20); await rejection;
    await host.dispose();
    expect(host.iframe.isConnected).toBe(false);
    send({ jsonrpc: "2.0", method: "ui/notifications/sandbox-proxy-ready" });
    expect(sent).toEqual([]);
  });
  it("aborts inflight tools and bounds nonresponsive teardown to one second", async () => {
    let signal: AbortSignal | undefined;
    const { host, send, transport } = setup({ onCallTool: async (_name, _args, s) => {
      signal = s;
      return new Promise(() => {});
    } });
    const app = new App({ name: "Nonresponsive fixture", version: "1" }, {}, { autoResize: false });
    app.onteardown = async () => new Promise(() => {});
    send({ jsonrpc: "2.0", method: "ui/notifications/sandbox-proxy-ready" });
    await app.connect(transport); await host.ready;
    const pending = app.callServerTool({ name: "status", arguments: {} }).catch(() => {});
    await vi.waitFor(() => expect(signal).toBeDefined());
    vi.useFakeTimers();
    const closing = host.dispose();
    expect(signal?.aborted).toBe(true);
    await vi.advanceTimersByTimeAsync(999);
    expect(host.iframe.isConnected).toBe(true);
    await vi.advanceTimersByTimeAsync(1); await closing;
    expect(host.iframe.isConnected).toBe(false);
    await app.close(); await pending;
  });
});
