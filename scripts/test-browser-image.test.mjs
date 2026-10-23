import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { browserImageVersion, validateBrowserImage } from "./test-browser-image.mjs";

const workspace = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));
const chromium = { name: "chromium", revision: "12345", browserVersion: "123.0.0.0" };
const browsers = { browsers: [chromium] };
const contract = {
  schema: 1,
  imageVersion: browserImageVersion,
  platform: "linux/amd64",
  playwright: "1.63.0",
  chromiumRevision: chromium.revision,
  chromiumVersion: chromium.browserVersion,
  executablePath: "/ms-playwright/chromium-12345/chrome-linux/chrome",
  node: "24.18.0",
  rust: "1.96.0",
  pnpm: "11.5.0",
};

test("locked workspace aligns with the dedicated browser contract and forbids browser downloads", () => {
  assert.deepEqual(validateBrowserImage(contract, workspace, "1.63.0", browsers), {
    PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD: "1",
    PLAYWRIGHT_BROWSERS_PATH: "/ms-playwright",
    WALTR_APPS_BROWSER: "chromium",
    WALTR_APPS_BROWSER_EXECUTABLE: contract.executablePath,
    CARGO_NET_OFFLINE: "true",
  });
});

test("rejects version, revision, architecture and executable mismatches before running tests", () => {
  for (const changed of [
    { schema: 2 },
    { imageVersion: "different" },
    { platform: "linux/arm64" },
    { playwright: "1.62.0" },
    { chromiumRevision: "0" },
    { chromiumVersion: "0" },
    { node: "0" },
    { rust: "0" },
    { pnpm: "0" },
    { executablePath: "/usr/bin/chromium" },
  ]) {
    assert.throws(() => validateBrowserImage({ ...contract, ...changed }, workspace, "1.63.0", browsers));
  }
  assert.throws(() => validateBrowserImage(contract, workspace, "1.62.0", browsers), /repo-local/);
  assert.throws(() => validateBrowserImage(contract, {
    ...workspace, devDependencies: { playwright: "1.64.0" },
  }, "1.64.0", browsers), /Workspace Playwright pin changed/);
});

test("launcher uses only local packages, preserves host/container configuration and retains the sandbox", () => {
  const source = readFileSync(new URL("./test-browser-image.mjs", import.meta.url), "utf8");
  assert.match(source, /node_modules\/playwright\/cli\.js/);
  assert.match(source, /PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 pnpm install --frozen-lockfile/);
  assert.doesNotMatch(source, /npx|execSync|shell: true/);
  const config = readFileSync(new URL("./browser-image.config.mjs", import.meta.url), "utf8");
  assert.match(config, /import config from "\.\.\/crates\/waltr-web\/playwright\.config\.mjs"/);
  assert.match(config, /webServer: config\.webServer\.map/);
  assert.match(config, /cargo run --locked --offline/);
  assert.match(config, /chromiumSandbox: true/);
  assert.match(config, /WALTR_APPS_BROWSER_EXECUTABLE/);
});
