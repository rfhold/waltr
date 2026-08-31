import assert from "node:assert/strict";
import { readdir, readFile } from "node:fs/promises";

const root = new URL("../", import.meta.url);
const dist = new URL("dist/", root);
const index = await readFile(new URL("index.html", dist), "utf8");
const manifest = JSON.parse(await readFile(new URL("site.webmanifest", dist), "utf8"));
const config = await readFile(new URL("../waltr-app/tauri.conf.json", root), "utf8");

assert.match(index, /<base href="\/">/);
assert.match(config, /base-uri 'self'/);

const references = [...index.matchAll(/(?:href|src)="([^"#]+)"/g)]
  .map((match) => match[1])
  .filter((reference) => reference !== "/");
assert.ok(references.length >= 4, "expected JS, CSS, manifest, and favicon references");
for (const reference of references) {
  assert.match(reference, /^\.\//, `packaged reference must stay syntactically relative: ${reference}`);
}

for (const route of [
  "/sessions/session-a",
  "/routines/invocation-a",
  "/config/provider/edit/provider-a/models/edit/model-a",
]) {
  const routeUrl = new URL(route, "tauri://localhost");
  const documentBase = new URL("/", routeUrl);
  for (const reference of references) {
    const resolved = new URL(reference, documentBase);
    assert.equal(resolved.origin, "null");
    assert.ok(
      resolved.pathname.startsWith("/assets/")
        || resolved.pathname === "/favicon.svg"
        || resolved.pathname === "/site.webmanifest",
      `${reference} resolved below nested route ${route}: ${resolved.pathname}`,
    );
  }
}

for (const icon of manifest.icons ?? []) {
  assert.equal(new URL(icon.src, "tauri://localhost/site.webmanifest").pathname, "/favicon.svg");
}

const outputFiles = await walk(dist);
assert.ok(outputFiles.some((path) => path.startsWith("assets/") && path.endsWith(".js")));
assert.ok(outputFiles.some((path) => path.startsWith("assets/") && path.endsWith(".css")));
assert.ok(outputFiles.includes("favicon.svg"));
assert.ok(outputFiles.includes("site.webmanifest"));
assert.ok(outputFiles.every((path) => !path.toLowerCase().includes("wasm")));

async function walk(directory, prefix = "") {
  const paths = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const relative = `${prefix}${entry.name}`;
    if (entry.isDirectory()) paths.push(...await walk(new URL(`${entry.name}/`, directory), `${relative}/`));
    else paths.push(relative);
  }
  return paths;
}
