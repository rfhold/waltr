import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["tests/mcp-apps/container.test.ts"],
    environment: "happy-dom",
  },
});
