import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

const proxyTarget = process.env.WALTR_WEB_PROXY_TARGET ?? "http://127.0.0.1:8080";

export default defineConfig({
  plugins: [solid()],
  server: {
    proxy: {
      "/waltr": {
        target: proxyTarget,
        changeOrigin: true,
      },
    },
  },
});
