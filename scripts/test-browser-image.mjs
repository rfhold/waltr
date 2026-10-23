import assert from "node:assert/strict";
import { accessSync, constants, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
export const browserImageVersion = "bw-pw1.63.0-r1.96.0-n24.18.0-p11.5.0-v1-amd64";

export function validateBrowserImage(contract, workspace, installed, browsers) {
  assert.equal(contract.schema, 1, "Unsupported browser image contract");
  assert.equal(contract.imageVersion, browserImageVersion, "Browser image version mismatch");
  assert.equal(contract.platform, "linux/amd64", "Only linux/amd64 is verified by this image contract");
  assert.equal(workspace.devDependencies.playwright, "1.63.0", "Workspace Playwright pin changed; rebuild the browser image contract");
  assert.equal(contract.playwright, workspace.devDependencies.playwright, "Image/workspace Playwright version mismatch");
  assert.equal(installed, contract.playwright, "Installed repo-local Playwright version mismatch; use pnpm install --frozen-lockfile");
  const chromium = browsers.browsers.find((browser) => browser.name === "chromium");
  assert.equal(contract.chromiumRevision, chromium?.revision, "Preinstalled Chromium revision mismatch");
  assert.equal(contract.chromiumVersion, chromium?.browserVersion, "Preinstalled Chromium version mismatch");
  assert.equal(contract.node, "24.18.0");
  assert.equal(contract.rust, "1.96.0");
  assert.equal(contract.pnpm, "11.5.0");
  assert.equal(workspace.packageManager, `pnpm@${contract.pnpm}`);
  assert.match(contract.executablePath, /^\/ms-playwright\/chromium-[0-9]+\//, "Expected image-managed Chromium executable");
  assert.ok(contract.executablePath.startsWith(`/ms-playwright/chromium-${contract.chromiumRevision}/`), "Executable path does not match the pinned Chromium revision");
  return {
    PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD: "1",
    PLAYWRIGHT_BROWSERS_PATH: "/ms-playwright",
    WALTR_APPS_BROWSER: "chromium",
    WALTR_APPS_BROWSER_EXECUTABLE: contract.executablePath,
    CARGO_NET_OFFLINE: "true",
  };
}

export function runBrowserImage(args = process.argv.slice(2)) {
  assert.ok(args.length === 0 || (args.length === 1 && args[0] === "--verify-only"), "Usage: node scripts/test-browser-image.mjs [--verify-only]");
  assert.equal(process.platform, "linux", "Run inside the dedicated linux/amd64 browser image");
  assert.equal(process.arch, "x64", "The browser image is amd64-only");
  assert.notEqual(process.getuid(), 0, "Use the image's non-root user; Chromium sandbox must remain enabled");
  const require = createRequire(join(root, "package.json"));
  const workspace = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
  const installed = require("playwright/package.json").version;
  const corePackage = require.resolve("playwright-core/package.json", { paths: [dirname(require.resolve("playwright/package.json"))] });
  assert.equal(JSON.parse(readFileSync(corePackage, "utf8")).version, installed, "Repo-local Playwright/core version mismatch");
  const browsers = JSON.parse(readFileSync(join(dirname(corePackage), "browsers.json"), "utf8"));
  const contract = JSON.parse(readFileSync("/opt/playwright/contract.json", "utf8"));
  const env = { ...process.env, ...validateBrowserImage(contract, workspace, installed, browsers) };
  assert.equal(process.version, `v${contract.node}`, "Node version mismatch");
  accessSync(contract.executablePath, constants.X_OK);
  accessSync(join(root, "crates/waltr-web/node_modules/vite"), constants.R_OK);
  for (const [command, commandArgs, expected] of [
    ["rustc", ["--version"], /^rustc 1\.96\.0 /],
    ["pnpm", ["--version"], /^11\.5\.0\s*$/],
  ]) {
    const result = spawnSync(command, commandArgs, { cwd: root, env, encoding: "utf8" });
    assert.equal(result.status, 0, `${command} is required`);
    assert.match(result.stdout, expected, `${command} version mismatch`);
  }
  const verify = spawnSync(process.execPath, ["/opt/playwright/verify.mjs"], { cwd: root, env, stdio: "inherit" });
  if (verify.status !== 0) return verify.status ?? 1;
  if (args[0] === "--verify-only") return 0;
  const result = spawnSync(process.execPath, [
    join(root, "node_modules/playwright/cli.js"), "test", "--config", "scripts/browser-image.config.mjs",
  ], { cwd: root, env, stdio: "inherit" });
  return result.status ?? 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    process.exitCode = runBrowserImage();
  } catch (error) {
    console.error(`Browser image prerequisite failed: ${error.message}\nBefore offline execution, install repository dependencies with PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 pnpm install --frozen-lockfile and prefetch locked Cargo dependencies for apps-host-assets. Provide a writable checkout/cache, non-root user, working Chromium user-namespace sandbox and adequate /dev/shm. This launcher never downloads browsers or packages.`);
    process.exitCode = 1;
  }
}
