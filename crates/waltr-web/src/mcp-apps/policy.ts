export { MAX_HTML_BYTES, MAX_MESSAGE_BYTES, DENIED_PERMISSIONS, boundedMessage } from "./bridge-contract";
export const EMPTY_SANDBOX_CAPABILITIES = {
  permissions: {},
  csp: { connectDomains: [], resourceDomains: [], frameDomains: [], baseUriDomains: [] },
};

let nativeContainer: string | undefined;

// Only the packaged adapter's authenticated native startup reply sets this module-local capability.
export function acceptNativeAppsBootstrap(value: unknown): void {
  if (value === undefined) {
    nativeContainer = undefined;
    return;
  }
  if (nativeContainer !== undefined) return;
  if (__WALTR_BUILD_PLATFORM__ !== "tauri" || window.top !== window
    || window.location.protocol !== "tauri:" || window.location.host !== "localhost") return;
  if (!value || typeof value !== "object" || !("containerUrl" in value) || typeof value.containerUrl !== "string") return;
  let url: URL;
  try { url = containerAddress(value.containerUrl, window.location.origin); } catch { return; }
  if (url.protocol !== "http:" || url.hostname !== "127.0.0.1" || !url.port || url.pathname !== "/container.html") return;
  nativeContainer = url.href;
}

export function appsAvailable(): boolean {
  return __WALTR_BUILD_PLATFORM__ === "browser" || nativeContainer !== undefined;
}

export function containerAddress(value: string | undefined, hostOrigin: string): URL {
  if (!value) throw new Error("MCP App container is not configured");
  const url = new URL(value);
  if (!['https:', 'http:'].includes(url.protocol) || url.origin === hostOrigin
    || url.username || url.password || url.search || url.hash
    || (url.protocol === 'http:' && !['localhost', '127.0.0.1', '[::1]'].includes(url.hostname))) {
    throw new Error("MCP Apps requires a separate trusted app container origin");
  }
  return url;
}

export function configuredContainerAddress(explicitUrl?: string, hostOrigin = window.location.origin): URL {
  if (explicitUrl !== undefined) return containerAddress(explicitUrl, hostOrigin);
  if (__WALTR_BUILD_PLATFORM__ === "tauri") return containerAddress(nativeContainer, hostOrigin);
  const markers = document.head.querySelectorAll<HTMLMetaElement>('meta[name="waltr-mcp-apps-container-url"]');
  if (markers.length > 1) throw new Error("Ambiguous MCP App container configuration");
  // A present runtime marker is authoritative, including malformed/empty values.
  const value = markers.length === 1 ? markers[0].getAttribute("content") ?? undefined : import.meta.env.VITE_WALTR_MCP_APP_CONTAINER_URL;
  return containerAddress(value, hostOrigin);
}
