import { defineConfig } from "vite";
import solid from "vite-plugin-solid";
import { fileURLToPath } from "node:url";
import { readFileSync } from "node:fs";

import { telemetryEnabledForBuild } from "./src/build";

const proxyTarget = process.env.WALTR_WEB_PROXY_TARGET ?? "http://127.0.0.1:8080";
const packageVersion = (JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")) as { version: string }).version;

export default defineConfig(({ command, mode }) => {
  const isTauri = mode === "tauri" || mode === "tauri-dev";
  const telemetryEnabled = telemetryEnabledForBuild(command, mode);

  return {
    appType: "spa",
    base: isTauri ? "./" : "/",
    plugins: [
      solid(),
      ...(isTauri ? [{
        name: "waltr-packaged-root-base",
        transformIndexHtml: {
          order: "pre" as const,
          handler: () => [
            { tag: "base", attrs: { href: "/" }, injectTo: "head-prepend" as const },
            { tag: "meta", attrs: { name: "waltr-telemetry", content: telemetryEnabled ? "enabled" : "disabled" }, injectTo: "head" as const },
          ],
        },
      }] : []),
    ],
    resolve: {
      alias: {
        "virtual:waltr-platform": fileURLToPath(new URL(
          isTauri ? "./src/platform/tauri.ts" : "./src/wasm/client.ts",
          import.meta.url,
        )),
      },
    },
    define: {
      __WALTR_BUILD_PLATFORM__: JSON.stringify(isTauri ? "tauri" : "browser"),
      __WALTR_BUILD_VERSION__: JSON.stringify(packageVersion),
      __WALTR_TELEMETRY_ENABLED__: JSON.stringify(telemetryEnabled),
    },
    server: {
      proxy: {
        "/waltr": {
          target: proxyTarget,
          changeOrigin: true,
        },
      },
    },
  };
});
