import { boundedMessage, DENIED_PERMISSIONS, MAX_HTML_BYTES } from "../bridge-contract";

/** Runs only on the separately hosted asset origin, never the Waltr origin. */
export function startAppContainer() {
  const fragment = new URLSearchParams(location.hash.slice(1));
  const generation = fragment.get("generation");
  const hostOrigin = fragment.get("hostOrigin");
  if (!generation || !hostOrigin || hostOrigin === location.origin) throw new Error("Invalid app container bootstrap");
  const parsed = new URL(hostOrigin);
  if (hostOrigin !== "tauri://localhost"
    && (parsed.origin !== hostOrigin || !["http:", "https:"].includes(parsed.protocol))) throw new Error("Invalid host origin");
  history.replaceState(null, "", location.pathname);
  const iframe = document.createElement("iframe");
  iframe.title = "Isolated MCP App";
  iframe.setAttribute("sandbox", "allow-scripts");
  iframe.setAttribute("allow", DENIED_PERMISSIONS);
  iframe.referrerPolicy = "no-referrer";
  iframe.style.cssText = "border:0;width:100%;height:100%;display:block";
  let viewPath: string | undefined;
  let received = false;
  let closed = false;
  const abort = new AbortController();
  const post = (message: unknown) => parent.postMessage({ generation, message }, hostOrigin);
  const cleanup = () => {
    if (closed) return;
    closed = true;
    abort.abort();
    window.removeEventListener("message", listener);
    window.removeEventListener("pagehide", unload);
    iframe.remove();
    if (viewPath) void fetch(viewPath, { method: "DELETE", credentials: "omit", keepalive: true }).catch(() => {});
  };
  const publish = async (html: string) => {
    const timeout = setTimeout(() => abort.abort(), 10_000);
    try {
      const response = await fetch("/views", {
        method: "POST", headers: { "Content-Type": "text/html; charset=utf-8" },
        body: html, credentials: "omit", redirect: "error", signal: abort.signal,
      });
      if (response.status !== 201 || Number(response.headers.get("Content-Length")) > 1024) throw new Error("View publication failed");
      const body = await response.text();
      if (body.length > 1024) throw new Error("View publication failed");
      const data = JSON.parse(body) as { path?: unknown };
      if (typeof data.path !== "string" || !/^\/views\/[A-Za-z0-9_-]{32,128}$/.test(data.path)) throw new Error("Invalid view path");
      viewPath = data.path;
      if (closed) {
        void fetch(viewPath, { method: "DELETE", credentials: "omit", keepalive: true }).catch(() => {});
        return;
      }
      iframe.src = viewPath;
      document.body.append(iframe);
    } catch {
      cleanup();
    } finally { clearTimeout(timeout); }
  };
  const listener = (event: MessageEvent) => {
    if (closed || !boundedMessage(event.data)) return;
    if (event.source === parent && event.origin === hostOrigin && event.data.generation === generation) {
      if (event.data.cleanup === true) { cleanup(); return; }
      const message = event.data.message;
      if (!boundedMessage(message)) return;
      if (message.method === "ui/notifications/sandbox-resource-ready" && !received) {
        const params = message.params;
        if (!boundedMessage(params) || typeof params.html !== "string"
          || new TextEncoder().encode(params.html).length > MAX_HTML_BYTES) { cleanup(); return; }
        received = true;
        void publish(params.html);
      } else if (typeof message.method !== "string" || !message.method.startsWith("ui/notifications/sandbox-")) {
        // The view is an opaque origin (sandbox lacks allow-same-origin).
        iframe.contentWindow?.postMessage(message, "*");
      }
    } else if (event.source === iframe.contentWindow && event.origin === "null" && viewPath) {
      if (typeof event.data.method === "string" && event.data.method.startsWith("ui/notifications/sandbox-")) return;
      post(event.data);
    }
  };
  const unload = (event: PageTransitionEvent) => {
    if (!event.persisted) cleanup();
  };
  window.addEventListener("message", listener);
  window.addEventListener("pagehide", unload);
  post({ jsonrpc: "2.0", method: "ui/notifications/sandbox-proxy-ready", params: {} });
  return cleanup;
}
