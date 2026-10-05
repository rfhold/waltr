import { AppHost } from "../../../src/mcp-apps/AppHost";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";

const activeListeners = new Set<EventListenerOrEventListenerObject>();
const add = window.addEventListener.bind(window);
const remove = window.removeEventListener.bind(window);
window.addEventListener = ((type: string, listener: EventListenerOrEventListenerObject, options: any) => {
  if (type === "message") activeListeners.add(listener);
  add(type, listener, options);
}) as typeof window.addEventListener;
window.removeEventListener = ((type: string, listener: EventListenerOrEventListenerObject, options: any) => {
  if (type === "message") activeListeners.delete(listener);
  remove(type, listener, options);
}) as typeof window.removeEventListener;

Object.assign(window, {
  privilegedKey: "mock-host-only-key",
  calls: [], aborted: 0, pending: false,
  listenerCount: () => activeListeners.size,
  mount: async (containerUrl: string, html: string, toolName = "status") => {
    const state = window as any;
    state.calls = [];
    state.original = { content: [], structuredContent: { ready: true, observedAtUnixMs: 1000 } };
    state.host = new AppHost(document.getElementById("app")!, {
      containerUrl, html,
      tool: { name: toolName, inputSchema: { type: "object" }, _meta: { ui: { resourceUri: `ui://fixtures/${toolName}` } } },
      arguments: { original: true }, result: state.original,
      theme: "light", platform: "web", containerDimensions: { width: 640, height: 480 },
      onCallTool: async (name, args, signal) => {
        state.calls.push({ name, args });
        if (state.pending) return new Promise<CallToolResult>((_, reject) => signal.addEventListener("abort", () => {
          state.aborted++; reject(new Error("aborted"));
        }, { once: true }));
        return { content: [], structuredContent: { ready: false, observedAtUnixMs: 2000 } };
      },
    });
    await state.host.ready;
  },
});
