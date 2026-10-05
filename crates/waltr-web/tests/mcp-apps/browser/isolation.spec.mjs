import assert from "node:assert/strict";
import { test, expect } from "playwright/test";
import { hostOrigin, containerOrigin } from "./origins.mjs";

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function check(name, actual, expected = true) {
  await test.step(name, async () => expect(actual).toEqual(expected));
}

function harness(page, html) {
  const responses = [];
  page.on("response", response => { if (response.url().includes("/views/")) responses.push(response); });
  const mount = async (content = html) => {
    await page.goto(hostOrigin);
    await page.evaluate(({ containerOrigin, html }) => window.mount(`${containerOrigin}/container.html`, html), { containerOrigin, html: content });
    const view = page.frames().find(frame => frame.url().startsWith(`${containerOrigin}/views/`));
    assert.ok(view, "Published view frame missing");
    await view.waitForFunction(() => document.getElementById("status").textContent === "Ready");
    return view;
  };
  const dispose = async viewUrl => {
    await page.evaluate(() => window.host.dispose());
    await expect.poll(async () => (await fetch(viewUrl)).status).toBe(404);
    await check("dispose deletes uploaded view", (await fetch(viewUrl)).status, 404);
    await check("dispose removes host message listener", await page.evaluate(() => window.listenerCount()), 0);
    await check("dispose removes outer iframe", await page.locator("iframe").count(), 0);
  };
  return { mount, dispose, responses };
}

test.describe("web-owned Apps isolation and host bridge against the real container service", () => {
  let status, html;
  test.beforeAll(async () => {
    ({ status, html } = await (await fetch(`${hostOrigin}/fixtures`)).json());
  });
  test.beforeEach(async ({ context }) => {
    await fetch(`${hostOrigin}/collector`, { method: "DELETE" });
    await context.addCookies([{ name: "mock-host-session", value: "host-only", url: hostOrigin }]);
  });
  test.afterEach(async ({ page }) => {
    await page.evaluate(async () => { await window.host?.dispose(); });
  });
  test("SDK lifecycle and tool-independent refresh", async ({ page }) => {
    const { mount, dispose } = harness(page, html);
    const view = await mount(status);
    await check("test-only SDK fixture served byte-for-byte", await (await fetch(view.url())).text(), status);
    await view.locator("#refresh").click();
    await view.waitForFunction(() => document.getElementById("status").textContent === "Not ready");
    await check("test-only SDK fixture refresh", await page.evaluate(() => window.calls.length), 1);
    await dispose(view.url());
    for (const toolName of ["weather_observation", "inventory_summary"]) {
      await page.goto(hostOrigin);
      await page.evaluate(({ containerOrigin, html, toolName }) => window.mount(`${containerOrigin}/container.html`, html, toolName), { containerOrigin, html: status, toolName });
      const alternate = page.frames().find(frame => frame.url().startsWith(`${containerOrigin}/views/`));
      assert.ok(alternate);
      await alternate.waitForFunction(() => document.getElementById("status").textContent === "Ready");
      await alternate.locator("#refresh").click();
      await alternate.waitForFunction(() => document.getElementById("status").textContent === "Not ready");
      await check(`generic container refreshes ${toolName}`, await page.evaluate(() => window.calls.at(-1).name), toolName);
      await dispose(alternate.url());
    }
  });
  test("HTTP CSP, protocol gates, browser attacks and pending disposal", async ({ page, context }) => {
    const { mount, dispose, responses } = harness(page, html);
    const view = await mount();
    const viewUrl = view.url();
    await check("separate host/app container origins", hostOrigin !== containerOrigin);
    await check("SDK input delivered once", await view.evaluate(() => probe.input), 1);
    await check("SDK original result delivered once", await view.evaluate(() => probe.result), 1);
    await check("SDK recorded arguments delivered intact", await view.evaluate(() => probe.messages.find(message => message.method === "ui/notifications/tool-input").params.arguments), { original: true });
    await check("SDK original result payload delivered intact", await view.evaluate(() => probe.messages.find(message => message.method === "ui/notifications/tool-result").params.structuredContent), { ready: true, observedAtUnixMs: 1000 });
    await check("SDK negotiates separate Apps protocol", await view.evaluate(() => probe.messages.find(message => message.result?.protocolVersion)?.result.protocolVersion), "2026-01-26");
    await check("initial observed content", await view.locator("#observed").textContent(), "Observed 1970-01-01T00:00:01.000Z");
    const headers = await responses.find(response => response.url() === viewUrl).allHeaders();
    await check("HTTP CSP applied to view before script", headers["content-security-policy"].includes("sandbox allow-scripts") && headers["content-security-policy"].includes("connect-src 'none'"));
    await check("asset response exposes no auth cookies or CORS", [headers["set-cookie"], headers["access-control-allow-origin"]], [undefined, undefined]);
    await check("asset service does not expose MCP API", (await fetch(`${containerOrigin}/mcp`)).status, 404);
    await check("opaque view cannot access parent DOM", await view.evaluate(() => { try { return parent.document.body.innerHTML; } catch (error) { return error.name; } }), "SecurityError");
    await view.locator("#refresh").click();
    await view.waitForFunction(() => document.getElementById("status").textContent === "Not ready");
    await check("refresh invokes injected callback once", await page.evaluate(() => window.calls), [{ name: "status", args: {} }]);
    await check("refresh updates observation", await view.locator("#observed").textContent(), "Observed 1970-01-01T00:00:02.000Z");
    await check("refresh does not renotify input/result", await view.evaluate(() => [probe.input, probe.result]), [1, 1]);
    await check("recorded original result unchanged", await page.evaluate(() => window.original.structuredContent), { ready: true, observedAtUnixMs: 1000 });

    for (const [id, method] of [[901, "ui/open-link"], [902, "resources/read"], [903, "ui/message"], [904, "sampling/createMessage"]]) {
      await view.evaluate(({ id, method }) => rawRequest(id, method), { id, method });
      await view.waitForFunction(id => probe.messages.some(message => message.id === id && message.error), id);
      await check(`unsupported ${method} returns error`, await view.evaluate(id => probe.messages.find(message => message.id === id).error.code, id), -32601);
    }
    const container = view.parentFrame();
    // Genuine view-origin reserved spoof: must never reach host or replace resource.
    await view.evaluate(() => parent.postMessage({ jsonrpc: "2.0", method: "ui/notifications/sandbox-resource-ready", params: { html: "spoof" } }, "*"));
    const generation = await page.evaluate(() => window.host.generation);
    await view.evaluate(generation => top.postMessage({ generation, message: { jsonrpc: "2.0", id: 996, method: "tools/call", params: { name: "status", arguments: {} } } }, "*"), generation);
    // Browser-delivered wrong-source messages, and explicit wrong-origin/source gates.
    await page.evaluate(({ generation, containerOrigin }) => {
      const message = { jsonrpc: "2.0", id: 999, method: "tools/call", params: { name: "status", arguments: {} } };
      window.postMessage({ generation, message }, "*");
      window.dispatchEvent(new MessageEvent("message", { origin: containerOrigin, source: window, data: { generation, message } }));
      window.dispatchEvent(new MessageEvent("message", { origin: location.origin, source: window.host.iframe.contentWindow, data: { generation, message } }));
    }, { generation, containerOrigin });
    await container.evaluate(generation => parent.postMessage({ generation: `${generation}-wrong`, message: { jsonrpc: "2.0", id: 998, method: "tools/call", params: { name: "status", arguments: {} } } }, "*"), generation);
    await sleep(200);
    await check("spoofed source/origin/generation and reserved container messages ignored", await page.evaluate(() => window.calls.length), 1);
    await check("reserved spoof does not replace view", view.url(), viewUrl);
    await container.evaluate(({ generation, hostOrigin }) => {
      const message = { jsonrpc: "2.0", id: 997, method: "tools/call", params: { name: "status", arguments: {} } };
      window.dispatchEvent(new MessageEvent("message", { origin: hostOrigin, source: window, data: { generation, message } }));
      window.dispatchEvent(new MessageEvent("message", { origin: "https://wrong-origin.invalid", source: parent, data: { generation, message } }));
    }, { generation, hostOrigin });
    await sleep(100);
    await check("container rejects wrong parent source/origin", await page.evaluate(() => window.calls.length), 1);

    await view.evaluate(hostOrigin => attack(hostOrigin), hostOrigin);
    await sleep(300);
    const attackState = await view.evaluate(() => probe);
    await check("parent DOM denied", attackState.denials.parentDom, "SecurityError");
    await check("host key inaccessible", attackState.denials.topDom, "SecurityError");
    await check("popup denied", attackState.denials.popup, true);
    await check("popup creates no browser page", context.pages().length, 1);
    await check("top navigation denied", attackState.denials.topNavigation, "SecurityError");
    for (const directive of ["connect-src", "script-src-elem", "img-src", "frame-src"]) {
      await check(`browser reports ${directive} denial`, attackState.violations.some(violation => violation.directive === directive));
    }
    await check("connect/script/image/frame/form/popup/top collector receives zero hits", (await (await fetch(`${hostOrigin}/collector`)).json()), []);
    await check("top document remains host", page.url(), `${hostOrigin}/`);
    await page.evaluate(() => { window.pending = true; });
    await view.locator("#refresh").click();
    await expect.poll(() => page.evaluate(() => window.calls.length)).toBe(2);
    await dispose(viewUrl);
    await check("pending callback aborted on dispose", await page.evaluate(() => window.aborted), 1);
  });
  test("navigation remains opaque and cannot reach the key-bearing host", async ({ page }) => {
    const { mount, dispose } = harness(page, html);
    for (const mode of ["host", "about:blank", "blob", "srcdoc"]) {
      const view = await mount();
      const originalUrl = view.url();
      await view.evaluate(({ mode, hostOrigin }) => {
        if (mode === "host") location.href = `${hostOrigin}/collect/navigation-host`;
        if (mode === "about:blank") location.href = "about:blank";
        if (mode === "blob") location.href = URL.createObjectURL(new Blob(['<script>window.escapeDocument=true</script>'], { type: "text/html" }));
        if (mode === "srcdoc") {
          const child = document.createElement("iframe"); child.srcdoc = '<script>window.escapeDocument=true</script>'; document.body.append(child);
        }
      }, { mode, hostOrigin });
      await sleep(300);
      const confinement = await view.evaluate(() => {
        let parentDenied = false, hostDenied = false;
        try { void parent.document; } catch { parentDenied = true; }
        try { void top.privilegedKey; } catch { hostDenied = true; }
        return { parentDenied, hostDenied, escape: window.escapeDocument === true, origin: location.origin, url: location.href };
      }).catch(error => ({ evaluationError: error.message, url: view.url() }));
      console.log(`NAVIGATION ${mode} ${JSON.stringify(confinement)}`);
      await check(`${mode} navigation retains opaque confinement`, confinement.parentDenied === true && confinement.hostDenied === true);
      await check(`${mode} does not create privileged host document`, !view.url().startsWith(hostOrigin));
      if (mode === "srcdoc") {
        await check("srcdoc child cannot acquire host origin", await view.evaluate(() => {
          try { return document.querySelector("iframe").contentWindow.top.privilegedKey === undefined; } catch { return true; }
        }));
        const child = view.childFrames()[0];
        assert.ok(child, "srcdoc child missing");
        await check("srcdoc child retains host DOM denial", await child.evaluate(() => {
          try { void top.document; return false; } catch (error) { return error.name === "SecurityError"; }
        }));
      }
      await dispose(originalUrl);
    }
    await check("navigation attempts never reach mock key-bearing host collector", (await (await fetch(`${hostOrigin}/collector`)).json()), []);
  });
});
