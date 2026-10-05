import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { delimiter, join, resolve } from "node:path";
import test from "node:test";
import { runInNewContext } from "node:vm";
import { buildApps, entries, serverRoot, validateAppOutput } from "./build-apps.mjs";

test("static entries build independent documents without publishing the second fixture", async () => {
  assert.deepEqual(entries.map(entry => entry.name), ["status"]);
  const output = await mkdtemp(join(tmpdir(), "waltr-served-apps-"));
  try {
    const manifest = join(output, "repo/crates/waltr-server");
    const dist = join(manifest, "dist");
    await mkdir(manifest, { recursive: true });
    await buildApps([...entries, {
      name: "second", root: resolve(serverRoot, "tests/fixtures/apps/second"),
    }], dist);
    assert.deepEqual((await readdir(dist)).sort(), ["second", "status"]);
    for (const name of ["status", "second"]) {
      assert.deepEqual(await readdir(join(dist, name)), ["index.html"]);
      const html = await readFile(join(dist, name, "index.html"), "utf8");
      assert.ok(Buffer.byteLength(html) > 0 && Buffer.byteLength(html) <= 1024 * 1024);
      assert.doesNotMatch(html, /<script\b[^>]*\bsrc\s*=|<link\b[^>]*\bhref\s*=/);
      if (name === "status") {
        assert.equal(html, await readFile(resolve(serverRoot, "dist/status/index.html"), "utf8"));
        assert.doesNotMatch(html, /independent-second-app-fixture/);
      } else {
        const element = { textContent: "" };
        const script = html.match(/<script\b[^>]*>([\s\S]*?)<\/script>/)[1];
        runInNewContext(script, { document: { getElementById: () => element } });
        assert.equal(element.textContent, "independent-second-app-fixture");
        assert.doesNotMatch(html, /Waltr Status/);
      }
    }
    // Compile the same Cargo pipeline against a test-only static entry list and real Vite output.
    await mkdir(join(output, "scripts"));
    await writeFile(join(output, "scripts/apps.entries"), "status assets/status\nsecond tests/fixtures/apps/second\n");
    await writeFile(join(output, "build.rs"), await readFile(resolve(serverRoot, "build.rs")));
    const executable = join(output, "build-assets");
    execFileSync("rustc", ["--edition=2024", join(output, "build.rs"), "-o", executable], {
      env: { ...process.env, CARGO_MANIFEST_DIR: manifest },
    });
    const bin = join(output, "bin");
    const bundled = join(output, "bundled");
    await mkdir(bin);
    await mkdir(bundled);
    await writeFile(join(bin, "pnpm"), `#!${process.execPath}\n// Vite already built the real outputs above.\n`, { mode: 0o755 });
    const env = { ...process.env, OUT_DIR: bundled, CARGO_FEATURE_APPS_SERVED_ASSETS: "1", PATH: `${bin}${delimiter}${process.env.PATH}` };
    for (const key of ["CARGO_FEATURE_WEB", "CARGO_FEATURE_APPS_HOST_ASSETS", "WALTR_APP_CONTAINER_DIST"]) delete env[key];
    execFileSync(executable, [], { env });
    const embedded = join(bundled, "served_app_assets.rs");
    const check = join(output, "check.rs");
    await writeFile(check, `mod assets { include!(${JSON.stringify(embedded)}); }
fn main() {
  assert!(assets::STATUS_HTML.unwrap().contains("Waltr Status"));
  assert!(assets::SECOND_HTML.unwrap().contains("independent-second-app-fixture"));
  assert!(!assets::STATUS_HTML.unwrap().contains("independent-second-app-fixture"));
}`);
    execFileSync("rustc", ["--edition=2024", check, "-o", join(output, "check")]);
    execFileSync(join(output, "check"));
    assert.ok(!(await readdir(resolve(serverRoot, "dist"))).includes("second"));
    const invalid = join(output, "invalid");
    await mkdir(invalid);
    await writeFile(join(invalid, "index.html"), '<script type="module" src="shared.js"></script>');
    await assert.rejects(validateAppOutput(invalid), /no external script/);
    await writeFile(join(invalid, "shared.js"), "shared chunk");
    await assert.rejects(validateAppOutput(invalid), /only index.html/);
    await rm(join(invalid, "shared.js"));
    await writeFile(join(invalid, "index.html"), "x".repeat(1024 * 1024 + 1));
    await assert.rejects(validateAppOutput(invalid), /at most 1 MiB/);
  } finally {
    await rm(output, { recursive: true, force: true });
  }
});
