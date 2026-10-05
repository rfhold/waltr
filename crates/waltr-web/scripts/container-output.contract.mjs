import assert from "node:assert/strict";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { existsSync } from "node:fs";
import { build, resolveConfig } from "vite";

const root = fileURLToPath(new URL("../", import.meta.url));

test("container-only Vite build needs no WASM and emits only fixed standalone assets", async () => {
  const config = await resolveConfig({ root, mode: "app-container" }, "build");
  assert.equal(config.publicDir, "");
  const result = await build({ root, mode: "app-container", logLevel: "error", build: { write: false } });
  assert.ok(!Array.isArray(result));
  const files = result.output;
  assert.deepEqual(files.map(file => file.fileName).sort(), ["container.html", "container.js"]);
  const html = files.find(file => file.fileName === "container.html");
  assert.match(String(html.source), /src="\.\/container\.js"/);
  assert.deepEqual([...String(html.source).matchAll(/(?:src|href)="([^"]+)"/g)].map(match => match[1]), ["./container.js"]);
  const script = files.find(file => file.fileName === "container.js");
  assert.deepEqual(script.imports, []);
  assert.deepEqual(script.dynamicImports, []);
  assert.deepEqual(script.referencedFiles, []);
  assert.ok(Object.keys(script.modules).every(id => !id.includes("wasm") && !id.includes("AppHost")));
});

test("browser and Tauri entry selections preserve the container boundary", async () => {
  const browser = await resolveConfig({ root, mode: "production" }, "build");
  assert.equal(Object.keys(browser.build.rollupOptions.input).length, 2);
  for (const mode of ["tauri", "tauri-dev"]) {
    const native = await resolveConfig({ root, mode }, "build");
    assert.equal(native.build.rollupOptions.input, "index.html");
    assert.ok(!native.plugins.some(plugin => plugin.name === "waltr-container-isolation"));
  }
});

test("full browser output isolates both entries and packaged outputs omit the container", async (context) => {
  for (const mode of ["production", "tauri", "tauri-dev"]) {
    const skip = mode === "production" && !existsSync(new URL("../generated/wasm-current/waltr_client.js", import.meta.url))
      ? "Full browser verification requires the WASM client; build it first"
      : false;
    await context.test(mode, { skip }, async () => {
      const result = await build({ root, mode, logLevel: "silent", build: { write: false } });
      assert.ok(!Array.isArray(result));
      const files = result.output;
      const containerFiles = files.filter(file => file.fileName.startsWith("app-container/"));
      if (mode === "production") {
        assert.deepEqual(containerFiles.map(file => file.fileName).sort(), ["app-container/container.html", "app-container/container.js"]);
        const script = containerFiles.find(file => file.type === "chunk");
        assert.deepEqual(script.imports, []);
        assert.deepEqual(script.dynamicImports, []);
        assert.deepEqual(script.referencedFiles, []);
        for (const file of files.filter(file => file.type === "chunk" && file !== script)) {
          assert.ok([...file.imports, ...file.dynamicImports].every(path => !path.startsWith("app-container/")));
        }
      } else {
        assert.deepEqual(containerFiles, []);
        assert.ok(files.every(file => !file.fileName.includes("wasm")));
      }
    });
  }
});
