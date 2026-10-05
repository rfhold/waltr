import { defineConfig } from "vitest/config";
import solid from "vite-plugin-solid";

export default defineConfig({
  plugins: [solid()],
  resolve: { conditions: ["browser", "development"] },
  define: {
    __WALTR_BUILD_PLATFORM__: JSON.stringify("tauri"),
    __WALTR_BUILD_VERSION__: JSON.stringify("test"),
    __WALTR_TELEMETRY_ENABLED__: "false",
  },
  test: {
    include: ["tests/mcp-apps/native-gate.test.ts", "tests/mcp-apps/dialog.test.tsx"],
    environment: "happy-dom",
  },
});
