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
  assert.match(pkg.scripts["build:tauri"], /--telemetry=enabled/);
  assert.match(pkg.scripts["build:tauri"], /tauri-output\.contract\.mjs/);
  assert.match(pkg.scripts["build:tauri:dev"], /^pnpm wasm:build && /);
  assert.match(pkg.scripts["build:tauri:dev"], /--mode tauri-dev/);
  assert.match(pkg.scripts["build:tauri:dev"], /--telemetry=disabled/);
});

test("the selected loader has no eager WASM import", async () => {
  const app = await read("src/app/App.tsx");
  const vite = await read("vite.config.ts");
  assert.match(app, /from "virtual:waltr-platform"/);
  assert.match(vite, /const isTauri = mode === "tauri" \|\| mode === "tauri-dev"/);
  assert.match(vite, /isTauri \? "\.\/src\/platform\/tauri\.ts" : "\.\/src\/wasm\/client\.ts"/);
  assert.doesNotMatch(app, /wasm/);
});

test("packaged assets are relative and the app declares SPA fallback", async () => {
  const vite = await read("vite.config.ts");
  assert.match(vite, /appType: "spa"/);
  assert.match(vite, /base: isTauri \? "\.\/" : "\/"/);
  assert.match(vite, /waltr-packaged-root-base/);
  assert.match(vite, /attrs: \{ href: "\/" \}/);
  const routes = await read("src/routes/index.tsx");
  for (const route of ["/:sessionId", "/routines/:invocationId", "/provider/edit/:provider/models/edit/:model"]) {
    assert.ok(routes.includes(route), `missing deep route ${route}`);
  }
});

test("telemetry is enabled only for release asset builds", async () => {
  const vite = await read("vite.config.ts");
  const telemetry = await read("src/telemetry.ts");
  const tauriConfig = JSON.parse(await read("../waltr-app/tauri.conf.json"));

  assert.match(vite, /telemetryEnabledForBuild\(command, mode\)/);
  assert.match(vite, /__WALTR_TELEMETRY_ENABLED__: JSON\.stringify\(telemetryEnabled\)/);
  assert.match(telemetry, /options\.enabled \?\? __WALTR_TELEMETRY_ENABLED__/);
  assert.doesNotMatch(telemetry, /import\.meta\.env\.PROD/);
  assert.match(tauriConfig.build.beforeBuildCommand, /build:tauri\b/);
  assert.match(tauriConfig.build.beforeDevCommand, /build:tauri:dev\b/);
});
