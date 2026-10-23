import config from "../crates/waltr-web/playwright.config.mjs";
import { fileURLToPath } from "node:url";

export default {
  ...config,
  testDir: fileURLToPath(new URL("../crates/waltr-web/tests/mcp-apps/browser", import.meta.url)),
  outputDir: fileURLToPath(new URL("../.dev/app-container-playwright", import.meta.url)),
  webServer: config.webServer.map((server) => ({
    ...server,
    command: server.command.startsWith("cargo run ")
      ? server.command.replace("cargo run ", "cargo run --locked --offline ")
      : server.command,
  })),
  use: {
    ...config.use,
    browserName: "chromium",
    launchOptions: {
      ...config.use.launchOptions,
      executablePath: process.env.WALTR_APPS_BROWSER_EXECUTABLE,
      chromiumSandbox: true,
    },
  },
};
