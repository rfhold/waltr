import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { readFileSync } from "node:fs";
import { isIP } from "node:net";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { createContext, SourceTextModule, SyntheticModule } from "node:vm";
import { devAppContainerConfig } from "./dev-app-container.mjs";

test("defaults use default-feature Cargo assets, a separate container and exact stable Vite origins", () => {
  const config = devAppContainerConfig({});
  assert.deepEqual(config, {
    viteArgs: ["--host", "127.0.0.1", "--port", "5173", "--strictPort"],
    webEnv: { VITE_WALTR_MCP_APP_CONTAINER_URL: "http://127.0.0.1:14334/container.html" },
    containerEnv: {
      WALTR_APP_CONTAINER_ADDR: "127.0.0.1:14334",
      WALTR_APP_CONTAINER_ORIGIN: "http://127.0.0.1:14334",
      WALTR_APP_CONTAINER_HOST_ORIGINS: "http://127.0.0.1:5173,http://localhost:5173",
    },
  });
});

test("custom ports and IPv6 retain the actual URL and exact parent origins", () => {
  const config = devAppContainerConfig({ WALTR_WEB_DEV_PORT: "25173", WALTR_APP_CONTAINER_ADDR: "[::1]:24334" });
  assert.equal(config.webEnv.VITE_WALTR_MCP_APP_CONTAINER_URL, "http://[::1]:24334/container.html");
  assert.equal(config.containerEnv.WALTR_APP_CONTAINER_ORIGIN, "http://[::1]:24334");
  assert.equal(config.containerEnv.WALTR_APP_CONTAINER_HOST_ORIGINS, "http://127.0.0.1:25173,http://localhost:25173");
  assert.deepEqual(config.viteArgs.slice(-3), ["--port", "25173", "--strictPort"]);
});

test("explicit external container URL is honored without spawning a duplicate", () => {
  const config = devAppContainerConfig({ VITE_WALTR_MCP_APP_CONTAINER_URL: "https://apps.example/container.html" });
  assert.equal(config.containerEnv, null);
  assert.equal(config.webEnv.VITE_WALTR_MCP_APP_CONTAINER_URL, "https://apps.example/container.html");
});

test("managed reverse proxy adds only validated exact parent origins", () => {
  const config = devAppContainerConfig({
    WALTR_APP_CONTAINER_ORIGIN: "https://apps.example",
    WALTR_APP_CONTAINER_HOST_ORIGINS: "https://web.example,http://localhost:5173",
  });
  assert.equal(config.webEnv.VITE_WALTR_MCP_APP_CONTAINER_URL, "https://apps.example/container.html");
  assert.equal(config.containerEnv.WALTR_APP_CONTAINER_HOST_ORIGINS, "http://127.0.0.1:5173,http://localhost:5173,https://web.example");
});

test("malformed ports, non-loopback binds, mismatched origins and unsafe URLs fail before spawning", () => {
  for (const value of ["", "0", "65536", "-1", "1.5", "05173", "5173junk", " 5173"]) {
    assert.throws(() => devAppContainerConfig({ WALTR_WEB_DEV_PORT: value }), /port/);
  }
  for (const value of ["0.0.0.0:14334", "localhost:14334", "example.com:14334", "127.0.0.1:0", "127.0.0.1:65536", "127.0.0.1:14334/path", "::1:14334"]) {
    assert.throws(() => devAppContainerConfig({ WALTR_APP_CONTAINER_ADDR: value }));
  }
  for (const value of ["http://127.0.0.1:24334", "http://[::1]:14334", "https://apps.example/", "https://user@apps.example", "https://apps.example?q=1"]) {
    assert.throws(() => devAppContainerConfig({ WALTR_APP_CONTAINER_ORIGIN: value }));
  }
  for (const value of ["", "http://public.example/container.html", "https://user:secret@apps.example/container.html", "file:///container.html", "http://localhost:5173/container.html", "http://127.0.0.1:14333/container.html", "https://apps.example/container.html#x", "https://apps.example/container.html?x=1"]) {
    assert.throws(() => devAppContainerConfig({ VITE_WALTR_MCP_APP_CONTAINER_URL: value }));
  }
  for (const value of ["*", "null", "https://web.example/", "https://web.example\nhttps://evil.example", "http://127.0.0.1:14334"]) {
    assert.throws(() => devAppContainerConfig({ WALTR_APP_CONTAINER_HOST_ORIGINS: value }));
  }
  assert.throws(() => devAppContainerConfig({ WALTR_APP_CONTAINER_ADDR: "127.0.0.1:5173" }), /separate/);
  assert.throws(() => devAppContainerConfig({ WALTR_APP_CONTAINER_ADDR: "127.0.0.1:14333" }), /separate/);
});

async function runner(script, env = {}) {
  const records = [];
  const signals = [];
  const timers = [];
  const logs = [];
  const fakeProcess = Object.assign(new EventEmitter(), {
    env,
    kill: (pid, signal) => signals.push([pid, signal]),
    exit: (code) => { fakeProcess.exitCode = code; },
  });
  const context = createContext({
    process: fakeProcess,
    console: { log() {}, error() {} },
    URL,
    setTimeout: (callback) => { timers.push(callback); return { unref() {} }; },
  });
  const dependencies = {
    "node:child_process": {
      spawn(command, args, options) {
        const child = Object.assign(new EventEmitter(), {
          pid: 1000 + records.length,
          kill: (signal) => signals.push([child.pid, signal]),
        });
        records.push({ command, args, options, child });
        if (args.includes("wasm:build")) queueMicrotask(() => child.emit("exit", 0, null));
        return child;
      },
    },
    "node:fs": {
      mkdirSync() {},
      openSync(path) { logs.push(path); return 20 + logs.length; },
    },
    "node:url": { fileURLToPath },
    "node:net": { isIP },
  };
  const modules = new Map();
  async function load(url) {
    if (modules.has(url)) return modules.get(url);
    const module = new SourceTextModule(readFileSync(new URL(url), "utf8"), {
      context,
      identifier: url,
      initializeImportMeta(meta) { meta.url = url; },
    });
    modules.set(url, module);
    await module.link(async (specifier, parent) => {
      if (!dependencies[specifier]) return load(new URL(specifier, parent.identifier).href);
      const exports = dependencies[specifier];
      return new SyntheticModule(Object.keys(exports), function () {
        for (const [name, value] of Object.entries(exports)) this.setExport(name, value);
      }, { context });
    });
    return module;
  }
  const module = await load(new URL(script, import.meta.url).href);
  const completion = module.evaluate();
  await new Promise(setImmediate);
  return { records, signals, timers, logs, process: fakeProcess, completion };
}

for (const script of ["./dev.mjs", "./dev-web.mjs"]) {
  test(`${script} starts the actual app-container CLI and configures Vite after WASM prebuild`, async () => {
    const state = await runner(script);
    const container = state.records.find((record) => record.args.includes("app-container"));
    const web = state.records.find((record) => record.args.includes("dev:vite"));
    assert.ok(state.records[0].args.includes("wasm:build"));
    assert.ok(!state.records.some((record) => record.args.includes("build:app-container")));
    assert.equal(container.command, "cargo");
    assert.deepEqual(Array.from(container.args), ["run", "-p", "waltr-server", "--", "app-container"]);
    assert.equal(container.options.detached, true);
    assert.equal(web.options.env.VITE_WALTR_MCP_APP_CONTAINER_URL, `${container.options.env.WALTR_APP_CONTAINER_ORIGIN}/container.html`);
    assert.equal(container.options.env.WALTR_APP_CONTAINER_HOST_ORIGINS, "http://127.0.0.1:5173,http://localhost:5173");
    assert.deepEqual(Array.from(web.args).slice(-5), ["--host", "127.0.0.1", "--port", "5173", "--strictPort"]);
    if (script === "./dev.mjs") assert.ok(state.logs.some((path) => path.endsWith("/app-container.log")));
    state.process.emit("SIGINT");
    for (const record of state.records.slice(1)) {
      const pid = record.options.detached ? -record.child.pid : record.child.pid;
      assert.ok(state.signals.some(([target, signal]) => target === pid && signal === "SIGTERM"));
    }
    state.timers[0]();
    assert.equal(state.process.exitCode, 130);
    assert.ok(state.signals.some(([target, signal]) => target === -container.child.pid && signal === "SIGKILL"));
    state.records.find((record) => record.args.includes("waltr-tui"))?.child.emit("exit", 0, null);
    await state.completion;
  });

  test(`${script} a container spawn error stops all other managed children`, async () => {
    const state = await runner(script);
    const container = state.records.find((record) => record.args.includes("app-container"));
    container.child.emit("error", new Error("spawn cargo ENOENT"));
    for (const record of state.records.slice(1).filter((record) => record !== container)) {
      const pid = record.options.detached ? -record.child.pid : record.child.pid;
      assert.ok(state.signals.some(([target, signal]) => target === pid && signal === "SIGTERM"));
    }
    state.timers[0]();
    assert.equal(state.process.exitCode, 1);
    state.records.find((record) => record.args.includes("waltr-tui"))?.child.emit("exit", 0, null);
    await state.completion;
  });

  test(`${script} an explicit container override skips the managed asset listener`, async () => {
    const state = await runner(script, { VITE_WALTR_MCP_APP_CONTAINER_URL: "https://apps.example/container.html" });
    assert.ok(!state.records.some((record) => record.args.includes("app-container")));
    const web = state.records.find((record) => record.args.includes("dev:vite"));
    assert.equal(web.options.env.VITE_WALTR_MCP_APP_CONTAINER_URL, "https://apps.example/container.html");
    state.process.emit("SIGTERM");
    state.records.find((record) => record.args.includes("waltr-tui"))?.child.emit("exit", 0, null);
    await state.completion;
  });

  test(`${script} a Vite strict-port failure stops the server and app container`, async () => {
    const state = await runner(script);
    const web = state.records.find((record) => record.args.includes("dev:vite"));
    web.child.emit("exit", 1, null);
    for (const name of ["waltr-server", "app-container"]) {
      const record = state.records.find((entry) => entry.args.includes(name));
      assert.ok(state.signals.some(([target, signal]) => target === -record.child.pid && signal === "SIGTERM"));
    }
    state.timers[0]();
    assert.equal(state.process.exitCode, 1);
    state.records.find((record) => record.args.includes("waltr-tui"))?.child.emit("exit", 0, null);
    await state.completion;
  });

  test(`${script} container startup failure stops Vite and preserves the failure code`, async () => {
    const state = await runner(script);
    const container = state.records.find((record) => record.args.includes("app-container"));
    container.child.emit("exit", 1, null);
    const web = state.records.find((record) => record.args.includes("dev:vite"));
    assert.ok(state.signals.some(([target, signal]) => target === -web.child.pid && signal === "SIGTERM"));
    state.timers[0]();
    assert.equal(state.process.exitCode, 1);
    state.records.find((record) => record.args.includes("waltr-tui"))?.child.emit("exit", 0, null);
    await state.completion;
  });
}
