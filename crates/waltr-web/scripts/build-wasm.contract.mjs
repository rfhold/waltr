import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import {
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  realpath,
  rm,
  symlink,
  unlink,
  utimes,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import nodeTest from "node:test";
import { setTimeout as delay } from "node:timers/promises";

const sourceScript = new URL("./build-wasm.mjs", import.meta.url);
const test = (name, run) => nodeTest(name, { concurrency: true }, run);

test("successful generation removes obsolete legacy snippets", async (context) => {
  const fixture = await createFixture(context);
  await seedLegacyOutput(fixture.generatedRoot);
  await runBuild(fixture);
  assert.match(await readCurrent(fixture), /^[0-9a-f-]{36}:waltr_client\.js$/);
  await assert.rejects(lstat(join(fixture.generatedRoot, "wasm")), { code: "ENOENT" });
});

test("failed generation preserves the current complete version", async (context) => {
  const fixture = await createFixture(context);
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "previous" });
  const target = await realpath(fixture.pointer);
  await assert.rejects(runBuild(fixture, { WALTR_WASM_BUILD_TEST_MODE: "fail-generation" }));
  assert.equal(await realpath(fixture.pointer), target);
  assert.equal(await readCurrent(fixture), "previous:waltr_client.js");
  await assertCleanTransactions(fixture);
});

for (const boundary of [
  "owner-writing-created",
  "owner-candidate-published",
  "transaction-dir-created",
  "owner-linked",
  "output-created",
  "version-finalized",
  "pointer-created",
  "pointer-published",
]) {
  test(`recovers after termination at ${boundary}`, async (context) => {
    const fixture = await createFixture(context);
    await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "previous" });
    const crashed = await runBuild(fixture, {
      WALTR_WASM_BUILD_TEST_CRASH: boundary,
      WALTR_WASM_BUILD_TEST_MARKER: "crashed",
    }, false);
    assert.equal(crashed.signal, "SIGKILL");
    if (boundary === "pointer-published") {
      assert.equal(await readCurrent(fixture), "crashed:waltr_client.js");
    }
    await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "recovered" });
    assert.equal(await readCurrent(fixture), "recovered:waltr_client.js");
    await assertCleanTransactions(fixture);
  });
}

test("portable heartbeat refresh blocks cleanup and lets publishers wait", async (context) => {
  const fixture = await createFixture(context);
  await runBuild(fixture);
  const lock = join(fixture.storeRoot, "publish.lock");
  const heartbeat = join(fixture.storeRoot, "publish.heartbeat");
  const active = runBuild(fixture, {
    WALTR_WASM_BUILD_TEST_HOLD_LOCK_MS: "300",
    WALTR_WASM_BUILD_TEST_MARKER: "active",
    WALTR_WASM_TEST_HEARTBEAT_INTERVAL_MS: "20",
    WALTR_WASM_TEST_IDENTITY: "no-proc",
    WALTR_WASM_TEST_LEASE_MS: "80",
  });
  await waitFor(() => readFile(heartbeat, "utf8"));
  const first = JSON.parse(await readFile(heartbeat, "utf8")).heartbeatAt;
  await waitFor(async () => JSON.parse(await readFile(heartbeat, "utf8")).heartbeatAt > first);
  await assert.rejects(runBuild(fixture, {
    WALTR_WASM_TEST_IDENTITY: "no-proc",
    WALTR_WASM_TEST_LEASE_MS: "80",
  }, true, ["--cleanup"]), /may be active/);
  assert.equal((await lstat(lock)).isFile(), true);
  const waiter = runBuild(fixture, {
    WALTR_WASM_BUILD_TEST_MARKER: "waiter",
    WALTR_WASM_TEST_HEARTBEAT_INTERVAL_MS: "20",
    WALTR_WASM_TEST_IDENTITY: "no-proc",
    WALTR_WASM_TEST_LEASE_MS: "80",
  });
  await active;
  await waiter;
  assert.equal(await readCurrent(fixture), "waiter:waltr_client.js");
  await assertCleanTransactions(fixture);
});

test("portable expired and malformed heartbeats are reclaimed without trusting a live PID", async (context) => {
  const fixture = await createFixture(context);
  await runBuild(fixture);
  const lock = join(fixture.storeRoot, "publish.lock");
  const heartbeat = join(fixture.storeRoot, "publish.heartbeat");
  const portable = currentOwner("portable", Date.now() - 1_000);
  await writeFile(lock, `${JSON.stringify(portable)}\n`);
  await writeFile(heartbeat, `${JSON.stringify({
    schema: 1,
    id: portable.id,
    nonce: portable.nonce,
    heartbeatAt: Date.now() - 1_000,
  })}\n`);
  await runBuild(fixture, {
    WALTR_WASM_TEST_IDENTITY: "no-proc",
    WALTR_WASM_TEST_LEASE_MS: "80",
    WALTR_WASM_TEST_PID_STATE: "reused",
  });

  await writeFile(lock, `${JSON.stringify(currentOwner("portable"))}\n`);
  const stale = new Date(Date.now() - 300_000);
  await writeFile(heartbeat, "malformed");
  await utimes(heartbeat, stale, stale);
  await runBuild(fixture, { WALTR_WASM_TEST_IDENTITY: "no-proc", WALTR_WASM_TEST_LEASE_MS: "80" });
  const linux = currentOwner("linux");
  await writeFile(lock, `${JSON.stringify(linux)}\n`);
  await runBuild(fixture, { WALTR_WASM_TEST_PID_STATE: "reused" });
  await assertCleanTransactions(fixture);
});

test("a stopped portable heartbeat cannot overwrite or release a replacement lock", async (context) => {
  const fixture = await createFixture(context);
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "old" });
  const lock = join(fixture.storeRoot, "publish.lock");
  const heartbeat = join(fixture.storeRoot, "publish.heartbeat");
  const active = runBuild(fixture, {
    WALTR_WASM_BUILD_TEST_HOLD_LOCK_MS: "250",
    WALTR_WASM_TEST_HEARTBEAT_INTERVAL_MS: "20",
    WALTR_WASM_TEST_IDENTITY: "no-proc",
    WALTR_WASM_TEST_LEASE_MS: "80",
  }, false);
  await waitFor(() => readFile(heartbeat, "utf8"));
  await unlink(lock);
  const replacement = currentOwner("portable");
  await writeFile(lock, `${JSON.stringify(replacement)}\n`);
  const result = await active;
  assert.notEqual(result.code, 0);
  assert.match(result.stderr, /lock ownership was lost/);
  assert.deepEqual(JSON.parse(await readFile(lock, "utf8")), replacement);
  await unlink(lock);
  await unlink(heartbeat);
  await assertCleanTransactions(fixture);
});

test("many publishers atomically share stale transaction recovery", async (context) => {
  const fixture = await createFixture(context);
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "old" });
  const crashed = await runBuild(fixture, {
    WALTR_WASM_BUILD_TEST_CRASH: "pointer-created",
    WALTR_WASM_BUILD_TEST_MARKER: "dead",
  }, false);
  assert.equal(crashed.signal, "SIGKILL");
  await Promise.all(Array.from({ length: 16 }, (_, index) => runBuild(fixture, {
    WALTR_WASM_BUILD_TEST_MARKER: `publisher-${index}`,
  })));
  assert.match(await readCurrent(fixture), /^publisher-[0-9]+:waltr_client\.js$/);
  await assertCleanTransactions(fixture);
});

test("recovery claims but never removes an artifact replaced after discovery", async (context) => {
  const fixture = await createFixture(context);
  const crashed = await runBuild(fixture, {
    WALTR_WASM_BUILD_TEST_CRASH: "owner-writing-created",
  }, false);
  assert.equal(crashed.signal, "SIGKILL");
  const transactions = join(fixture.storeRoot, "transactions");
  const ownerName = (await readdir(transactions)).find((name) => name.startsWith(".owner-writing-"));
  assert.ok(ownerName);
  const ownerPath = join(transactions, ownerName);
  const recovering = runBuild(fixture, { WALTR_WASM_BUILD_TEST_RECOVERY_DELAY_MS: "200" }, false);
  await waitFor(async () => (await readdir(join(fixture.storeRoot, "recovery"))).length > 0);
  await unlink(ownerPath);
  await writeFile(ownerPath, "hostile replacement");
  const result = await recovering;
  assert.notEqual(result.code, 0);
  const claimName = (await readdir(join(fixture.storeRoot, "recovery")))[0];
  const claim = join(fixture.storeRoot, "recovery", claimName);
  assert.equal(await readFile(join(claim, "owner-0"), "utf8"), "hostile replacement");
  await assert.rejects(lstat(ownerPath), { code: "ENOENT" });
});

test("unchanged generations reuse content versions and cleanup preserves current", async (context) => {
  const fixture = await createFixture(context);
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "same" });
  const firstTarget = await realpath(fixture.pointer);
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "same" });
  assert.equal(await realpath(fixture.pointer), firstTarget);
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "different" });
  const currentTarget = await realpath(fixture.pointer);
  assert.notEqual(currentTarget, firstTarget);
  assert.equal((await readdir(join(fixture.storeRoot, "versions"))).length, 2);
  await runBuild(fixture, {}, true, ["--cleanup"]);
  assert.deepEqual(new Set(await readdir(join(fixture.storeRoot, "versions"))), new Set([basename(firstTarget), basename(currentTarget)]));
  assert.equal(await readCurrent(fixture), "different:waltr_client.js");
});

test("cleanup preserves resolved readers through grace and minimum history", async (context) => {
  const fixture = await createFixture(context);
  const start = 1_800_000_000_000;
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "v0", WALTR_WASM_BUILD_TEST_NOW_MS: String(start) });
  const oldTarget = await realpath(fixture.pointer);
  for (let index = 1; index <= 3; index += 1) {
    await runBuild(fixture, {
      WALTR_WASM_BUILD_TEST_MARKER: `v${index}`,
      WALTR_WASM_BUILD_TEST_NOW_MS: String(start + index),
    });
  }
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_NOW_MS: String(start + 4) }, true, ["--cleanup"]);
  for (let read = 0; read < 100; read += 1) {
    assert.equal(await readFile(join(oldTarget, "waltr_client.js"), "utf8"), "v0:waltr_client.js");
  }
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_NOW_MS: String(start + 300_001) }, true, ["--cleanup"]);
  await assert.rejects(lstat(oldTarget), { code: "ENOENT" });
  assert.equal((await readdir(join(fixture.storeRoot, "versions"))).length, 3);
  assert.equal(await readCurrent(fixture), "v3:waltr_client.js");
});

test("cleanup validates every version before removing any", async (context) => {
  const fixture = await createFixture(context);
  const start = 1_800_000_000_000;
  for (let index = 0; index <= 3; index += 1) {
    await runBuild(fixture, {
      WALTR_WASM_BUILD_TEST_MARKER: `v${index}`,
      WALTR_WASM_BUILD_TEST_NOW_MS: String(start + index),
    });
  }
  const versions = join(fixture.storeRoot, "versions");
  const before = await readdir(versions);
  const oldest = await versionByPublication(versions, start);
  await writeFile(join(oldest, ".waltr-wasm-version.json"), "malformed");
  await assert.rejects(runBuild(fixture, {
    WALTR_WASM_BUILD_TEST_NOW_MS: String(start + 300_001),
  }, true, ["--cleanup"]), /invalid content version manifest/);
  assert.deepEqual(await readdir(versions), before);
});

test("pointer failures preserve old pointer, temporary cleanup, and resolved readers", async (context) => {
  const fixture = await createFixture(context);
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "old" });
  const oldTarget = await realpath(fixture.pointer);
  for (const failure of ["pointer-create", "pointer-rename"]) {
    await assert.rejects(runBuild(fixture, {
      WALTR_WASM_BUILD_TEST_FAIL: failure,
      WALTR_WASM_BUILD_TEST_MARKER: failure,
    }));
    assert.equal(await realpath(fixture.pointer), oldTarget);
    assert.equal(await readFile(join(oldTarget, "waltr_client.js"), "utf8"), "old:waltr_client.js");
    await assertCleanTransactions(fixture);
  }
  await runBuild(fixture, { WALTR_WASM_BUILD_TEST_MARKER: "new" });
  assert.equal(await readFile(join(oldTarget, "waltr_client.js"), "utf8"), "old:waltr_client.js");
});

test("concurrent writers publish only complete versions", async (context) => {
  const fixture = await createFixture(context);
  await Promise.all(Array.from({ length: 4 }, () => runBuild(fixture)));
  assert.match(await readCurrent(fixture), /:waltr_client\.js$/);
  await assertCleanTransactions(fixture);
});

test("continuous readers never observe missing or mixed output", async (context) => {
  const fixture = await createFixture(context);
  await runBuild(fixture);
  const failures = [];
  let reading = true;
  const reader = (async () => {
    while (reading) {
      try {
        const target = await realpath(fixture.pointer);
        const files = await Promise.all([
          readFile(join(target, "waltr_client.js"), "utf8"),
          readFile(join(target, "waltr_client_bg.wasm.d.ts"), "utf8"),
          readFile(join(target, "snippets/current/inline0.js"), "utf8"),
        ]);
        const markers = files.map((value) => value.split(":")[0]);
        if (!markers.every((marker) => marker === markers[0])) failures.push(files);
      } catch (error) {
        failures.push(error);
      }
      await delay(1);
    }
  })();
  let buildFailure;
  try {
    await Promise.all(Array.from({ length: 8 }, () => runBuild(fixture)));
  } catch (error) {
    buildFailure = error;
  } finally {
    reading = false;
    await reader;
  }
  if (buildFailure) throw buildFailure;
  assert.deepEqual(failures, []);
});

test("rejects symlinked generated roots and external output pointers", async (context) => {
  const fixture = await createFixture(context);
  const external = join(fixture.root, "external");
  await mkdir(external);
  await rm(fixture.generatedRoot, { recursive: true });
  await symlink(external, fixture.generatedRoot, "dir");
  await assert.rejects(runBuild(fixture), /untrusted generated path ancestor/);
  assert.equal((await readdir(external)).length, 0);

  await unlink(fixture.generatedRoot);
  await mkdir(fixture.generatedRoot);
  await symlink(external, fixture.pointer, "dir");
  await assert.rejects(runBuild(fixture), /trusted generated root|owned symbolic link/);
  assert.equal((await readdir(external)).length, 0);
});

test("rejects traversal and absolute output arguments", async (context) => {
  const fixture = await createFixture(context);
  const sentinel = join(fixture.root, "sentinel");
  await writeFile(sentinel, "safe");
  await assert.rejects(runBuild(fixture, {}, true, ["../outside"]), /accepts only/);
  await assert.rejects(runBuild(fixture, {}, true, [join(fixture.root, "outside")]), /accepts only/);
  assert.equal(await readFile(sentinel, "utf8"), "safe");
});

test("fails closed after generated ancestor replacement", async (context) => {
  const fixture = await createFixture(context);
  await runBuild(fixture);
  const external = join(fixture.root, "external");
  await mkdir(external);
  await rm(fixture.generatedRoot, { recursive: true });
  await symlink(external, fixture.generatedRoot, "dir");
  await assert.rejects(runBuild(fixture), /untrusted generated path ancestor/);
  assert.equal((await readdir(external)).length, 0);
});

test("hostile legacy and backup names never delete external sentinels", async (context) => {
  const fixture = await createFixture(context);
  const external = join(fixture.root, "external");
  await mkdir(external);
  const sentinel = join(external, "sentinel");
  await writeFile(sentinel, "safe");
  await mkdir(join(fixture.generatedRoot, "wasm"), { recursive: true });
  await symlink(external, join(fixture.generatedRoot, "wasm/hostile"), "dir");
  await symlink(external, join(fixture.generatedRoot, ".wasm-backup-hostile"), "dir");
  await assert.rejects(runBuild(fixture), /untrusted legacy WASM artifact/);
  assert.equal(await readFile(sentinel, "utf8"), "safe");
  assert.match(await readCurrent(fixture), /:waltr_client\.js$/);
  await assertCleanTransactions(fixture);
});

async function createFixture(context) {
  const root = await mkdtemp(join(tmpdir(), "waltr-wasm-contract-"));
  context.after(() => rm(root, { recursive: true, force: true }));
  const webRoot = join(root, "repo/crates/waltr-web");
  const script = join(webRoot, "scripts/build-wasm.mjs");
  await mkdir(dirname(script), { recursive: true });
  await copyFile(sourceScript, script);
  await mkdir(join(webRoot, "generated"));
  return {
    root,
    script,
    generatedRoot: join(webRoot, "generated"),
    pointer: join(webRoot, "generated/wasm-current"),
    storeRoot: join(webRoot, "generated/.wasm-store"),
  };
}

async function runBuild(fixture, env = {}, expectSuccess = true, args = []) {
  const result = await new Promise((resolveRun) => {
    let stderr = "";
    const child = spawn(process.execPath, [fixture.script, ...args], {
      env: { ...process.env, WALTR_WASM_BUILD_TEST_MODE: "success", ...env },
      stdio: ["ignore", "ignore", "pipe"],
    });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.once("exit", (code, signal) => resolveRun({ code, signal, stderr }));
  });
  if (expectSuccess && result.code !== 0) throw new Error(result.stderr || `fixture build failed: ${JSON.stringify(result)}`);
  if (!expectSuccess) return result;
}

async function seedLegacyOutput(generatedRoot) {
  const output = join(generatedRoot, "wasm");
  await mkdir(join(output, "snippets/obsolete"), { recursive: true });
  for (const file of ["waltr_client.js", "waltr_client.d.ts", "waltr_client_bg.wasm", "waltr_client_bg.wasm.d.ts"]) {
    await writeFile(join(output, file), "previous");
  }
  await writeFile(join(output, "snippets/obsolete/inline0.js"), "obsolete");
}

async function readCurrent(fixture) {
  return readFile(join(await realpath(fixture.pointer), "waltr_client.js"), "utf8");
}

async function assertCleanTransactions(fixture) {
  assert.deepEqual(await readdir(join(fixture.storeRoot, "transactions")), []);
  assert.deepEqual(await readdir(join(fixture.storeRoot, "recovery")), []);
  await assert.rejects(lstat(join(fixture.storeRoot, "publish.lock")), { code: "ENOENT" });
  await assert.rejects(lstat(join(fixture.storeRoot, "publish.heartbeat")), { code: "ENOENT" });
  const generatedEntries = await readdir(fixture.generatedRoot);
  assert.equal(generatedEntries.some((name) => name.startsWith(".wasm-pointer-")), false);
}

async function waitFor(operation) {
  const deadline = Date.now() + 2_000;
  while (true) {
    try {
      const result = await operation();
      if (result) return result;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    if (Date.now() >= deadline) throw new Error("timed out waiting for fixture state");
    await delay(10);
  }
}

async function versionByPublication(versions, publishedAt) {
  for (const name of await readdir(versions)) {
    const version = join(versions, name);
    const manifest = JSON.parse(await readFile(join(version, ".waltr-wasm-version.json"), "utf8"));
    if (manifest.publishedAt === publishedAt) return version;
  }
  throw new Error(`missing version published at ${publishedAt}`);
}

function currentOwner(kind, createdAt = Date.now()) {
  return {
    schema: 2,
    id: "00000000-0000-4000-8000-000000000001",
    nonce: "00000000-0000-4000-8000-000000000002",
    pid: process.pid,
    createdAt,
    leaseUntil: createdAt + 120_000,
    identity: kind === "linux"
      ? { kind, bootId: "boot", processStart: "start" }
      : { kind },
  };
}
