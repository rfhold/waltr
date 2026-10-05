import { createEffect, onCleanup, type Accessor } from "solid-js";
import { CallToolResultSchema, ToolSchema } from "@modelcontextprotocol/sdk/types.js";
import type { ClientCommand, ClientSnapshot, McpAppOperation, McpAppView } from "../generated/waltr-client";

function resultPayload(json: string) {
  const value = JSON.parse(json);
  if (!Array.isArray(value?.content)) throw new Error("Invalid app result");
  return CallToolResultSchema.parse(value);
}

export function hasLinkedApp(metadataJson: string): boolean {
  try {
    const app = JSON.parse(metadataJson).mcp_app;
    return app?.version === 1 && typeof app.resource_uri === "string" && app.resource_uri.startsWith("ui://");
  } catch { return false; }
}

export function appPayload(view: McpAppView) {
  const declaration: unknown = JSON.parse(view.toolJson);
  if (declaration && typeof declaration === "object" && !Array.isArray(declaration)) {
    const fields = declaration as Record<string, unknown>;
    // Rust McpToolDefinition Option fields serialize absent declarations as null.
    for (const key of ["title", "description", "outputSchema", "annotations", "_meta"]) {
      if (fields[key] === null) delete fields[key];
    }
  }
  const tool = ToolSchema.parse(declaration);
  const args: unknown = JSON.parse(view.argumentsJson);
  if (!args || typeof args !== "object" || Array.isArray(args)) throw new Error("Invalid app arguments");
  return { tool, arguments: args as Record<string, unknown>, result: resultPayload(view.resultJson) };
}

/** All authority stays in the shared client; this only correlates UI promises. */
export function createAppCommands(snapshot: Accessor<ClientSnapshot | undefined>, dispatch: (command: ClientCommand) => Promise<ClientSnapshot>, timeoutMs = 30_000) {
  const pending = new Map<string, { kind: "open" | "call"; complete: (operation: McpAppOperation, state: ClientSnapshot) => void; cancel: () => void }>();
  const leases = new Set<string>();
  let disposed = false;
  const send = (command: ClientCommand) => { void dispatch(command).catch(() => {}); };
  const inspect = (state: ClientSnapshot | undefined) => {
    if (!state || disposed) return;
    for (const [id, waiter] of pending) {
      const operation = state.apps?.requests[id];
      if (operation && operation.kind === waiter.kind && operation.status !== "loading") waiter.complete(operation, state);
    }
  };
  createEffect(() => inspect(snapshot()));
  const close = (appId: string) => { leases.delete(appId); send({ type: "closeSessionMcpApp", appId }); };
  const dispose = () => {
    if (disposed) return;
    disposed = true;
    for (const waiter of [...pending.values()]) waiter.cancel();
    for (const id of [...leases]) close(id);
  };
  onCleanup(dispose);
  function request<T>(kind: "open" | "call", command: ClientCommand, id: string, decode: (operation: McpAppOperation, state: ClientSnapshot) => T, signal?: AbortSignal): Promise<T> {
    if (disposed || signal?.aborted) return Promise.reject(new Error("App unavailable"));
    return new Promise((resolve, reject) => {
      const cleanup = () => { pending.delete(id); clearTimeout(timer); signal?.removeEventListener("abort", cancel); };
      const cancel = () => {
        cleanup();
        if (kind === "open") send({ type: "cancelSessionMcpAppOpen", requestId: id });
        else dispose(); // No individual backend cancellation exists: revoke the lease.
        reject(new Error("App request cancelled"));
      };
      const timer = setTimeout(cancel, timeoutMs);
      pending.set(id, { kind, cancel, complete: (operation, state) => {
        cleanup();
        if (operation.status !== "ready") { reject(new Error("App request failed")); return; }
        try { resolve(decode(operation, state)); } catch { reject(new Error("App response unavailable")); }
      } });
      signal?.addEventListener("abort", cancel, { once: true });
      void dispatch(command).then(inspect).catch(() => { if (pending.has(id)) cancel(); });
    });
  }
  return {
    dispose,
    open(sessionId: string, callId: string) {
      const requestId = crypto.randomUUID();
      return request("open", { type: "openSessionMcpApp", sessionId, callId, requestId }, requestId, (operation, state) => {
        if (!operation.appId) throw new Error("Missing lease");
        leases.add(operation.appId);
        const view = state.apps.current[operation.appId];
        if (!view || view.appId !== operation.appId) { close(operation.appId); throw new Error("Missing app"); }
        return view;
      });
    },
    call(appId: string, name: string, args: Record<string, unknown>, signal: AbortSignal) {
      if (!leases.has(appId)) return Promise.reject(new Error("App unavailable"));
      const requestId = crypto.randomUUID();
      return request("call", { type: "callSessionMcpAppTool", appId, name, argumentsJson: JSON.stringify(args), requestId }, requestId, operation => {
        if (operation.appId !== appId || !operation.resultJson) throw new Error("Missing result");
        return resultPayload(operation.resultJson);
      }, signal);
    },
  };
}
