import { configDefaults, defineConfig } from "vitest/config";
import solid from "vite-plugin-solid";
import { fileURLToPath } from "node:url";
import { readFileSync } from "node:fs";

import { telemetryEnabledForBuild } from "./src/build";

const containerEntry = fileURLToPath(new URL("./app-container/container.html", import.meta.url));
const bridgeContract = fileURLToPath(new URL("./src/mcp-apps/bridge-contract.ts", import.meta.url));
const isolatedBridge = `${bridgeContract}?waltr-container-isolated`;

const proxyTarget = process.env.WALTR_WEB_PROXY_TARGET ?? "http://127.0.0.1:8080";
const packageVersion = (JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")) as { version: string }).version;

export default defineConfig(({ command, mode }) => {
  const isTauri = mode === "tauri" || mode === "tauri-dev";
  const containerOnly = mode === "app-container";
  const telemetryEnabled = telemetryEnabledForBuild(command, mode);

  return {
    test: {
      exclude: [...configDefaults.exclude, "tests/mcp-apps/browser/**"],
    },
    appType: "spa",
    base: isTauri ? "./" : "/",
    publicDir: containerOnly ? false : "public",
    plugins: [
      solid(),
      ...(!isTauri ? [{
        name: "waltr-container-isolation",
        enforce: "pre" as const,
        resolveId(source: string, importer?: string) {
          // Duplicate the shared contract in the container graph, not a UI-owned chunk.
          if (source === "../bridge-contract" && importer?.endsWith("/mcp-apps/container/container.ts")) return isolatedBridge;
        },
        load(id: string) {
          if (id === isolatedBridge) {
            this.addWatchFile(bridgeContract);
            return readFileSync(bridgeContract, "utf8");
          }
        },
        generateBundle: { order: "post" as const, handler(_options, bundle) {
          const entry = bundle["app-container/container.js"];
          if (!entry || entry.type !== "chunk" || entry.imports.length || entry.dynamicImports.length || entry.referencedFiles.length) {
            this.error("App container must be a self-contained app-container/container.js with no shared assets");
          }
          const html = bundle["app-container/container.html"];
          if (!html || html.type !== "asset") this.error("App container HTML is missing");
          html.source = String(html.source).replaceAll("/app-container/container.js", "./container.js");
          const references = [...String(html.source).matchAll(/(?:src|href)="([^"]+)"/g)].map(match => match[1]);
          if (references.length !== 1 || references[0] !== "./container.js") {
            this.error("App container HTML must reference only its fixed container.js");
          }
          const files = Object.keys(bundle).filter(name => name.startsWith("app-container/"));
          if (files.length !== 2) this.error("App container must emit only container.html and container.js");
          if (containerOnly) {
            if (Object.keys(bundle).length !== 2) this.error("Container-only build must not emit UI or shared assets");
            for (const name of files) {
              const asset = bundle[name];
              delete bundle[name];
              asset.fileName = name.slice("app-container/".length);
              bundle[asset.fileName] = asset;
            }
          }
        } },
      } satisfies import("vite").Plugin] : []),
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
    build: {
      outDir: containerOnly ? "dist/app-container" : "dist",
      // A preload polyfill shared by HTML entries would escape the container boundary.
      modulePreload: isTauri ? undefined : { polyfill: false },
      rollupOptions: {
        input: isTauri ? "index.html" : containerOnly ? containerEntry : {
          main: fileURLToPath(new URL("./index.html", import.meta.url)),
          container: containerEntry,
        },
        output: {
          entryFileNames: chunk => chunk.facadeModuleId === containerEntry
            ? "app-container/container.js" : "assets/[name]-[hash].js",
        },
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
  };
});
