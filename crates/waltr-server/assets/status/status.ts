import { App } from "@modelcontextprotocol/ext-apps";

const app = new App({ name: "Waltr Status", version: "0.2.1" }, { availableDisplayModes: ["inline"] }, { autoResize: false });
const status = document.getElementById("status")!;
const observed = document.getElementById("observed")!;
const error = document.getElementById("error")!;
const refresh = document.getElementById("refresh") as HTMLButtonElement;
let closed = false;

const render: NonNullable<typeof app.ontoolresult> = (result) => {
  if (closed) return;
  const data = result.structuredContent;
  if (!result.isError && data && typeof data.ready === "boolean"
    && typeof data.observedAtUnixMs === "number" && Number.isFinite(data.observedAtUnixMs)
    && Math.abs(data.observedAtUnixMs) <= 8.64e15) {
    status.textContent = data.ready ? "Ready" : "Not ready";
    observed.textContent = `Observed ${new Date(data.observedAtUnixMs).toISOString()}`;
    error.textContent = "";
  } else {
    error.textContent = "Status could not be refreshed. Previous observation is preserved.";
  }
};
app.ontoolresult = render;
app.onteardown = async () => {
  closed = true;
  refresh.disabled = true;
  refresh.onclick = null;
  return {};
};
refresh.onclick = async () => {
  refresh.disabled = true;
  try { render(await app.callServerTool({ name: "status", arguments: {} })); }
  catch { if (!closed) error.textContent = "Status refresh failed. Previous observation is preserved."; }
  finally { if (!closed) refresh.disabled = false; }
};
app.connect().then(() => {
  if (!closed) refresh.disabled = false;
}).catch(() => {
  if (!closed) error.textContent = "Status app could not connect to its host.";
});
