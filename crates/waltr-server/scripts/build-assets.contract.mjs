import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, mkdir, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

test("Cargo asset orchestration preserves the feature and override matrix", async () => {
  const temporary = await mkdtemp(join(tmpdir(), "waltr-build-assets-"));
  try {
    const root = join(temporary, "repo");
    const server = join(root, "crates/waltr-server");
    const web = join(root, "crates/waltr-web");
    const bin = join(temporary, "bin");
    await Promise.all([mkdir(server, { recursive: true }), mkdir(web, { recursive: true }), mkdir(bin)]);
    const executable = join(bin, "build-assets");
    execFileSync("rustc", ["--edition=2024", fileURLToPath(new URL("../build.rs", import.meta.url)), "-o", executable], {
      env: { ...process.env, CARGO_MANIFEST_DIR: server },
    });
    // Mock package commands only: real Vite output is checked by status-app.contract.mjs.
    await writeFile(join(bin, "pnpm"), `#!${process.execPath}
const fs = require("node:fs");
const path = require("node:path");
const [, , ...args] = process.argv;
fs.appendFileSync(process.env.BUILD_LOG, JSON.stringify(args) + "\\n");
const web = path.join(process.cwd(), "crates/waltr-web/dist");
const status = path.join(process.cwd(), "crates/waltr-server/dist/status");
if (args[1] === "@rfhold/waltr-server") {
  fs.mkdirSync(status, { recursive: true });
  fs.writeFileSync(path.join(status, "index.html"), '<!doctype html><script type="module">console.log("status")</script>');
} else {
  fs.mkdirSync(path.join(web, "app-container"), { recursive: true });
  fs.writeFileSync(path.join(web, "app-container/container.html"), '<script src="container.js"></script>');
  fs.writeFileSync(path.join(web, "app-container/container.js"), 'console.log("container")');
  if (args[2] === "build") {
    fs.writeFileSync(path.join(web, "index.html"), "web UI");
    fs.mkdirSync(path.join(web, "apps"), { recursive: true });
    fs.writeFileSync(path.join(web, "apps/legacy.html"), "excluded");
  }
}
`, { mode: 0o755 });
    const override = join(temporary, "override");
    await mkdir(override);
    await writeFile(join(override, "container.html"), "override HTML");
    await writeFile(join(override, "container.js"), "override JS");
    const cases = [
      { name: "no-assets", host: false, served: false, web: false, commands: [] },
      { name: "host-only", host: true, served: false, web: false, commands: [["@rfhold/waltr-web", "build:app-container"]] },
      { name: "served-only", host: false, served: true, web: false, commands: [["@rfhold/waltr-server", "build"]] },
      { name: "default", host: true, served: true, web: false, commands: [["@rfhold/waltr-web", "build:app-container"], ["@rfhold/waltr-server", "build"]] },
      { name: "web", host: true, served: true, web: true, commands: [["@rfhold/waltr-web", "build"], ["@rfhold/waltr-server", "build"]] },
      { name: "host-override", host: true, served: false, web: false, override, commands: [] },
      { name: "served-with-override", host: true, served: true, web: false, override, commands: [["@rfhold/waltr-server", "build"]] },
      { name: "web-with-override", host: true, served: true, web: true, override, commands: [["@rfhold/waltr-web", "build"], ["@rfhold/waltr-server", "build"]] },
    ];
    for (const item of cases) {
      const output = join(temporary, item.name);
      await mkdir(output);
      const log = join(output, "commands.jsonl");
      await writeFile(log, "");
      const env = { ...process.env, OUT_DIR: output, BUILD_LOG: log, PATH: `${bin}${delimiter}${process.env.PATH}` };
      for (const key of ["CARGO_FEATURE_WEB", "CARGO_FEATURE_APPS_HOST_ASSETS", "CARGO_FEATURE_APPS_SERVED_ASSETS", "WALTR_APP_CONTAINER_DIST"]) delete env[key];
      if (item.host) env.CARGO_FEATURE_APPS_HOST_ASSETS = "1";
      if (item.served) env.CARGO_FEATURE_APPS_SERVED_ASSETS = "1";
      if (item.web) env.CARGO_FEATURE_WEB = "1";
      if (item.override) env.WALTR_APP_CONTAINER_DIST = item.override;
      execFileSync(executable, [], { env });
      const commands = (await readFile(log, "utf8")).trim().split("\n").filter(Boolean).map(line => JSON.parse(line));
      assert.deepEqual(commands, item.commands.map(command => ["--filter", ...command]), item.name);
      const hostSource = await readFile(join(output, "apps_assets.rs"), "utf8");
      const statusSource = await readFile(join(output, "served_app_assets.rs"), "utf8");
      assert.equal(hostSource.includes("Some(include_str!"), item.host, item.name);
      assert.equal(statusSource.includes("Some(include_str!"), item.served, item.name);
      if (item.override) assert.equal(await readFile(join(output, "container.html"), "utf8"), "override HTML");
      if (item.web) {
        const webSource = await readFile(join(output, "web_assets.rs"), "utf8");
        assert.match(webSource, /\/index\.html/);
        assert.doesNotMatch(webSource, /\/app-container\/|\/apps\//);
      }
    }
    await writeFile(join(override, "extra.js"), "invalid");
    const output = join(temporary, "invalid-override");
    await mkdir(output);
    const env = { ...process.env, OUT_DIR: output, CARGO_FEATURE_APPS_HOST_ASSETS: "1", WALTR_APP_CONTAINER_DIST: override };
    delete env.CARGO_FEATURE_WEB;
    delete env.CARGO_FEATURE_APPS_SERVED_ASSETS;
    assert.throws(() => execFileSync(executable, [], { env, stdio: "pipe" }), /container dist must contain only/);
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
});
