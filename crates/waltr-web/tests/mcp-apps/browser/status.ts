import { App } from "@modelcontextprotocol/ext-apps";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";

export function renderStatus(result: CallToolResult, status: HTMLElement, observed: HTMLElement, error: HTMLElement) {
  const data = result.structuredContent;
  if (!result.isError && data && typeof data.ready === "boolean"
    && typeof data.observedAtUnixMs === "number" && Number.isFinite(data.observedAtUnixMs)
    && Math.abs(data.observedAtUnixMs) <= 8.64e15) {
    status.textContent = data.ready ? "Ready" : "Not ready";
    observed.textContent = `Observed ${new Date(data.observedAtUnixMs).toISOString()}`;
    error.textContent = "";
  } else {
    error.textContent = result.content.filter(item => item.type === "text")
      .map(item => item.text).join("\n").slice(0, 4096) || "Status could not be refreshed.";
  }
}

export async function startStatusApp() {
  const app = new App({ name: "Waltr Status", version: "0.1.0" }, { availableDisplayModes: ["inline"] }, { autoResize: false });
  const status = document.getElementById("status")!;
  const observed = document.getElementById("observed")!;
  const error = document.getElementById("error")!;
  const refresh = document.getElementById("refresh") as HTMLButtonElement;
  let closed = false;
  let toolName = "status";
  app.ontoolinput = () => { toolName = app.getHostContext()?.toolInfo?.tool.name ?? toolName; };
  const render = (result: CallToolResult) => renderStatus(result, status, observed, error);
  app.ontoolresult = render;
  app.onteardown = async () => { closed = true; refresh.disabled = true; refresh.onclick = null; return {}; };
  refresh.onclick = async () => {
    refresh.disabled = true;
    try { render(await app.callServerTool({ name: toolName, arguments: {} })); }
    catch { error.textContent = "Status refresh failed. Previous observation is preserved."; }
    finally { if (!closed) refresh.disabled = false; }
  };
  await app.connect();
  if (!closed) refresh.disabled = false;
}
