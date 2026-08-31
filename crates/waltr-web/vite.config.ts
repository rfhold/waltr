import { defineConfig } from "vite";
import solid from "vite-plugin-solid";
import { fileURLToPath } from "node:url";

const proxyTarget = process.env.WALTR_WEB_PROXY_TARGET ?? "http://127.0.0.1:8080";

export default defineConfig(({ mode }) => ({
  appType: "spa",
  base: mode === "tauri" ? "./" : "/",
  plugins: [
    solid(),
    ...(mode === "tauri" ? [{
      name: "waltr-packaged-root-base",
      transformIndexHtml: {
        order: "pre" as const,
        handler: () => [{ tag: "base", attrs: { href: "/" }, injectTo: "head-prepend" as const }],
      },
    }] : []),
  ],
  resolve: {
    alias: {
      "virtual:waltr-platform": fileURLToPath(new URL(
        mode === "tauri" ? "./src/platform/tauri.ts" : "./src/wasm/client.ts",
        import.meta.url,
      )),
    },
  },
  server: {
    proxy: {
      "/waltr": {
        target: proxyTarget,
        changeOrigin: true,
      },
    },
  },
}));
