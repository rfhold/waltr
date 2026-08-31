import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);
const read = (path) => readFile(new URL(path, root), "utf8");

test("Tauri and browser scripts include their build prerequisites", async () => {
  const pkg = JSON.parse(await read("package.json"));
  assert.match(pkg.scripts.build, /wasm:build/);
  assert.match(pkg.scripts["build:tauri"], /^pnpm wasm:build && /);
  assert.match(pkg.scripts["build:tauri"], /--mode tauri/);
  assert.match(pkg.scripts["build:tauri"], /tauri-output\.contract\.mjs/);
});

test("the selected loader has no eager WASM import", async () => {
  const app = await read("src/app/App.tsx");
  const vite = await read("vite.config.ts");
  assert.match(app, /from "virtual:waltr-platform"/);
  assert.match(vite, /mode === "tauri" \? "\.\/src\/platform\/tauri\.ts" : "\.\/src\/wasm\/client\.ts"/);
  assert.doesNotMatch(app, /wasm/);
});

test("packaged assets are relative and the app declares SPA fallback", async () => {
  const vite = await read("vite.config.ts");
  assert.match(vite, /appType: "spa"/);
  assert.match(vite, /mode === "tauri" \? "\.\/" : "\/"/);
  assert.match(vite, /waltr-packaged-root-base/);
  assert.match(vite, /attrs: \{ href: "\/" \}/);
  const routes = await read("src/routes/index.tsx");
  for (const route of ["/:sessionId", "/routines/:invocationId", "/provider/edit/:provider/models/edit/:model"]) {
    assert.ok(routes.includes(route), `missing deep route ${route}`);
  }
});
