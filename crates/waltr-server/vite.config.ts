import { defineConfig } from "vite";
import { viteSingleFile } from "vite-plugin-singlefile";

export default defineConfig({
  publicDir: false,
  plugins: [viteSingleFile({ removeViteModuleLoader: true })],
  build: {
    emptyOutDir: true,
  },
});
