import { afterEach, expect, it, vi } from "vitest";
import { startAppContainer } from "../../src/mcp-apps/container/container";
import { renderStatus } from "./browser/status";

afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); document.body.replaceChildren(); });
function disableIframeLoading() {
  (window as unknown as { happyDOM: { settings: { disableIframePageLoading: boolean } } }).happyDOM.settings.disableIframePageLoading = true;
  const create = document.createElement.bind(document);
  vi.spyOn(document, "createElement").mockImplementation(((name: string) => {
    const element = create(name);
    if (name === "iframe") element.setAttribute("srcdoc", "");
    return element;
  }) as typeof document.createElement);
}
function pagehide(persisted: boolean) {
  const event = new Event("pagehide");
  Object.defineProperty(event, "persisted", { value: persisted });
  window.dispatchEvent(event);
}
it("test fixture preserves previous status on error and renders text without HTML interpretation", () => {
  const status = document.createElement("p"), observed = document.createElement("p"), error = document.createElement("p");
  renderStatus({ content: [], structuredContent: { ready: true, observedAtUnixMs: 1 } }, status, observed, error);
  const previous = observed.textContent;
  renderStatus({ isError: true, content: [{ type: "text", text: "<script>error</script>" }] }, status, observed, error);
  expect(status.textContent).toBe("Ready"); expect(observed.textContent).toBe(previous);
  expect(error.textContent).toBe("<script>error</script>"); expect(error.querySelector("script")).toBeNull();
});
it("publishes only host-authenticated HTML and blocks view sandbox spoofing", async () => {
  disableIframeLoading();
  vi.spyOn(HTMLIFrameElement.prototype, "contentWindow", "get").mockReturnValue({ postMessage: vi.fn() } as unknown as Window);
  location.hash = new URLSearchParams({ generation: "test-generation", hostOrigin: "https://waltr.example" }).toString();
  const fetchMock = vi.fn(async () => new Response(JSON.stringify({ path: `/views/${"a".repeat(32)}` }), { status: 201 }));
  vi.stubGlobal("fetch", fetchMock);
  const post = vi.spyOn(parent, "postMessage").mockImplementation(() => {});
  const cleanup = startAppContainer();
  expect(post).toHaveBeenCalledWith({ generation: "test-generation", message: { jsonrpc: "2.0", method: "ui/notifications/sandbox-proxy-ready", params: {} } }, "https://waltr.example");
  const send = (origin: string, source: MessageEventSource | null, message: unknown, generation = "test-generation") => {
    window.dispatchEvent(new MessageEvent("message", { origin, source, data: { generation, message } }));
  };
  const html = { jsonrpc: "2.0", method: "ui/notifications/sandbox-resource-ready", params: { html: "<script>untrusted()</script>", sandbox: "allow-same-origin", permissions: { camera: {} } } };
  send("https://evil.example", parent, html);
  send("https://waltr.example", null, html);
  send("https://waltr.example", parent, html, "stale");
  expect(fetchMock).not.toHaveBeenCalled();
  send("https://waltr.example", parent, html);
  await vi.waitFor(() => expect(document.querySelector("iframe")).not.toBeNull());
  const frame = document.querySelector("iframe")!;
  expect(frame.getAttribute("sandbox")).toBe("allow-scripts");
  expect(frame.getAttribute("allow")).toContain("camera 'none'");
  expect(frame.src).toContain(`/views/${"a".repeat(32)}`);
  expect(fetchMock.mock.calls[0]).toEqual(["/views", expect.objectContaining({ credentials: "omit", body: "<script>untrusted()</script>" })]);
  const count = post.mock.calls.length;
  window.dispatchEvent(new MessageEvent("message", { source: frame.contentWindow, origin: "null", data: { jsonrpc: "2.0", method: "ui/notifications/sandbox-proxy-ready" } }));
  expect(post.mock.calls).toHaveLength(count);
  window.dispatchEvent(new MessageEvent("message", { source: frame.contentWindow, origin: "https://evil.example", data: { jsonrpc: "2.0", id: 4, method: "tools/call" } }));
  expect(post.mock.calls).toHaveLength(count);
  window.dispatchEvent(new MessageEvent("message", { source: frame.contentWindow, origin: "null", data: { jsonrpc: "2.0", id: 4, method: "tools/call", params: { name: "status", arguments: {} } } }));
  expect(post.mock.calls).toHaveLength(count + 1);
  cleanup();
  expect(fetchMock).toHaveBeenLastCalledWith(`/views/${"a".repeat(32)}`, expect.objectContaining({ method: "DELETE", credentials: "omit" }));
});

it("preserves the published view and message bridge on persisted pagehide, then cleans up on final unload", async () => {
  disableIframeLoading();
  const viewPost = vi.fn();
  const viewWindow = { postMessage: viewPost } as unknown as Window;
  vi.spyOn(HTMLIFrameElement.prototype, "contentWindow", "get").mockReturnValue(viewWindow);
  location.hash = new URLSearchParams({ generation: "test-generation", hostOrigin: "https://waltr.example" }).toString();
  const viewPath = `/views/${"a".repeat(32)}`;
  const fetchMock = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => new Response(JSON.stringify({ path: viewPath }), { status: 201 }));
  vi.stubGlobal("fetch", fetchMock);
  const hostPost = vi.spyOn(parent, "postMessage").mockImplementation(() => {});
  const cleanup = startAppContainer();
  try {
    const sendHost = (message: unknown) => window.dispatchEvent(new MessageEvent("message", {
      origin: "https://waltr.example", source: parent, data: { generation: "test-generation", message },
    }));
    sendHost({ jsonrpc: "2.0", method: "ui/notifications/sandbox-resource-ready", params: { html: "<p>app</p>" } });
    await vi.waitFor(() => expect(document.querySelector("iframe")).not.toBeNull());
    const frame = document.querySelector("iframe")!;
    pagehide(true);
    expect(document.querySelector("iframe")).toBe(frame);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock.mock.calls[0][1]?.signal?.aborted).toBe(false);

    const response = { jsonrpc: "2.0", id: 4, result: {} };
    sendHost(response);
    expect(viewPost).toHaveBeenCalledWith(response, "*");
    const request = { jsonrpc: "2.0", id: 5, method: "tools/call", params: { name: "status", arguments: {} } };
    window.dispatchEvent(new MessageEvent("message", { source: viewWindow, origin: "null", data: request }));
    expect(hostPost).toHaveBeenLastCalledWith({ generation: "test-generation", message: request }, "https://waltr.example");

    pagehide(false);
    expect(document.querySelector("iframe")).toBeNull();
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(fetchMock).toHaveBeenLastCalledWith(viewPath, { method: "DELETE", credentials: "omit", keepalive: true });
    expect(fetchMock.mock.calls[0][1]?.signal?.aborted).toBe(true);
    pagehide(false);
    expect(fetchMock).toHaveBeenCalledTimes(2);
  } finally { cleanup(); }
});

it("removes both owned event listeners on explicit cleanup", () => {
  location.hash = new URLSearchParams({ generation: "test-generation", hostOrigin: "https://waltr.example" }).toString();
  vi.spyOn(parent, "postMessage").mockImplementation(() => {});
  const add = vi.spyOn(window, "addEventListener");
  const remove = vi.spyOn(window, "removeEventListener");
  const cleanup = startAppContainer();
  const ownedListeners = add.mock.calls.filter(([type]) => type === "message" || type === "pagehide");
  expect(ownedListeners.map(([type]) => type)).toEqual(["message", "pagehide"]);
  cleanup();
  for (const [type, listener] of ownedListeners) expect(remove).toHaveBeenCalledWith(type, listener);
  expect(remove).toHaveBeenCalledTimes(2);
  cleanup();
  pagehide(false);
  expect(remove).toHaveBeenCalledTimes(2);
});

it.each(["null", "tauri://remote", "tauri://localhost:123", "tauri://localhost/", "tauri://localhost?x=y", "tauri://localhost#fragment", "tauri://user@localhost", "tauri://user:secret@localhost", "data:text/html,app", "file://localhost"])("rejects noncanonical native host origin %s", hostOrigin => {
  location.hash = new URLSearchParams({ generation: "native-generation", hostOrigin }).toString();
  expect(() => startAppContainer()).toThrow();
});

it("accepts only the exact native parent origin, source and generation without accepting null serialization", async () => {
  disableIframeLoading();
  location.hash = new URLSearchParams({ generation: "native-generation", hostOrigin: "tauri://localhost" }).toString();
  const fetchMock = vi.fn(async () => new Response(JSON.stringify({ path: `/views/${"a".repeat(32)}` }), { status: 201 }));
  vi.stubGlobal("fetch", fetchMock);
  const post = vi.spyOn(parent, "postMessage").mockImplementation(() => {});
  const cleanup = startAppContainer();
  try {
    expect(new URL("tauri://localhost").origin).toBe("null");
    expect(post).toHaveBeenLastCalledWith(expect.objectContaining({ generation: "native-generation" }), "tauri://localhost");
    const send = (origin: string, source: MessageEventSource | null = parent, generation = "native-generation") => window.dispatchEvent(new MessageEvent("message", {
      origin, source, data: { generation, message: { jsonrpc: "2.0", method: "ui/notifications/sandbox-resource-ready", params: { html: "<p>native</p>" } } },
    }));
    send("null"); send("tauri://remote"); send("tauri://localhost", null); send("tauri://localhost", parent, "stale");
    expect(fetchMock).not.toHaveBeenCalled();
    send("tauri://localhost");
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
  } finally { cleanup(); }
});
