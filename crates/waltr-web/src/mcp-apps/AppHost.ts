import { AppBridge } from "@modelcontextprotocol/ext-apps/app-bridge";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import type { CallToolResult, JSONRPCMessage, Tool } from "@modelcontextprotocol/sdk/types.js";
import { boundedMessage, configuredContainerAddress, DENIED_PERMISSIONS, EMPTY_SANDBOX_CAPABILITIES, MAX_HTML_BYTES } from "./policy";

export interface AppHostOptions {
  containerUrl?: string;
  html: string;
  tool: Tool;
  arguments: Record<string, unknown>;
  result: CallToolResult;
  theme: "light" | "dark";
  platform: "web" | "desktop" | "mobile";
  containerDimensions: { width: number; height: number };
  /** Must authorize app visibility and bind calls to the recorded call's server. */
  onCallTool(name: string, args: Record<string, unknown>, signal: AbortSignal): Promise<CallToolResult>;
  timeoutMs?: number;
}

function packagedHostOrigin(): string {
  const page = new URL(window.location.href);
  if (page.protocol !== "tauri:") return window.location.origin;
  if (window.top !== window || page.hostname !== "localhost" || page.port
    || page.username || page.password || page.search || page.hash) throw new Error("Invalid packaged app host");
  // URL.origin serializes custom schemes as null; never use that as a trusted origin.
  return "tauri://localhost";
}

/** No MCP connection, credentials, history mutation, or model inference is owned here. */
export class AppHost {
  readonly iframe: HTMLIFrameElement;
  readonly ready: Promise<void>;
  private bridge: AppBridge;
  private generation = crypto.randomUUID();
  private state: "container" | "resource" | "handshake" | "initializing" | "initialized" | "closing" | "closed" = "container";
  private aborts = new Set<AbortController>();
  private listener: (event: MessageEvent) => void;
  private timer: ReturnType<typeof setTimeout>;
  private closing?: Promise<void>;
  private resolveReady!: () => void;
  private rejectReady!: (error: Error) => void;

  constructor(container: HTMLElement, options: AppHostOptions) {
    const hostOrigin = packagedHostOrigin();
    const url = configuredContainerAddress(options.containerUrl);
    if (new TextEncoder().encode(options.html).length > MAX_HTML_BYTES) throw new Error("App HTML exceeds limit");
    const { width, height } = options.containerDimensions;
    if (![width, height].every(n => Number.isFinite(n) && n > 0 && n <= 8192)) throw new Error("Invalid app dimensions");
    // Capture the recorded call once; refresh responses never replace it.
    const args = structuredClone(options.arguments);
    const result = structuredClone(options.result);
    this.ready = new Promise((resolve, reject) => { this.resolveReady = resolve; this.rejectReady = reject; });
    this.iframe = document.createElement("iframe");
    this.iframe.title = "MCP App";
    this.iframe.setAttribute("sandbox", "allow-scripts allow-same-origin");
    this.iframe.setAttribute("allow", DENIED_PERMISSIONS);
    this.iframe.referrerPolicy = "no-referrer";
    this.iframe.style.cssText = `border:0;width:${width}px;height:${height}px;max-width:100%`;
    url.hash = new URLSearchParams({ hostOrigin, generation: this.generation }).toString();
    this.iframe.src = url.href;
    const post = (message: unknown) => this.iframe.contentWindow?.postMessage({ generation: this.generation, message }, url.origin);
    const error = (id: unknown, code: number, message: string) => post({ jsonrpc: "2.0", id, error: { code, message } });
    let initializeId: unknown;
    const transport: Transport = {
      start: async () => {},
      close: async () => { window.removeEventListener("message", this.listener); transport.onclose?.(); },
      send: async message => {
        if (this.state === "closed") throw new Error("App closed");
        if (!boundedMessage(message)) throw new Error("App message exceeds limit");
        if (this.state === "handshake" && "id" in message && message.id === initializeId && "result" in message) {
          this.state = "initializing";
        }
        post(message);
      },
    };
    this.bridge = new AppBridge(null, { name: "Waltr", version: "0.3.0" }, {
      serverTools: {}, sandbox: EMPTY_SANDBOX_CAPABILITIES,
    }, { hostContext: {
      toolInfo: { tool: structuredClone(options.tool) }, theme: options.theme,
      platform: options.platform, containerDimensions: { width, height },
      displayMode: "inline", availableDisplayModes: ["inline"],
    } });
    this.bridge.onerror = () => { /* Raw SDK errors can contain app data; intentionally not logged. */ };
    this.bridge.onrequestdisplaymode = async () => ({ mode: "inline" });
    this.bridge.oncalltool = async (params, extra) => {
      if (this.state !== "initialized" || this.aborts.size >= 8) throw new Error("App tool call unavailable");
      const controller = new AbortController();
      this.aborts.add(controller);
      const abort = () => controller.abort();
      extra.signal.addEventListener("abort", abort, { once: true });
      let timeout: ReturnType<typeof setTimeout> | undefined;
      try {
        return await Promise.race([
          options.onCallTool(params.name, params.arguments ?? {}, controller.signal),
          new Promise<never>((_, reject) => {
            const fail = () => reject(new Error("App tool call cancelled"));
            controller.signal.addEventListener("abort", fail, { once: true });
            timeout = setTimeout(() => controller.abort(), 30_000);
          }),
        ]);
      } catch { throw new Error("App tool call failed"); }
      finally {
        clearTimeout(timeout);
        extra.signal.removeEventListener("abort", abort);
        this.aborts.delete(controller);
      }
    };
    this.bridge.oninitialized = () => {
      if (this.state !== "initializing") return;
      this.state = "initialized";
      void (async () => {
        await this.bridge.sendToolInput({ arguments: args });
        await this.bridge.sendToolResult(result);
        clearTimeout(this.timer);
        this.resolveReady();
      })().catch(() => { this.rejectReady(new Error("App initialization failed")); void this.dispose(); });
    };
    this.listener = event => {
      if (event.source !== this.iframe.contentWindow || event.origin !== url.origin
        || !boundedMessage(event.data) || event.data.generation !== this.generation
        || !boundedMessage(event.data.message)) return;
      const message = event.data.message;
      if (message.jsonrpc !== "2.0") return;
      const method = message.method;
      if (method === "ui/notifications/sandbox-proxy-ready" && this.state === "container") {
        this.state = "resource";
        post({ jsonrpc: "2.0", method: "ui/notifications/sandbox-resource-ready", params: { html: options.html } });
        return;
      }
      if (typeof method === "string" && method.startsWith("ui/notifications/sandbox-")) return;
      if (typeof method === "string") {
        if (method === "ui/initialize" && this.state === "resource" && "id" in message) {
          this.state = "handshake";
          initializeId = message.id;
        }
        else if (method === "ui/notifications/initialized" && this.state === "initializing") { /* SDK checks handshake. */ }
        else if (this.state !== "initialized") {
          if ("id" in message) error(message.id, -32000, "App not initialized");
          return;
        } else if (!["tools/call", "ping", "ui/request-display-mode", "ui/notifications/size-changed", "notifications/cancelled"].includes(method)) {
          if ("id" in message) error(message.id, -32601, "Unsupported app request");
          return;
        }
      }
      transport.onmessage?.(message as unknown as JSONRPCMessage);
    };
    window.addEventListener("message", this.listener);
    this.timer = setTimeout(() => {
      this.rejectReady(new Error("App initialization timed out")); void this.dispose();
    }, Math.min(30_000, Math.max(1, options.timeoutMs ?? 10_000)));
    void this.bridge.connect(transport).catch(() => { this.rejectReady(new Error("App connection failed")); void this.dispose(); });
    container.append(this.iframe);
  }

  dispose(): Promise<void> {
    return this.closing ??= this.close();
  }

  private async close(): Promise<void> {
    const initialized = this.state === "initialized";
    this.state = "closing";
    clearTimeout(this.timer);
    this.rejectReady(new Error("App closed"));
    this.aborts.forEach(controller => controller.abort());
    if (initialized) {
      let timer: ReturnType<typeof setTimeout> | undefined;
      await Promise.race([
        this.bridge.teardownResource({}).catch(() => {}),
        new Promise<void>(resolve => { timer = setTimeout(resolve, 1000); }),
      ]);
      clearTimeout(timer);
    }
    this.iframe.contentWindow?.postMessage({ generation: this.generation, cleanup: true }, new URL(this.iframe.src).origin);
    this.state = "closed";
    await this.bridge.close();
    window.removeEventListener("message", this.listener);
    this.iframe.remove();
  }
}
