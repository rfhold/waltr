import { createServer } from "node:http";
import { createRequire } from "node:module";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { hostOrigin } from "./origins.mjs";

const web = fileURLToPath(new URL("../../../", import.meta.url));
const { build } = createRequire(`${web}/package.json`)("esbuild");
async function bundle(entry) {
  return (await build({
    entryPoints: [fileURLToPath(new URL(entry, import.meta.url))],
    nodePaths: [`${web}/node_modules`],
    bundle: true, write: false, format: "iife", platform: "browser", target: "es2022",
    define: { "import.meta.env.VITE_WALTR_MCP_APP_CONTAINER_URL": "undefined" },
  })).outputFiles[0].text;
}
const host = await bundle("host.ts");
const fixture = await bundle("status-entry.ts");
const probe = await readFile(new URL("probe.js", import.meta.url), "utf8");
const status = `<!doctype html><html><body><p id="status">Waiting</p><p id="observed"></p><button id="refresh" disabled>Refresh</button><p id="error"></p><script>${fixture.replaceAll("</script", "<\\/script")}</script></body></html>`;
const html = status.replace("<script>", `<script>${probe}</script><script>`);
const hits = [];
const server = createServer((request, response) => {
  if (request.url === "/collector") {
    if (request.method === "DELETE") hits.length = 0;
    response.setHeader("content-type", "application/json");
    response.end(JSON.stringify(hits));
  } else if (request.url === "/fixtures") {
    response.setHeader("content-type", "application/json");
    response.end(JSON.stringify({ status, html }));
  } else if (request.url === "/host.js") {
    response.setHeader("content-type", "text/javascript");
    response.end(host);
  } else {
    response.setHeader("content-type", "text/html");
    if (request.url.startsWith("/collect/")) {
      hits.push({ url: request.url, cookie: request.headers.cookie ?? null });
      response.end('<script>window.privilegedKey="mock-host-only-key";window.hostDocumentLoaded=true;</script>');
    } else response.end('<!doctype html><div id="app"></div><script src="/host.js"></script>');
  }
});
server.listen(Number(new URL(hostOrigin).port), "127.0.0.1");
for (const signal of ["SIGINT", "SIGTERM"]) process.once(signal, () => server.close());
