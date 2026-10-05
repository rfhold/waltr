import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import { runInNewContext } from "node:vm";

{
  const dist = new URL("../dist/status/", import.meta.url);
  assert.deepEqual(await readdir(dist), ["index.html"], "Vite emits one self-contained status document");
  const html = await readFile(new URL("index.html", dist), "utf8");
  assert.doesNotMatch(html, /<script\b[^>]*\bsrc\s*=|<link\b[^>]*\bhref\s*=|STATUS_SCRIPT/);
  const scripts = [...html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/g)];
  assert.equal(scripts.length, 1);
  const script = scripts[0][1];
  assert.ok(script, "the server bundle contains an inline script");
  assert.ok(Buffer.byteLength(html) <= 1024 * 1024);
  const elements = Object.fromEntries(["status", "observed", "error", "refresh"].map(id => [id, { textContent: "", disabled: true }]));
  const listeners = new Set();
  const sent = [];
  let nextResult = { content: [{ type: "text", text: "Waltr is not ready" }], structuredContent: { ready: false, observedAtUnixMs: 2000 } };
  const parent = {
    postMessage(message) {
      sent.push(message);
      if (message.method === "ui/initialize") {
        assert.equal(message.params.protocolVersion, "2026-01-26");
        assert.equal(message.params.appInfo.name, "Waltr Status");
        queueMicrotask(() => receive({ jsonrpc: "2.0", id: message.id, result: {
          protocolVersion: "2026-01-26", hostInfo: { name: "fixture", version: "1" },
          hostCapabilities: { serverTools: {} }, hostContext: { theme: "light", displayMode: "inline" },
        } }));
      } else if (message.method === "tools/call") {
        assert.deepEqual(JSON.parse(JSON.stringify(message.params)), { name: "status", arguments: {} });
        queueMicrotask(() => receive({ jsonrpc: "2.0", id: message.id, result: nextResult }));
      }
    },
  };
  function receive(data) { for (const listener of listeners) listener({ data, source: parent, origin: "https://host.invalid" }); }
  const window = {
    parent,
    addEventListener(name, listener) { if (name === "message") listeners.add(listener); },
    removeEventListener(name, listener) { if (name === "message") listeners.delete(listener); },
  };
  runInNewContext(script, {
    window, document: { getElementById: id => elements[id] },
    console, setTimeout, clearTimeout, setInterval, clearInterval,
    URL, AbortController, AbortSignal, queueMicrotask,
  });
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.ok(sent.some(message => message.method === "ui/notifications/initialized"));
  assert.equal(elements.refresh.disabled, false);
  receive({ jsonrpc: "2.0", method: "ui/notifications/tool-result", params: {
    content: [{ type: "text", text: "Waltr is ready" }], structuredContent: { ready: true, observedAtUnixMs: 1000 },
  } });
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(elements.status.textContent, "Ready");
  assert.equal(elements.observed.textContent, "Observed 1970-01-01T00:00:01.000Z");
  await elements.refresh.onclick();
  assert.equal(elements.status.textContent, "Not ready");
  assert.equal(elements.observed.textContent, "Observed 1970-01-01T00:00:02.000Z");
  nextResult = { content: [], structuredContent: { ready: true, observedAtUnixMs: "invalid" } };
  await elements.refresh.onclick();
  assert.equal(elements.status.textContent, "Not ready");
  assert.equal(elements.observed.textContent, "Observed 1970-01-01T00:00:02.000Z");
  assert.match(elements.error.textContent, /Previous observation is preserved/);
  receive({ jsonrpc: "2.0", id: "teardown", method: "ui/resource-teardown", params: {} });
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(elements.refresh.disabled, true);
  assert.equal(elements.refresh.onclick, null);
  assert.ok(sent.some(message => message.id === "teardown" && message.result));
  console.log("PASS server-owned SDK status app: initialize, original result, refresh, invalid result, teardown");
}
