import assert from "node:assert/strict";
import { execFile, spawnSync } from "node:child_process";
import { cpSync, existsSync, lstatSync, renameSync, readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import * as fs from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { main } from "./dev-install.mjs";

const tempRoot = path.join(tmpdir(), "opencode");
const directoryHelper = fileURLToPath(new URL("linux-directory-helper.py", import.meta.url));
const runFile = promisify(execFile);

async function bundle(directory, marker = "new", identifier = "dev.waltr.app") {
  await fs.mkdir(path.join(directory, "Contents/MacOS"), { recursive: true });
  await fs.writeFile(path.join(directory, "Contents/Info.plist"), JSON.stringify({ CFBundleIdentifier: identifier, CFBundleExecutable: "waltr" }));
  await fs.writeFile(path.join(directory, "Contents/MacOS/waltr"), marker, { mode: 0o755 });
  await fs.symlink("MacOS/waltr", path.join(directory, "Contents/binary-link"));
}

async function fixture(t, { installed = true } = {}) {
  await fs.mkdir(tempRoot, { recursive: true });
  const directory = await fs.mkdtemp(path.join(tempRoot, "dev-install-test-"));
  t.after(() => fs.rm(directory, { recursive: true, force: true }));
  const root = path.join(directory, "repo with spaces");
  const home = path.join(directory, "home with spaces");
  const destination = path.join(home, "Applications/Waltr.app");
  const target = path.join(root, "target/macos-default");
  const source = path.join(target, "aarch64-apple-darwin/debug/bundle/macos/Waltr.app");
  await bundle(source);
  if (installed) await bundle(destination, "old");
  const calls = [];
  const logs = [];
  const options = {
    platform: "darwin", arch: "arm64", root, home,
    env: { CARGO_TARGET_DIR: "/wrong", SENTINEL: "kept" },
    log: (message) => logs.push(message),
    runner: (command, args, options) => {
      calls.push({ command, args, options });
      if (command === "/usr/libexec/PlistBuddy") {
        // The fixtures use JSON; no real macOS tools run in these tests.
        return fsReadPlist(args);
      }
      if (command === "/usr/bin/ditto") return copy(args);
      if (command === "cargo" || command === "/usr/bin/codesign") return "";
      throw new Error(`Unexpected command: ${command}`);
    },
  };
  function fsReadPlist(args) {
    const key = args[1].replace("Print :", "");
    const value = JSON.parse(readFileSync(args[2], "utf8"))[key];
    if (value === undefined) throw new Error("Missing plist key");
    return value;
  }
  function copy(args) {
    cpSync(args[0], args[1], { recursive: true, verbatimSymlinks: true });
    return "";
  }
  const marker = () => fs.readFile(path.join(destination, "Contents/MacOS/waltr"), "utf8");
  const clean = async () => assert.deepEqual((await fs.readdir(path.dirname(destination))).sort(), ["Waltr.app"]);
  return { directory, root, home, destination, source, target, calls, logs, options, marker, clean };
}

test("build uses exact packaged debug command, absolute cwd and explicit target; replacement preserves symlinks", async (t) => {
  const f = await fixture(t);
  await main([], f.options);
  const build = f.calls.find((call) => call.command === "cargo");
  assert.deepEqual(build.args, ["tauri", "build", "--debug", "--target", "aarch64-apple-darwin", "--bundles", "app", "--no-sign", "--", "--locked"]);
  assert.equal(build.options.cwd, path.join(f.root, "crates/waltr-app"));
  assert.deepEqual(build.options.env, { CARGO_TARGET_DIR: f.target, SENTINEL: "kept" });
  assert.equal(build.options.stdio, "inherit");
  assert.equal(await f.marker(), "new");
  assert.equal(await fs.readlink(path.join(f.destination, "Contents/binary-link")), "MacOS/waltr");
  assert.ok(f.logs.at(-1).includes(`open ${JSON.stringify(f.destination)}`));
  assert.ok(f.calls.every((call) => ["cargo", "/usr/bin/ditto", "/usr/libexec/PlistBuddy", "/usr/bin/codesign"].includes(call.command)));
  await f.clean();
});

test("copy with a different executable is rejected before signing", async (t) => {
  const f = await fixture(t);
  const runner = f.options.runner;
  f.options.runner = (...args) => {
    const result = runner(...args);
    if (args[0] === "/usr/bin/ditto") writeFileSync(path.join(args[1][1], "Contents/MacOS/waltr"), "wrong copy");
    return result;
  };
  await assert.rejects(main([], f.options), /Staged executable differs/);
  assert.ok(!f.calls.some((call) => call.command === "/usr/bin/codesign"));
  assert.equal(await f.marker(), "old");
  await f.clean();
});

test("signed executable digest is verified after promotion without claiming a revision or running identity", async (t) => {
  const f = await fixture(t);
  f.options.env.WALTR_APP_REVISION = "abcdef123";
  const runner = f.options.runner;
  f.options.runner = (...args) => {
    const result = runner(...args);
    if (args[0] === "/usr/bin/codesign" && args[1].includes("--sign")) {
      writeFileSync(path.join(args[1].at(-1), "Contents/MacOS/waltr"), "signed executable");
    }
    return result;
  };
  await main([], f.options);
  assert.equal(await f.marker(), "signed executable");
  const digest = createHash("sha256").update("signed executable").digest("hex");
  assert.ok(f.logs.at(-1).includes(`Installed executable SHA-256: ${digest}`));
  assert.match(f.logs.at(-1), /Embedded revision and running-process identity were not checked/);
  assert.ok(f.logs.every((message) => !message.includes("abcdef123")));
  assert.equal(f.calls.find((call) => call.command === "cargo").options.env.WALTR_APP_REVISION, "abcdef123");
  await f.clean();
});

for (const installed of [true, false]) {
  test(`post-promotion executable mismatch restores ${installed ? "previous app" : "empty destination"}`, async (t) => {
    const f = await fixture(t, { installed });
    f.options.io = { ...fs, rename: async (from, to) => {
      await fs.rename(from, to);
      if (from.endsWith("/Waltr.app") && from.includes(".waltr-install-")) {
        await fs.writeFile(path.join(to, "Contents/MacOS/waltr"), "tampered");
      }
    } };
    await assert.rejects(main([], f.options), /Installed executable differs/);
    if (installed) {
      assert.equal(await f.marker(), "old");
      await f.clean();
    } else {
      await assert.rejects(fs.lstat(f.destination), { code: "ENOENT" });
      assert.deepEqual(await fs.readdir(path.dirname(f.destination)), []);
    }
    assert.ok(!f.logs.some((message) => message.startsWith("Installed:")));
  });
}

test("post-promotion mismatch and rollback removal failure retain the previous app backup", async (t) => {
  const f = await fixture(t);
  f.options.io = { ...fs,
    rename: async (from, to) => {
      await fs.rename(from, to);
      if (from.endsWith("/Waltr.app") && from.includes(".waltr-install-")) {
        await fs.writeFile(path.join(to, "Contents/MacOS/waltr"), "tampered");
      }
    },
    rm: async (target, options) => {
      if (target === f.destination) throw new Error("fake rollback removal failure");
      return fs.rm(target, options);
    },
  };
  await assert.rejects(main([], f.options), /Previous app preserved at/);
  const transaction = (await fs.readdir(path.dirname(f.destination))).find((name) => name.startsWith(".waltr-install-"));
  assert.ok(transaction);
  assert.equal(await fs.readFile(path.join(path.dirname(f.destination), transaction, "previous.app/Contents/MacOS/waltr"), "utf8"), "old");
});

test("destination substitution after promotion refuses rollback removal and retains backup", async (t) => {
  const f = await fixture(t);
  f.options.io = { ...fs, rename: async (from, to) => {
    await fs.rename(from, to);
    if (from.endsWith("/Waltr.app") && from.includes(".waltr-install-")) {
      await fs.rename(to, `${to}.displaced`);
      await bundle(to, "external app");
    }
  } };
  await assert.rejects(main([], f.options), /Previous app preserved at/);
  assert.equal(await f.marker(), "external app");
  const transaction = (await fs.readdir(path.dirname(f.destination))).find((name) => name.startsWith(".waltr-install-"));
  assert.ok(transaction);
  assert.equal(await fs.readFile(path.join(path.dirname(f.destination), transaction, "previous.app/Contents/MacOS/waltr"), "utf8"), "old");
});

test("repeated installs replace only the verified destination", async (t) => {
  const f = await fixture(t);
  await main([], f.options);
  await fs.writeFile(path.join(f.source, "Contents/MacOS/waltr"), "next");
  await main([], f.options);
  assert.equal(await f.marker(), "next");
  await f.clean();
});

test("invalid staged copy preserves the previous app", async (t) => {
  const f = await fixture(t);
  const runner = f.options.runner;
  f.options.runner = (...args) => {
    if (args[0] === "/usr/bin/ditto") return "";
    return runner(...args);
  };
  await assert.rejects(main([], f.options), /Invalid app directory/);
  assert.ok(!f.calls.some((call) => call.command === "/usr/bin/codesign"));
  assert.equal(await f.marker(), "old");
  await f.clean();
});

test("destination changes during build fail without overwrite", async (t) => {
  const f = await fixture(t);
  const runner = f.options.runner;
  const { renameSync, mkdirSync } = await import("node:fs");
  f.options.runner = (...args) => {
    if (args[0] === "cargo") {
      renameSync(f.destination, `${f.destination}.saved`);
      mkdirSync(f.destination);
    }
    return runner(...args);
  };
  await assert.rejects(main([], f.options), /Destination changed/);
  assert.deepEqual(await fs.readdir(f.destination), []);
  assert.equal(await fs.readFile(path.join(`${f.destination}.saved`, "Contents/MacOS/waltr"), "utf8"), "old");
});

test("first install creates Applications and installs complete bundle", async (t) => {
  const f = await fixture(t, { installed: false });
  await main([], f.options);
  assert.equal(await f.marker(), "new");
  await f.clean();
});

test("build failure preserves previous app and never copies stale artifact", async (t) => {
  const f = await fixture(t);
  const runner = f.options.runner;
  f.options.runner = (...args) => {
    if (args[0] === "cargo") throw new Error("fake build failure");
    return runner(...args);
  };
  await assert.rejects(main([], f.options), /fake build failure/);
  assert.equal(await f.marker(), "old");
  assert.ok(!f.calls.some((call) => call.command === "/usr/bin/ditto"));
  await f.clean();
});

test("copy failure preserves previous app and removes only its transaction", async (t) => {
  const f = await fixture(t);
  const unrelated = path.join(f.home, "Applications/.waltr-install-unrelated");
  await fs.mkdir(unrelated);
  const runner = f.options.runner;
  f.options.runner = (...args) => {
    if (args[0] === "/usr/bin/ditto") throw new Error("fake copy failure");
    return runner(...args);
  };
  await assert.rejects(main([], f.options), /fake copy failure/);
  assert.equal(await f.marker(), "old");
  assert.deepEqual((await fs.readdir(path.dirname(f.destination))).sort(), [".waltr-install-unrelated", "Waltr.app"]);
});

for (const invalid of ["missing", "identifier", "executable", "plist", "symlink"]) {
  test(`invalid source (${invalid}) refuses install`, async (t) => {
    const f = await fixture(t);
    if (invalid === "missing") await fs.rm(f.source, { recursive: true });
    if (invalid === "identifier") await fs.writeFile(path.join(f.source, "Contents/Info.plist"), JSON.stringify({ CFBundleIdentifier: "other.app" }));
    if (invalid === "executable") await fs.unlink(path.join(f.source, "Contents/MacOS/waltr"));
    if (invalid === "plist") await fs.writeFile(path.join(f.source, "Contents/Info.plist"), "not a plist");
    if (invalid === "symlink") {
      await fs.rename(f.source, `${f.source}.real`);
      await fs.symlink(`${f.source}.real`, f.source);
    }
    await assert.rejects(main([], f.options));
    assert.equal(await f.marker(), "old");
    assert.ok(!f.calls.some((call) => ["/usr/bin/ditto", "/usr/bin/codesign"].includes(call.command)));
    await f.clean();
  });
}

for (const invalid of ["identifier", "symlink", "file"]) {
  test(`unsafe destination (${invalid}) fails before build`, async (t) => {
    const f = await fixture(t);
    if (invalid === "identifier") await fs.writeFile(path.join(f.destination, "Contents/Info.plist"), JSON.stringify({ CFBundleIdentifier: "other.app" }));
    if (invalid === "file") {
      await fs.rm(f.destination, { recursive: true });
      await fs.writeFile(f.destination, "not an app");
    }
    if (invalid === "symlink") {
      await fs.rename(f.destination, `${f.destination}.real`);
      await fs.symlink(`${f.destination}.real`, f.destination);
    }
    await assert.rejects(main([], f.options));
    assert.ok(!f.calls.some((call) => call.command === "cargo"));
    assert.ok(await fs.lstat(f.destination));
  });
}

test("promotion failure rolls back previous bundle", async (t) => {
  const f = await fixture(t);
  f.options.io = { ...fs, rename: async (from, to) => {
    if (from.endsWith("/Waltr.app") && from.includes(".waltr-install-")) throw new Error("fake promotion failure");
    return fs.rename(from, to);
  } };
  await assert.rejects(main([], f.options), /fake promotion failure/);
  assert.equal(await f.marker(), "old");
  await f.clean();
});

test("cleanup failure after promotion reports installed app and retained transaction", async (t) => {
  const f = await fixture(t);
  const cleanupError = Object.assign(new Error("fake cleanup EPERM"), { code: "EPERM" });
  f.options.io = { ...fs, rm: async () => { throw cleanupError; } };
  const error = await main([], f.options).catch((error) => error);
  assert.ok(error instanceof Error);
  assert.equal(error.cause, cleanupError);
  assert.ok(error.message.includes(`Installed: ${f.destination}, but cleanup failed.`));
  assert.equal(await f.marker(), "new");
  const transaction = (await fs.readdir(path.dirname(f.destination))).find((name) => name.startsWith(".waltr-install-"));
  const transactionPath = path.join(path.dirname(f.destination), transaction);
  assert.ok(error.message.includes(`Inspect transaction path: ${transactionPath}`));
  assert.ok(error.message.includes(path.join(transactionPath, "previous.app")));
  assert.equal(await fs.readFile(path.join(transactionPath, "previous.app/Contents/MacOS/waltr"), "utf8"), "old");
  assert.ok(!f.logs.some((message) => message.startsWith("Installed:")));
});

test("cleanup failure preserves primary promotion failure and rollback context", async (t) => {
  const f = await fixture(t);
  const promotionError = new Error("fake promotion failure");
  const cleanupError = Object.assign(new Error("fake cleanup EPERM"), { code: "EPERM" });
  f.options.io = { ...fs,
    rename: async (from, to) => {
      if (from.endsWith("/Waltr.app") && from.includes(".waltr-install-")) throw promotionError;
      return fs.rename(from, to);
    },
    rm: async () => { throw cleanupError; },
  };
  await assert.rejects(main([], f.options), (error) => {
    assert.ok(error instanceof AggregateError);
    assert.equal(error.cause, promotionError);
    assert.deepEqual(error.errors, [promotionError, cleanupError]);
    assert.match(error.message, /^fake promotion failure\nCleanup failed: fake cleanup EPERM/);
    assert.match(error.message, /Inspect transaction path:/);
    assert.match(error.message, /Previous-app backup path \(if retained\):/);
    assert.ok(!error.message.includes("Installed:"));
    return true;
  });
  assert.equal(await f.marker(), "old");
  const transaction = (await fs.readdir(path.dirname(f.destination))).find((name) => name.startsWith(".waltr-install-"));
  assert.ok(transaction);
  await assert.rejects(fs.stat(path.join(path.dirname(f.destination), transaction, "previous.app")), { code: "ENOENT" });
});

test("rollback failure preserves backup for manual recovery", async (t) => {
  const f = await fixture(t);
  f.options.io = { ...fs, rename: async (from, to) => {
    if (from.includes(".waltr-install-")) throw new Error("fake rename failure");
    return fs.rename(from, to);
  } };
  await assert.rejects(main([], f.options), /Previous app preserved at/);
  const [transaction] = await fs.readdir(path.dirname(f.destination));
  assert.match(transaction, /^\.waltr-install-/);
  assert.equal(await fs.readFile(path.join(path.dirname(f.destination), transaction, "previous.app/Contents/MacOS/waltr"), "utf8"), "old");
});

test("platform, architecture and unknown flags fail before subprocesses", async () => {
  const runner = () => assert.fail("unexpected subprocess");
  await assert.rejects(main([], { platform: "linux", arch: "ia32", runner }), /Linux \(x64\/arm64\)/);
  await assert.rejects(main([], { platform: "darwin", arch: "x64", runner }), /Apple Silicon/);
  await assert.rejects(main(["--destination", "/tmp/elsewhere"], { runner }), /Usage/);
});

test("help and dry-run perform no filesystem operations or subprocesses and describe signing", async () => {
  const fail = () => assert.fail("unexpected side effect");
  const logs = [];
  const options = { platform: "darwin", arch: "arm64", home: "/fake home", root: "/fake repo", env: {}, io: new Proxy({}, { get: fail }), runner: fail, log: (message) => logs.push(message) };
  await main(["--help"], options);
  await main(["--dry-run"], options);
  assert.match(logs[0], /WALTR_DEV_SIGNING_IDENTITY/);
  assert.match(logs[0], /ad-hoc/);
  assert.match(logs[1], /Sign staged bundle: ad-hoc \(-\); identifier dev\.waltr\.app/);
  assert.match(logs[1], /Entitlements: \/fake repo\/crates\/waltr-app\/Entitlements\.plist/);
  assert.match(logs[1], /codesign --verify --deep --strict before destination replacement/);
  assert.match(logs[1], /do not guarantee.*Accessibility/);
  options.env.WALTR_DEV_SIGNING_IDENTITY = "private identity name";
  await main(["--dry-run"], options);
  assert.match(logs[2], /supplied WALTR_DEV_SIGNING_IDENTITY/);
  assert.ok(!logs[2].includes(options.env.WALTR_DEV_SIGNING_IDENTITY));
});

test("complete staged bundle is validated, signed and verified before destination guard and promotion", async (t) => {
  const f = await fixture(t);
  const events = [];
  const runner = f.options.runner;
  f.options.runner = (...args) => {
    const [command, commandArgs] = args;
    if (command === "/usr/bin/ditto") events.push("copy");
    if (command === "/usr/libexec/PlistBuddy" && commandArgs[2].includes(".waltr-install-")) events.push(commandArgs[1]);
    if (command === "/usr/bin/codesign") {
      events.push(commandArgs.includes("--sign") ? "sign" : "verify");
      assert.equal(readFileSync(path.join(commandArgs.at(-1), "Contents/MacOS/waltr"), "utf8"), "new");
    }
    return runner(...args);
  };
  f.options.io = { ...fs,
    lstat: async (target) => {
      if (target === f.destination && events.includes("copy")) events.push("destination guard");
      return fs.lstat(target);
    },
    rename: async (from, to) => {
      events.push(from === f.destination ? "backup" : "promote");
      return fs.rename(from, to);
    },
  };
  await main([], f.options);
  assert.deepEqual(events.slice(0, 6), ["copy", "Print :CFBundleIdentifier", "Print :CFBundleExecutable", "sign", "verify", "destination guard"]);
  assert.ok(events.indexOf("backup") > events.indexOf("destination guard"));
  assert.ok(events.indexOf("promote") > events.indexOf("backup"));
  const [sign, verify] = f.calls.filter((call) => call.command === "/usr/bin/codesign");
  const staged = f.calls.find((call) => call.command === "/usr/bin/ditto").args[1];
  assert.deepEqual(sign.args, ["--force", "--sign", "-", "--identifier", "dev.waltr.app", "--entitlements", path.join(f.root, "crates/waltr-app/Entitlements.plist"), staged]);
  assert.deepEqual(verify.args, ["--verify", "--deep", "--strict", staged]);
  assert.notEqual(staged, f.source);
  assert.notEqual(staged, f.destination);
  await f.clean();
});

test("supplied signing identity is one literal argument and absent from installer logs", async (t) => {
  const f = await fixture(t);
  const identity = "Developer ID: Local Person (TEAM); $(touch do-not-run)";
  f.options.env.WALTR_DEV_SIGNING_IDENTITY = identity;
  await main([], f.options);
  const sign = f.calls.find((call) => call.command === "/usr/bin/codesign");
  assert.equal(sign.args[2], identity);
  assert.equal(sign.options?.shell, undefined);
  assert.ok(f.logs.every((message) => !message.includes(identity)));
  await f.clean();
});

for (const identity of ["", "  \t\n"]) {
  test(`blank signing override ${JSON.stringify(identity)} fails before any side effects`, async () => {
    const fail = () => assert.fail("unexpected side effect");
    const options = { platform: "darwin", arch: "arm64", env: { WALTR_DEV_SIGNING_IDENTITY: identity }, runner: fail, io: new Proxy({}, { get: fail }), log: fail };
    await assert.rejects(main([], options), /WALTR_DEV_SIGNING_IDENTITY must not be blank/);
    await assert.rejects(main(["--dry-run"], options), /WALTR_DEV_SIGNING_IDENTITY must not be blank/);
  });
}

for (const operation of ["--sign", "--verify"]) {
  for (const installed of [true, false]) {
    test(`${operation} failure preserves ${installed ? "old app" : "empty destination"} and cleans transaction`, async (t) => {
      const f = await fixture(t, { installed });
      const runner = f.options.runner;
      f.options.runner = (...args) => {
        if (args[0] === "/usr/bin/codesign" && args[1].includes(operation)) throw new Error(`fake ${operation} failure`);
        return runner(...args);
      };
      f.options.io = { ...fs, rename: async () => assert.fail("must not rename before successful verification") };
      await assert.rejects(main([], f.options), new RegExp(`fake ${operation} failure`));
      if (installed) {
        assert.equal(await f.marker(), "old");
        await f.clean();
      } else {
        await assert.rejects(fs.lstat(f.destination), { code: "ENOENT" });
        assert.deepEqual(await fs.readdir(path.dirname(f.destination)), []);
      }
      if (operation === "--sign") assert.ok(!f.calls.some((call) => call.command === "/usr/bin/codesign" && call.args.includes("--verify")));
    });
  }
}

test("cleanup failure retains signature failure and reports retained staged transaction", async (t) => {
  const f = await fixture(t);
  const signingError = new Error("fake signature failure");
  const cleanupError = new Error("fake cleanup failure");
  const runner = f.options.runner;
  f.options.runner = (...args) => {
    if (args[0] === "/usr/bin/codesign") throw signingError;
    return runner(...args);
  };
  f.options.io = { ...fs, rm: async () => { throw cleanupError; } };
  await assert.rejects(main([], f.options), (error) => {
    assert.ok(error instanceof AggregateError);
    assert.equal(error.cause, signingError);
    assert.deepEqual(error.errors, [signingError, cleanupError]);
    assert.match(error.message, /Inspect transaction path:/);
    assert.ok(!error.message.includes("Installed:"));
    return true;
  });
  assert.equal(await f.marker(), "old");
  const transaction = (await fs.readdir(path.dirname(f.destination))).find((name) => name.startsWith(".waltr-install-"));
  assert.ok(await fs.stat(path.join(path.dirname(f.destination), transaction, "Waltr.app")));
});

function linuxElf(machine = 62, label = "new") {
  const bytes = Buffer.alloc(128 + label.length);
  Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1]).copy(bytes);
  bytes.writeUInt16LE(3, 16);
  bytes.writeUInt16LE(machine, 18);
  bytes.writeUInt32LE(1, 20);
  bytes.writeBigUInt64LE(0x80n, 24);
  bytes.writeBigUInt64LE(64n, 32);
  bytes.writeUInt16LE(64, 52);
  bytes.writeUInt16LE(56, 54);
  bytes.writeUInt16LE(1, 56);
  bytes.writeUInt32LE(1, 64); // PT_LOAD
  bytes.writeUInt32LE(5, 68); // PF_R | PF_X
  bytes.writeBigUInt64LE(BigInt(bytes.length), 96);
  bytes.writeBigUInt64LE(BigInt(bytes.length), 104);
  bytes.write(label, 128);
  return bytes;
}

async function linuxFixture(t, { arch = "x64", machine = 62, installed = false } = {}) {
  await fs.mkdir(tempRoot, { recursive: true });
  const directory = await fs.mkdtemp(path.join(await fs.realpath(tempRoot), "dev-install-linux-test-"));
  t.after(() => fs.rm(directory, { recursive: true, force: true }));
  const home = path.join(directory, "home with spaces");
  const root = path.join(directory, "repo with spaces");
  const local = path.join(home, ".local");
  const parent = path.join(local, "bin");
  const destination = path.join(parent, "waltr-desktop");
  const host = `${arch === "x64" ? "x86_64" : "aarch64"}-unknown-linux-gnu`;
  const target = path.join(root, "target/linux-default");
  const source = path.join(target, host, "debug/waltr-app");
  await fs.mkdir(home);
  await fs.mkdir(path.dirname(source), { recursive: true });
  await fs.writeFile(source, linuxElf(machine), { mode: 0o755 });
  const calls = [];
  const logs = [];
  const attributes = new Map();
  const options = {
    platform: "linux", arch, home, root, env: { CARGO_TARGET_DIR: "/wrong", CARGO_BUILD_TARGET: "wasm32-unknown-unknown", WALTR_DEV_SIGNING_IDENTITY: "" },
    log: (message) => logs.push(message),
    runner: (command, args, options) => {
      calls.push({ command, args, options });
      if (command === "rustc") return `rustc x.y.z\nhost: ${host}\n`;
      if (["pnpm", "node", "cargo"].includes(command)) return "";
      assert.equal(command, "python3");
      if (args[0] === "-c") {
        const [, , op, file, value] = args;
        const { dev, ino } = lstatSync(file, { bigint: true });
        const key = `${dev}:${ino}`;
        if (op === "get") return attributes.get(key) ?? "";
        assert.equal(op, "set");
        assert.ok(!attributes.has(key));
        attributes.set(key, value);
        return "";
      }
      assert.equal(args[1], "promote-install");
      if (process.platform === "linux") {
        const result = spawnSync(command, args, { encoding: "utf8" });
        if (result.error) throw result.error;
        if (result.status !== 0) {
          const { code, message } = JSON.parse(result.stderr);
          throw new Error(`${code}: ${message}`);
        }
        return result.stdout;
      }
      const [, , dir, staged, name, , , ...previous] = args;
      const file = path.join(dir, name);
      if (previous.length) {
        const stat = existsSync(file) ? lstatSync(file, { bigint: true }) : null;
        if (destinationIdentity(stat).join() !== previous.join()) throw new Error("Destination changed during build");
      } else if (existsSync(file)) throw new Error("EEXIST");
      renameSync(path.join(dir, staged), file);
      return "";
    },
  };
  if (installed) {
    await fs.writeFile(source, linuxElf(machine, "old"));
    await main([], options);
    await fs.writeFile(source, linuxElf(machine));
    calls.length = 0;
  }
  const clean = async () => assert.deepEqual(await fs.readdir(parent), ["waltr-desktop"]);
  return { home, local, parent, destination, host, target, source, root, calls, logs, options, clean };
}

function destinationIdentity(stat) {
  return stat ? [stat.dev, stat.ino, stat.ctimeNs, stat.mtimeNs, stat.size].map(String) : [];
}

for (const [arch, machine] of [["x64", 62], ["arm64", 183]]) {
  test(`Linux ${arch} dry-run is inert; first install uses a native locked debug build and identical executable`, async (t) => {
    const f = await linuxFixture(t, { arch, machine });
    const fail = () => assert.fail("unexpected dry-run side effect");
    await main(["--dry-run"], { ...f.options, runner: fail, io: new Proxy({}, { get: fail }) });
    assert.match(f.logs.at(-1), /WALTR_WASM_BUILD_LOCKED=1 pnpm/);
    assert.match(f.logs.at(-1), /cargo build --locked.*tauri\/custom-protocol --target <host-triple>/);
    assert.ok(f.logs.at(-1).includes(f.destination));
    await assert.rejects(fs.lstat(f.local), { code: "ENOENT" });
    await main([], f.options);
    assert.deepEqual(f.calls.slice(0, 4).map(({ command, args, options }) => [command, args, options.cwd]), [
      ["rustc", ["-vV"], f.root],
      ["pnpm", ["--filter", "@rfhold/waltr-web", "build:tauri"], f.root],
      ["node", ["scripts/stage-web.mjs"], path.join(f.root, "crates/waltr-app")],
      ["cargo", ["build", "--locked", "-p", "waltr-app", "--bin", "waltr-app", "--features", "tauri/custom-protocol", "--target", f.host], f.root],
    ]);
    assert.equal(f.calls[3].options.env.CARGO_TARGET_DIR, f.target);
    assert.equal(f.calls[3].options.stdio, "inherit");
    assert.equal(f.calls[1].options.env.WALTR_WASM_BUILD_LOCKED, "1");
    assert.ok(f.calls.filter(({ command }) => command !== "pnpm").every(({ options }) => !("WALTR_WASM_BUILD_LOCKED" in options.env)));
    assert.equal(f.calls.at(-1).args[1], "promote-install");
    assert.equal(f.calls.at(-1).args.length, 7);
    assert.deepEqual(await fs.readFile(f.destination), linuxElf(machine));
    assert.equal((await fs.stat(f.destination)).mode & 0o777, 0o755);
    assert.match(f.logs.at(-1), /Waltr was not launched/);
    await f.clean();
  });
}

test("Linux repeats install via one atomic replacement of the marked previous executable", async (t) => {
  const f = await linuxFixture(t, { installed: true });
  let promotions = 0;
  const runner = f.options.runner;
  f.options.runner = (command, args, options) => {
    if (args[1] === "promote-install") {
      promotions++;
      assert.equal(args[2], f.parent);
      assert.equal(args[4], "waltr-desktop");
      assert.deepEqual(readFileSync(f.destination), linuxElf(62, "old"));
      assert.deepEqual(args.slice(7), destinationIdentity(lstatSync(f.destination, { bigint: true })));
    }
    return runner(command, args, options);
  };
  await main([], f.options);
  assert.equal(promotions, 1);
  assert.deepEqual(await fs.readFile(f.destination), linuxElf());
  assert.ok(f.logs.at(-1).includes(createHash("sha256").update(linuxElf()).digest("hex")));
  await f.clean();
});

for (const failure of ["pnpm", "node", "cargo", "source", "source mode", "source symlink", "staged bytes", "marker", "rename"]) {
  test(`Linux ${failure} failure preserves the previous install`, async (t) => {
    const f = await linuxFixture(t, { installed: true });
    const runner = f.options.runner;
    f.options.runner = (command, args, options) => {
      if (command === failure || (failure === "marker" && command === "python3" && args[2] === "set") ||
          (failure === "rename" && args[1] === "promote-install")) throw new Error("fake failure");
      return runner(command, args, options);
    };
    if (failure === "source") await fs.writeFile(f.source, linuxElf(183));
    if (failure === "source mode") await fs.chmod(f.source, 0o644);
    if (failure === "source symlink") {
      await fs.rename(f.source, `${f.source}.real`);
      await fs.symlink(`${f.source}.real`, f.source);
    }
    f.options.io = { ...fs,
      copyFile: async (...args) => {
        await fs.copyFile(...args);
        if (failure === "staged bytes") await fs.writeFile(args[1], linuxElf(62, "different"));
      },
      rename: async () => assert.fail("expected promotion through the helper"),
    };
    await assert.rejects(main([], f.options), /fake failure|invalid native Linux ELF|Staged executable differs/);
    assert.deepEqual(await fs.readFile(f.destination), linuxElf(62, "old"));
    await f.clean();
  });
}

for (const unsafe of ["home symlink", "local symlink", "bin symlink", "writable bin", "destination symlink", "destination directory", "unmarked executable", "hardlink", "in-place edit"]) {
  test(`Linux refuses ${unsafe} before any build`, async (t) => {
    const f = await linuxFixture(t, { installed: ["hardlink", "in-place edit"].includes(unsafe) });
    if (["home symlink", "local symlink", "bin symlink"].includes(unsafe)) {
      const dir = f[unsafe.split(" ")[0] === "bin" ? "parent" : unsafe.split(" ")[0]];
      await fs.mkdir(dir, { recursive: true });
      await fs.rename(dir, `${dir}.real`);
      await fs.symlink(`${dir}.real`, dir);
    } else {
      await fs.mkdir(f.parent, { recursive: true });
      if (unsafe === "writable bin") await fs.chmod(f.parent, 0o777);
      if (unsafe === "destination symlink") await fs.symlink(f.source, f.destination);
      if (unsafe === "destination directory") await fs.mkdir(f.destination);
      if (unsafe === "unmarked executable") await fs.writeFile(f.destination, linuxElf(), { mode: 0o755 });
      if (unsafe === "hardlink") await fs.link(f.destination, `${f.destination}.alias`);
      if (unsafe === "in-place edit") await fs.writeFile(f.destination, linuxElf(62, "unrelated"));
    }
    await assert.rejects(main([], f.options), /Refusing unsafe|unrecognized or modified/);
    assert.ok(!f.calls.some(({ command }) => ["pnpm", "node", "cargo"].includes(command)));
  });
}

test("Linux detects incompatible Rust host before any build", async (t) => {
  const f = await linuxFixture(t, { arch: "arm64", machine: 183 });
  f.options.runner = (command) => {
    assert.equal(command, "rustc");
    return "host: x86_64-unknown-linux-gnu\n";
  };
  await assert.rejects(main([], f.options), /not native Linux arm64/);
});

for (const change of ["new destination", "different inode", "same-inode rewrite", "different bin"]) {
  test(`Linux refuses to overwrite following ${change} during the build`, async (t) => {
    const f = await linuxFixture(t, { installed: change !== "new destination" });
    await fs.mkdir(f.parent, { recursive: true });
    const runner = f.options.runner;
    f.options.runner = (command, ...args) => {
      if (command === "cargo") {
        if (change === "different inode") renameSync(f.destination, `${f.destination}.saved`);
        if (change === "different bin") {
          renameSync(f.parent, `${f.parent}.saved`);
        } else {
          writeFileSync(f.destination, linuxElf(62, "external"), { mode: 0o755 });
        }
      }
      return runner(command, ...args);
    };
    await assert.rejects(main([], f.options), /Destination changed|directory changed|EEXIST/);
    const checked = change === "different bin" ? path.join(`${f.parent}.saved`, "waltr-desktop") : f.destination;
    assert.deepEqual(await fs.readFile(checked), linuxElf(62, change === "different bin" ? "old" : "external"));
  });
}

test("Linux first promotion atomically refuses a destination appearing after the final check", async (t) => {
  const f = await linuxFixture(t);
  const runner = f.options.runner;
  f.options.runner = (command, args, options) => {
    if (command === "python3" && args[1] === "promote-install") writeFileSync(f.destination, "unrelated");
    return runner(command, args, options);
  };
  await assert.rejects(main([], f.options), /EEXIST/);
  assert.equal(await fs.readFile(f.destination, "utf8"), "unrelated");
  await f.clean();
});

test("Linux promotion helper lets only one competing reinstall replace the same snapshot", { skip: process.platform !== "linux" }, async (t) => {
  const f = await linuxFixture(t, { installed: true });
  const parent = await fs.lstat(f.parent, { bigint: true });
  const snapshot = destinationIdentity(await fs.lstat(f.destination, { bigint: true }));
  const candidates = ["candidate A", "candidate B"];
  for (const candidate of candidates) await fs.writeFile(path.join(f.parent, candidate), candidate);
  const results = await Promise.allSettled(candidates.map((candidate) => runFile("python3", [
    directoryHelper, "promote-install", f.parent, candidate, "waltr-desktop", String(parent.dev), String(parent.ino), ...snapshot,
  ])));
  const winner = results.findIndex(({ status }) => status === "fulfilled");
  assert.notEqual(winner, -1);
  const loser = 1 - winner;
  assert.equal(results[loser].status, "rejected");
  assert.match(results[loser].reason.stderr, /ESTALE.*Destination changed/);
  assert.equal(await fs.readFile(f.destination, "utf8"), candidates[winner]);
  assert.deepEqual((await fs.readdir(f.parent)).sort(), [candidates[loser], "waltr-desktop"]);
});
