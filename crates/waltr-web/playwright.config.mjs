import { defineConfig } from "playwright/test";
import { hostOrigin, containerOrigin } from "./tests/mcp-apps/browser/origins.mjs";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../", import.meta.url));
export default defineConfig({
  testDir: "./tests/mcp-apps/browser",
  testMatch: "**/*.spec.mjs",
  outputDir: "../../.dev/app-container-playwright",
  workers: 1,
  timeout: 30_000,
  use: {
    browserName: process.env.WALTR_APPS_BROWSER ?? "chromium",
    launchOptions: { executablePath: process.env.WALTR_APPS_BROWSER_EXECUTABLE },
  },
  webServer: [
    {
      command: "node crates/waltr-web/tests/mcp-apps/browser/host-server.mjs",
      cwd: root,
      url: hostOrigin,
      reuseExistingServer: false,
      gracefulShutdown: { signal: "SIGTERM", timeout: 3000 },
    },
    {
      command: process.env.WALTR_APPS_TEST_BINARY
        ? `"${process.env.WALTR_APPS_TEST_BINARY}" app-container`
        : "cargo run -p waltr-server --no-default-features --features apps-host-assets -- app-container",
      cwd: root,
      url: `${containerOrigin}/container.html`,
      timeout: 300_000,
      reuseExistingServer: false,
      gracefulShutdown: { signal: "SIGTERM", timeout: 3000 },
      env: {
        WALTR_APP_CONTAINER_ADDR: new URL(containerOrigin).host,
        WALTR_APP_CONTAINER_ORIGIN: containerOrigin,
        WALTR_APP_CONTAINER_HOST_ORIGINS: hostOrigin,
      },
    },
  ],
});
