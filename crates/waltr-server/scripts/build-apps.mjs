import { readFile, readdir, stat } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "vite";

export const serverRoot = fileURLToPath(new URL("../", import.meta.url));
export const entries = (await readFile(new URL("apps.entries", import.meta.url), "utf8"))
  .trim().split("\n").map(line => {
    const fields = line.trim().split(/\s+/);
    if (fields.length !== 2 || !/^[a-z][a-z0-9_]*$/.test(fields[0])) {
      throw new Error(`Invalid static app entry: ${line}`);
    }
    return { name: fields[0], root: resolve(serverRoot, fields[1]) };
  });
if (new Set(entries.map(entry => entry.name)).size !== entries.length) {
  throw new Error("Duplicate static app entry");
}

export async function buildApps(apps, output = resolve(serverRoot, "dist")) {
  for (const app of apps) {
    await build({
      configFile: resolve(serverRoot, "vite.config.ts"),
      root: app.root,
      build: { outDir: resolve(output, app.name) },
    });
    await validateAppOutput(resolve(output, app.name));
  }
}

export async function validateAppOutput(directory) {
  if (JSON.stringify(await readdir(directory)) !== JSON.stringify(["index.html"])) {
    throw new Error("App output must contain only index.html");
  }
  const input = resolve(directory, "index.html");
  const metadata = await stat(input);
  if (!metadata.isFile() || metadata.size === 0 || metadata.size > 1024 * 1024) {
    throw new Error("App HTML must be a nonempty file of at most 1 MiB");
  }
  const html = await readFile(input, "utf8");
  if (!html.length || Buffer.byteLength(html) > 1024 * 1024) {
    throw new Error("App HTML must be nonempty and at most 1 MiB");
  }
  if (!/<script\b[^>]*\btype="module"/.test(html)
    || /<script\b[^>]*\bsrc\s*=|<link\b[^>]*\bhref\s*=/i.test(html)) {
    throw new Error("App HTML must contain an inline module and no external script or stylesheet");
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await buildApps(entries);
}
