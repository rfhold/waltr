import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import {
  constants as fsConstants,
  chmodSync,
  existsSync,
  linkSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  renameSync,
  rmSync,
  statSync,
  symlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { test } from "node:test";

import {
  acquireScreenshotLock,
  buildRunPlan,
  cleanEnvironment,
  cleanupHarness,
  createHarnessRuntime,
  createRunContext,
  createScreenshotLockOwner,
  executeComposeHarness,
  executeWithCleanup,
  exitCodeFor,
  integrationPhases,
  parseCliArgs,
  profileForPhase,
  releaseScreenshotLock,
  requestSignalStop,
  runCli,
  runCommandResult,
  runDevK6,
  screenshotPathsForRun,
  spawnRecord,
  stopChildren,
  stopCompose,
  validateScreenshots,
} from "./test-web-integration.mjs";

test("CLI defaults select both static Compose phases", () => {
  assert.deepEqual(parseCliArgs([]), {
    mode: "compose",
    access: "both",
    live: false,
    screenshots: false,
    updateScreenshots: false,
    script: null,
  });
});

test("CLI accepts every explicit flag", () => {
  assert.deepEqual(parseCliArgs([
    "--mode",
    "dev",
    "--access",
    "disabled",
    "--live",
    "--screenshots",
    "--update-screenshots",
    "--script",
    "sessions/session-live.browser.js",
  ]), {
    mode: "dev",
    access: "disabled",
    live: true,
    screenshots: true,
    updateScreenshots: true,
    script: "sessions/session-live.browser.js",
  });
});

test("CLI rejects invalid or incomplete selections before a run can start", () => {
  assert.throws(() => parseCliArgs(["--mode", "other"]), /--mode must be compose or dev/);
  assert.throws(() => parseCliArgs(["--access", "other"]), /--access must be/);
  assert.throws(() => parseCliArgs(["--mode", "dev", "--access", "oidc"]), /requires --access disabled/);
  assert.throws(() => parseCliArgs(["--script"]), /requires a value/);
  assert.throws(() => parseCliArgs(["--screenshots", "--screenshots"]), /only be provided once/);
  assert.throws(() => parseCliArgs(["--update-screenshots"]), /requires --screenshots/);
  assert.throws(() => parseCliArgs(["unexpected"]), /unknown web integration argument/);
});

test("invalid CLI input is rejected before context allocation", async () => {
  let contextAllocations = 0;
  const messages = [];
  const exitCode = await runCli(["--mode", "invalid"], {
    createContext: () => {
      contextAllocations += 1;
      throw new Error("context should not be allocated");
    },
    logger: { error: (message) => messages.push(message) },
  });
  assert.equal(exitCode, 1);
  assert.equal(contextAllocations, 0);
  assert.match(messages[0], /--mode must be compose or dev/);
});

test("cleanEnvironment strips all harness-controlled prefixes", () => {
  assert.deepEqual(cleanEnvironment({
    PATH: "/bin",
    HOME: "/tmp/home",
    DOCKER_HOST: "unix:///docker.sock",
    WALTR_DATABASE_URL: "postgres://production",
    WALTR_AUTH_MODE: "oidc",
    K6_INSECURE_SKIP_TLS_VERIFY: "true",
    COMPOSE_PROFILES: "production",
    COMPOSE_PROJECT_NAME: "collision",
  }), {
    PATH: "/bin",
    HOME: "/tmp/home",
    DOCKER_HOST: "unix:///docker.sock",
  });
});

test("phase and profile mapping is exact", () => {
  assert.deepEqual(integrationPhases("compose", "both"), ["disabled", "oidc"]);
  assert.deepEqual(integrationPhases("compose", "disabled"), ["disabled"]);
  assert.deepEqual(integrationPhases("compose", "oidc"), ["oidc"]);
  assert.deepEqual(integrationPhases("dev", "disabled"), ["disabled"]);
  assert.deepEqual(profileForPhase("disabled"), {
    profile: "test",
    waltrService: "waltr-test",
    k6Service: "k6",
    upServices: ["waltr-test"],
  });
  assert.deepEqual(profileForPhase("oidc"), {
    profile: "test-auth",
    waltrService: "waltr-auth",
    k6Service: "k6-auth",
    upServices: ["waltr-auth", "mcp-auth"],
  });
});

test("script selection only schedules phases where the script exists", () => {
  const options = parseCliArgs([
    "--mode",
    "compose",
    "--access",
    "both",
    "--script",
    "access/login.browser.js",
  ]);
  const plan = buildRunPlan(options);
  assert.equal(plan.length, 1);
  assert.equal(plan[0].profile, "test-auth");
  assert.deepEqual(plan[0].scripts, ["tests/web/k6/access/login.browser.js"]);
  assert.throws(
    () => buildRunPlan({ ...options, script: "missing.browser.js" }),
    /selected web integration script is not enabled/,
  );
  const trustPlan = buildRunPlan(parseCliArgs([
    "--access",
    "oidc",
    "--script",
    "access/trust.browser.js",
  ]));
  assert.deepEqual(trustPlan[0].scripts, ["tests/web/k6/access/trust.browser.js"]);

  const oidcPlan = buildRunPlan(parseCliArgs(["--access", "oidc"]));
  assert.equal(
    oidcPlan[0].scripts.includes("tests/web/k6/sessions/mcp-oauth.browser.js"),
    false,
  );
  assert.throws(
    () => buildRunPlan(parseCliArgs([
      "--access",
      "oidc",
      "--script",
      "sessions/mcp-oauth.browser.js",
    ])),
    /selected web integration script is not enabled/,
  );
});

test("exit code precedence preserves signals and test failures", () => {
  assert.equal(exitCodeFor({}), 0);
  assert.equal(exitCodeFor({ cleanupFailed: true }), 1);
  assert.equal(exitCodeFor({ testExitCode: 99, cleanupFailed: true }), 99);
  assert.equal(exitCodeFor({ testExitCode: 99, cleanupFailed: true, signalCode: 130 }), 130);
  assert.equal(exitCodeFor({ signalCode: 143 }), 143);
});

test("cleanup runs once even when requested during execution and in finally", async () => {
  let cleanupCalls = 0;
  const result = await executeWithCleanup({
    execute: async ({ cleanup }) => {
      await cleanup();
      await cleanup();
      return 0;
    },
    cleanup: async () => {
      cleanupCalls += 1;
      return true;
    },
  });
  assert.equal(cleanupCalls, 1);
  assert.equal(result.exitCode, 0);
});

test("startup failure is nonzero and still cleans up", async () => {
  let cleanupCalls = 0;
  const result = await executeWithCleanup({
    execute: async () => {
      throw new Error("startup failed");
    },
    cleanup: async () => {
      cleanupCalls += 1;
      return true;
    },
  });
  assert.equal(cleanupCalls, 1);
  assert.match(result.executionError.message, /startup failed/);
  assert.equal(result.exitCode, 1);
});

test("k6 failure takes precedence over teardown failure", async () => {
  const result = await executeWithCleanup({
    execute: async () => ({ testExitCode: 99 }),
    cleanup: async () => {
      throw new Error("teardown failed");
    },
  });
  assert.equal(result.cleanupFailed, true);
  assert.match(result.cleanupError.message, /teardown failed/);
  assert.equal(result.exitCode, 99);
});

test("teardown failure turns an otherwise successful run nonzero", async () => {
  const result = await executeWithCleanup({
    execute: async () => 0,
    cleanup: async () => false,
  });
  assert.equal(result.cleanupFailed, true);
  assert.equal(result.exitCode, 1);
});

test("controlled signal codes survive cleanup failures", async () => {
  const result = await executeWithCleanup({
    execute: async () => ({ testExitCode: 1 }),
    cleanup: async () => false,
    getSignalCode: () => 130,
  });
  assert.equal(result.exitCode, 130);
});

test("signals received during context allocation are applied before execution", async () => {
  const signalProcess = new EventEmitter();
  const state = testState();
  let cleanupCalls = 0;
  let executeCalls = 0;

  const exitCode = await runCli(["--access", "disabled"], {
    signalProcess,
    logger: silentLogger,
    createContext: () => {
      assert.equal(signalProcess.listenerCount("SIGTERM"), 1);
      signalProcess.emit("SIGTERM");
      return state;
    },
    execute: async (runState) => {
      executeCalls += 1;
      assert.equal(runState.signalCode, 143);
      assert.equal(runState.abortController.signal.aborted, true);
      return 0;
    },
    cleanup: async () => {
      cleanupCalls += 1;
      return true;
    },
  });

  assert.equal(exitCode, 143);
  assert.equal(executeCalls, 1);
  assert.equal(cleanupCalls, 1);
  assert.equal(signalProcess.listenerCount("SIGINT"), 0);
  assert.equal(signalProcess.listenerCount("SIGTERM"), 0);
});

test("screenshot lock uses process identity and safely recovers stale metadata", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-lock-contract-"));
  const lockPath = join(root, "screenshots.lock");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const runtime = createHarnessRuntime({ logger: silentLogger });
  const firstOwner = createScreenshotLockOwner("first", runtime);
  const secondOwner = createScreenshotLockOwner("second", runtime);
  const first = acquireScreenshotLock(lockPath, firstOwner, runtime);

  assert.throws(
    () => acquireScreenshotLock(lockPath, secondOwner, runtime),
    /first already holds/,
  );
  assert.equal(releaseScreenshotLock({ path: lockPath, owner: secondOwner }, runtime), false);
  assert.equal(existsSync(lockPath), true);
  assert.equal(releaseScreenshotLock(first, runtime), true);
  assert.equal(existsSync(lockPath), false);

  const deadOwner = {
    ...createScreenshotLockOwner("stale", runtime),
    pid: 999_999,
    processStartIdentity: "dead-process",
  };
  writeFileSync(lockPath, `${JSON.stringify(deadOwner)}\n`);
  const defaultIdentity = runtime.getProcessIdentity;
  const staleRuntime = createHarnessRuntime({
    getProcessIdentity: (pid) => pid === deadOwner.pid ? null : defaultIdentity(pid),
    logger: silentLogger,
  });
  const recoveredOwner = createScreenshotLockOwner("recovered", staleRuntime);
  const recovered = acquireScreenshotLock(lockPath, recoveredOwner, staleRuntime);
  assert.equal(JSON.parse(readFileSync(lockPath, "utf8")).runId, "recovered");
  assert.equal(releaseScreenshotLock(recovered, staleRuntime), true);

  writeFileSync(lockPath, "{\"pid\":");
  assert.throws(() => acquireScreenshotLock(lockPath, secondOwner, runtime), /metadata .* incomplete/);
  utimesSync(lockPath, new Date(Date.now() - 20_000), new Date(Date.now() - 20_000));
  const malformedRecovered = acquireScreenshotLock(lockPath, secondOwner, runtime);
  assert.equal(JSON.parse(readFileSync(lockPath, "utf8")).runId, "second");
  assert.equal(releaseScreenshotLock(malformedRecovered, runtime), true);

  const reusedOwner = { ...firstOwner, runId: "reused", token: "reused-owner-token", processStartIdentity: "old-start" };
  writeFileSync(lockPath, `${JSON.stringify(reusedOwner)}\n`);
  const reusedRuntime = createHarnessRuntime({
    getProcessIdentity: () => ({
      pid: process.pid,
      processGroupId: process.pid,
      sessionId: process.pid,
      startIdentity: "new-start",
    }),
    logger: silentLogger,
  });
  const afterReuseOwner = createScreenshotLockOwner("after-reuse", reusedRuntime);
  const afterReuse = acquireScreenshotLock(lockPath, afterReuseOwner, reusedRuntime);
  assert.equal(JSON.parse(readFileSync(lockPath, "utf8")).runId, "after-reuse");
  assert.equal(releaseScreenshotLock(afterReuse, reusedRuntime), true);

  const failedPath = join(root, "failed.lock");
  const failedRuntime = createHarnessRuntime({
    logger: silentLogger,
    writeFileSync() {
      throw new Error("lock write failed");
    },
  });
  const failedOwner = createScreenshotLockOwner("failed", failedRuntime);
  assert.throws(
    () => acquireScreenshotLock(failedPath, failedOwner, failedRuntime),
    /lock write failed/,
  );
  assert.equal(existsSync(failedPath), false);
});

test("screenshot lock release cannot delete a replacement owner", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-lock-race-"));
  const lockPath = join(root, "screenshots.lock");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const baseRuntime = createHarnessRuntime({ logger: silentLogger });
  const originalOwner = createScreenshotLockOwner("original", baseRuntime);
  const replacementOwner = createScreenshotLockOwner("replacement", baseRuntime);
  const contenderOwner = createScreenshotLockOwner("contender", baseRuntime);
  const lock = acquireScreenshotLock(lockPath, originalOwner, baseRuntime);
  let replaced = false;
  const raceRuntime = createHarnessRuntime({
    logger: silentLogger,
    renameNoReplaceSync(source, target) {
      if (!replaced && source === lockPath) {
        replaced = true;
        rmSync(lockPath, { force: true });
        writeFileSync(lockPath, `${JSON.stringify(replacementOwner)}\n`);
        renameSync(source, target);
        writeFileSync(lockPath, `${JSON.stringify(contenderOwner)}\n`);
        return;
      }
      renameSync(source, target);
    },
  });

  assert.equal(releaseScreenshotLock(lock, raceRuntime), false);
  assert.equal(JSON.parse(readFileSync(lockPath, "utf8")).token, contenderOwner.token);
});

test("a stalled screenshot lock writer cannot become a second owner after stale time", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-lock-publication-race-"));
  const lockPath = join(root, "screenshots.lock");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  let now = 1_000;
  let outerWrite = true;
  let contenderLock;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    now: () => now,
    screenshotLockStaleMs: 10_000,
    writeFileSync(target, contents) {
      writeFileSync(target, contents);
      if (outerWrite) {
        outerWrite = false;
        now += 20_000;
        contenderLock = acquireScreenshotLock(lockPath, contenderOwner, runtime);
      }
    },
  });
  const stalledOwner = createScreenshotLockOwner("stalled", runtime);
  const contenderOwner = createScreenshotLockOwner("contender", runtime);

  assert.throws(
    () => acquireScreenshotLock(lockPath, stalledOwner, runtime),
    /contender already holds/,
  );
  assert.equal(JSON.parse(readFileSync(lockPath, "utf8")).token, contenderOwner.token);
  assert.deepEqual(
    readdirSync(root).filter((entry) => entry.includes(".pending-")),
    [],
    "the stalled writer must not leave a publishable candidate",
  );
  assert.equal(releaseScreenshotLock(contenderLock, runtime), true);
});

test("screenshot lock publication rolls back when linking reports a late failure", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-lock-late-link-failure-"));
  const lockPath = join(root, "screenshots.lock");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  let linked = false;
  const failingRuntime = createHarnessRuntime({
    logger: silentLogger,
    linkSync(source, target) {
      linkSync(source, target);
      linked = true;
      throw new Error("lock link failed after publication");
    },
  });
  const failedOwner = createScreenshotLockOwner("failed", failingRuntime);

  assert.throws(
    () => acquireScreenshotLock(lockPath, failedOwner, failingRuntime),
    /lock link failed after publication/,
  );
  assert.equal(linked, true);
  assert.equal(existsSync(lockPath), false);
  assert.deepEqual(readdirSync(root), []);

  const runtime = createHarnessRuntime({ logger: silentLogger });
  const contender = acquireScreenshotLock(
    lockPath,
    createScreenshotLockOwner("contender", runtime),
    runtime,
  );
  assert.equal(releaseScreenshotLock(contender, runtime), true);
});

test("a future lock acquisition recovers a stale pending publication artifact", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-stale-pending-lock-"));
  const lockPath = join(root, "screenshots.lock");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const failingRuntime = createHarnessRuntime({
    logger: silentLogger,
    rmSync(path, options) {
      if (path.includes(".pending-")) {
        throw new Error("pending cleanup failed");
      }
      rmSync(path, options);
    },
  });
  const failedOwner = createScreenshotLockOwner("failed-pending", failingRuntime);

  assert.throws(
    () => acquireScreenshotLock(lockPath, failedOwner, failingRuntime),
    /pending cleanup failed.*screenshot lock acquisition rollback/,
  );
  assert.equal(existsSync(lockPath), false);
  const pending = readdirSync(root).find((entry) => entry.includes(".pending-"));
  assert.ok(pending);
  utimesSync(join(root, pending), new Date(Date.now() - 20_000), new Date(Date.now() - 20_000));

  const runtime = createHarnessRuntime({ logger: silentLogger });
  const contender = acquireScreenshotLock(
    lockPath,
    createScreenshotLockOwner("contender", runtime),
    runtime,
  );
  assert.deepEqual(readdirSync(root).filter((entry) => entry.includes(".pending-")), []);
  assert.equal(releaseScreenshotLock(contender, runtime), true);
});

test("screenshot setup refuses a runs symlink without touching its external target", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-runs-symlink-"));
  const tempRoot = join(root, "tmp");
  const screenshotsRoot = join(root, "screenshots");
  const externalRoot = join(root, "external");
  const sentinel = join(externalRoot, "sentinel.txt");
  mkdirSync(tempRoot);
  mkdirSync(screenshotsRoot);
  mkdirSync(externalRoot);
  writeFileSync(sentinel, "must survive");
  symlinkSync(externalRoot, join(screenshotsRoot, "runs"), "dir");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  let allocatedRoot;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
    mkdtempSync(prefix) {
      allocatedRoot = mkdtempSync(prefix);
      return allocatedRoot;
    },
  });

  assert.throws(
    () => createRunContext(screenshotOptions(), [], runtime),
    /refusing symlinked or non-directory screenshot runs root/,
  );
  assert.equal(readFileSync(sentinel, "utf8"), "must survive");
  assert.equal(existsSync(join(screenshotsRoot, ".lock")), false);
  assert.equal(existsSync(allocatedRoot), false);
});

test("screenshot-enabled context creates a run directory and releases its lock on signal cleanup", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-context-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const tempRoot = join(root, "tmp");
  mkdirSync(tempRoot);
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: join(root, "screenshots"),
    tempRoot,
  });
  const options = screenshotOptions();
  const state = createRunContext(options, [], runtime);
  const paths = screenshotPathsForRun(state.runId, runtime);

  assert.equal(state.screenshotsDir, paths.runDir);
  assert.equal(state.screenshotCurrentPath, paths.currentPath);
  assert.equal(readlinkSync(paths.currentPath), paths.currentTarget);
  assert.equal(statSync(paths.runDir).mode & 0o777, 0o777);
  const owner = JSON.parse(readFileSync(paths.lockPath, "utf8"));
  assert.equal(owner.runId, state.runId);
  assert.equal(owner.processStartIdentity, runtime.getProcessIdentity(process.pid).startIdentity);
  assert.equal(Number.isFinite(owner.createdAtMs), true);

  requestSignalStop(state, 130, "SIGINT");
  assert.equal(await cleanupHarness(state), true);
  assert.equal(existsSync(paths.lockPath), false);
  assert.equal(existsSync(state.runRoot), false);
  assert.equal(existsSync(paths.runDir), true);
  assert.equal(readlinkSync(paths.currentPath), paths.currentTarget);
});

test("partial screenshot context failure releases ownership and temporary paths", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-context-failure-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const tempRoot = join(root, "tmp");
  const screenshotsRoot = join(root, "screenshots");
  mkdirSync(tempRoot);
  let allocatedRoot;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
    mkdtempSync(prefix) {
      allocatedRoot = mkdtempSync(prefix);
      return allocatedRoot;
    },
    chmodSync(path, mode) {
      if (path.includes("/runs/")) {
        throw new Error("screenshot setup failed");
      }
      chmodSync(path, mode);
    },
  });
  const options = screenshotOptions();

  assert.throws(() => createRunContext(options, [], runtime), /screenshot setup failed/);
  assert.equal(existsSync(allocatedRoot), false);
  assert.equal(existsSync(`${screenshotsRoot}/.lock`), false);
  assert.equal(existsSync(`${screenshotsRoot}/current`), false);
  assert.deepEqual(readdirSync(`${screenshotsRoot}/runs`), []);
});

test("partial screenshot context cleanup removes runRoot despite lock inspection failure", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-partial-lock-failure-"));
  const tempRoot = join(root, "tmp");
  const screenshotsRoot = join(root, "screenshots");
  const lockPath = join(screenshotsRoot, ".lock");
  mkdirSync(tempRoot);
  context.after(() => rmSync(root, { force: true, recursive: true }));
  let allocatedRoot;
  let setupFailed = false;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
    mkdtempSync(prefix) {
      allocatedRoot = mkdtempSync(prefix);
      return allocatedRoot;
    },
    chmodSync(path, mode) {
      if (path.includes("/runs/")) {
        setupFailed = true;
        throw new Error("screenshot setup failed");
      }
      chmodSync(path, mode);
    },
    lstatSync(path) {
      if (setupFailed && path === lockPath) {
        throw new Error("lock inspection failed");
      }
      return lstatSync(path);
    },
  });

  assert.throws(
    () => createRunContext(screenshotOptions(), [], runtime),
    /screenshot setup failed.*lock inspection failed/,
  );
  assert.equal(existsSync(allocatedRoot), false);
  assert.equal(existsSync(lockPath), true, "an unverified lock must not be deleted");
});

test("screenshot setup restores the prior current link when pruning fails after replacement", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-current-rollback-"));
  const tempRoot = join(root, "tmp");
  const screenshotsRoot = join(root, "screenshots");
  mkdirSync(tempRoot);
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
  });

  const first = createRunContext(screenshotOptions(), [], runtime);
  assert.equal(await cleanupHarness(first), true);
  const second = createRunContext(screenshotOptions(), [], runtime);
  const secondPaths = screenshotPathsForRun(second.runId, runtime);
  const sentinel = join(secondPaths.runDir, "complete.txt");
  writeFileSync(sentinel, "previous run is complete");
  assert.equal(await cleanupHarness(second), true);
  assert.equal(readlinkSync(secondPaths.currentPath), secondPaths.currentTarget);

  let pruneFailed = false;
  const failingRuntime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
    openSync(path, flags, mode) {
      if (!pruneFailed && basename(path).startsWith(".prune-") && (flags & fsConstants.O_DIRECTORY)) {
        pruneFailed = true;
        throw new Error("screenshot prune failed");
      }
      return openSync(path, flags, mode);
    },
  });

  assert.throws(
    () => createRunContext(screenshotOptions(), [], failingRuntime),
    /screenshot prune failed/,
  );
  assert.equal(pruneFailed, true);
  assert.equal(readlinkSync(secondPaths.currentPath), secondPaths.currentTarget);
  assert.equal(readFileSync(sentinel, "utf8"), "previous run is complete");
  assert.equal(existsSync(join(screenshotsRoot, ".lock")), false);
});

test("screenshot setup restores the prior current link when atomic replacement reports a late failure", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-current-swap-rollback-"));
  const tempRoot = join(root, "tmp");
  const screenshotsRoot = join(root, "screenshots");
  mkdirSync(tempRoot);
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
  });
  const previous = createRunContext(screenshotOptions(), [], runtime);
  const previousPaths = screenshotPathsForRun(previous.runId, runtime);
  const sentinel = join(previousPaths.runDir, "complete.txt");
  writeFileSync(sentinel, "previous current is complete");
  assert.equal(await cleanupHarness(previous), true);

  let replacementFailed = false;
  const failingRuntime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
    renameSync(source, target) {
      if (!replacementFailed && basename(source).startsWith(".current-next-") && target === previousPaths.currentPath) {
        replacementFailed = true;
        renameSync(source, target);
        throw new Error("current swap failed after replacement");
      }
      renameSync(source, target);
    },
  });

  assert.throws(
    () => createRunContext(screenshotOptions(), [], failingRuntime),
    /current swap failed after replacement/,
  );
  assert.equal(replacementFailed, true);
  assert.equal(readlinkSync(previousPaths.currentPath), previousPaths.currentTarget);
  assert.equal(readFileSync(sentinel, "utf8"), "previous current is complete");
  assert.equal(existsSync(join(screenshotsRoot, ".lock")), false);
});

test("failed current restoration preserves the new current target instead of leaving a dangling link", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-current-restore-failure-"));
  const tempRoot = join(root, "tmp");
  const screenshotsRoot = join(root, "screenshots");
  mkdirSync(tempRoot);
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
  });
  const first = createRunContext(screenshotOptions(), [], runtime);
  assert.equal(await cleanupHarness(first), true);
  const previous = createRunContext(screenshotOptions(), [], runtime);
  const previousPaths = screenshotPathsForRun(previous.runId, runtime);
  writeFileSync(join(previousPaths.runDir, "complete.txt"), "previous run");
  assert.equal(await cleanupHarness(previous), true);

  let pruneFailed = false;
  let restoreFailed = false;
  const failingRuntime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
    openSync(path, flags, mode) {
      if (!pruneFailed && basename(path).startsWith(".prune-") && (flags & fsConstants.O_DIRECTORY)) {
        pruneFailed = true;
        throw new Error("prune cleanup failed");
      }
      return openSync(path, flags, mode);
    },
    renameSync(source, target) {
      if (!restoreFailed && basename(source).startsWith(".current-restore-") && target === previousPaths.currentPath) {
        restoreFailed = true;
        throw new Error("current restore failed");
      }
      renameSync(source, target);
    },
  });

  assert.throws(
    () => createRunContext(screenshotOptions(), [], failingRuntime),
    /prune cleanup failed.*current restore failed/,
  );
  assert.equal(pruneFailed, true);
  assert.equal(restoreFailed, true);
  const currentTarget = readlinkSync(previousPaths.currentPath);
  const currentTargetPath = resolve(screenshotsRoot, currentTarget);
  assert.equal(existsSync(currentTargetPath), true);
  assert.notEqual(currentTarget, previousPaths.currentTarget);
  assert.equal(readFileSync(join(previousPaths.runDir, "complete.txt"), "utf8"), "previous run");
  assert.equal(existsSync(join(screenshotsRoot, ".lock")), false);
});

test("stale-run pruning cannot delete a replacement directory after quarantine", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-prune-replacement-"));
  const tempRoot = join(root, "tmp");
  const screenshotsRoot = join(root, "screenshots");
  mkdirSync(tempRoot);
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
  });
  const stale = createRunContext(screenshotOptions(), [], runtime);
  assert.equal(await cleanupHarness(stale), true);
  const current = createRunContext(screenshotOptions(), [], runtime);
  assert.equal(await cleanupHarness(current), true);

  let raced = false;
  let replacementPath;
  let movedPath;
  const racingRuntime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
    openSync(path, flags, mode) {
      if (!raced && basename(path).startsWith(".prune-") && (flags & fsConstants.O_DIRECTORY)) {
        raced = true;
        replacementPath = path;
        movedPath = `${path}.moved`;
        renameSync(path, movedPath);
        mkdirSync(path);
        writeFileSync(join(path, "external-sentinel"), "must survive");
      }
      return openSync(path, flags, mode);
    },
  });

  assert.throws(
    () => createRunContext(screenshotOptions(), [], racingRuntime),
    /quarantined screenshot run changed while opening/,
  );
  assert.equal(raced, true);
  assert.equal(readFileSync(join(replacementPath, "external-sentinel"), "utf8"), "must survive");
  assert.equal(existsSync(movedPath), true);
});

test("stale-run pruning preserves an occupied quarantine destination", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-prune-occupied-"));
  const tempRoot = join(root, "tmp");
  const screenshotsRoot = join(root, "screenshots");
  mkdirSync(tempRoot);
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: screenshotsRoot,
    tempRoot,
  });
  const stale = createRunContext(screenshotOptions(), [], runtime);
  const stalePaths = screenshotPathsForRun(stale.runId, runtime);
  writeFileSync(join(stalePaths.runDir, "owned-sentinel"), "stale run");
  assert.equal(await cleanupHarness(stale), true);
  const previous = createRunContext(screenshotOptions(), [], runtime);
  const previousPaths = screenshotPathsForRun(previous.runId, runtime);
  assert.equal(await cleanupHarness(previous), true);
  const staleIdentity = fileIdentity(lstatSync(stalePaths.runDir));

  let quarantinePath;
  let quarantineIdentity;
  const renameNoReplaceSync = runtime.renameNoReplaceSync;
  runtime.renameNoReplaceSync = (source, destination) => {
    if (!quarantinePath && source === stalePaths.runDir && basename(destination).startsWith(".prune-")) {
      quarantinePath = destination;
      mkdirSync(destination);
      writeFileSync(join(destination, "external-sentinel"), "must survive");
      quarantineIdentity = fileIdentity(lstatSync(destination));
    }
    renameNoReplaceSync(source, destination);
  };

  assert.throws(
    () => createRunContext(screenshotOptions(), [], runtime),
    (error) => error.code === "EEXIST",
  );
  assert.ok(quarantinePath);
  assert.deepEqual(fileIdentity(lstatSync(stalePaths.runDir)), staleIdentity);
  assert.equal(readFileSync(join(stalePaths.runDir, "owned-sentinel"), "utf8"), "stale run");
  assert.deepEqual(fileIdentity(lstatSync(quarantinePath)), quarantineIdentity);
  assert.equal(readFileSync(join(quarantinePath, "external-sentinel"), "utf8"), "must survive");
  assert.equal(readlinkSync(previousPaths.currentPath), previousPaths.currentTarget);
  assert.equal(existsSync(join(screenshotsRoot, ".lock")), false);
});

test("execution error releases screenshot ownership through CLI cleanup", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-execution-failure-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const tempRoot = join(root, "tmp");
  mkdirSync(tempRoot);
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    screenshotRoot: join(root, "screenshots"),
    tempRoot,
  });
  let state;
  const exitCode = await runCli([
    "--access",
    "disabled",
    "--screenshots",
    "--script",
    "status/landing.browser.js",
  ], {
    runtime,
    logger: silentLogger,
    createContext(options, plan) {
      state = createRunContext(options, plan, runtime);
      return state;
    },
    execute: async () => {
      throw new Error("browser execution failed");
    },
  });

  assert.equal(exitCode, 1);
  assert.equal(existsSync(`${runtime.screenshotRoot}/.lock`), false);
  assert.equal(existsSync(state.runRoot), false);
});

test("screenshot update replaces stale snapshots and validation compares generated PNGs", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-validation-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const actual = join(root, "actual");
  const expected = join(root, "expected");
  mkdirSync(actual);
  mkdirSync(expected);
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    "base64",
  );
  writeFileSync(join(actual, "screen.png"), png);
  writeFileSync(join(expected, "stale.png"), png);

  validateScreenshots(actual, expected, { update: true, logger: silentLogger });
  assert.deepEqual(readdirSync(expected), ["screen.png"]);
  assert.equal(statSync(join(expected, "screen.png")).mode & 0o777, 0o644);
  validateScreenshots(actual, expected, { update: false, logger: silentLogger });

  writeFileSync(join(actual, "unexpected.png"), png);
  assert.throws(
    () => validateScreenshots(actual, expected, { update: false, logger: silentLogger }),
    /unexpected\.png: missing snapshot/,
  );
});

test("screenshot snapshot update preserves the complete baseline across copy and swap failures", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-snapshot-transaction-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    "base64",
  );

  for (const failure of ["copy", "swap"]) {
    const caseRoot = join(root, failure);
    const actual = join(caseRoot, "actual");
    const expected = join(caseRoot, "expected");
    mkdirSync(actual, { recursive: true });
    mkdirSync(expected);
    writeFileSync(join(actual, "fresh.png"), png);
    writeFileSync(join(expected, "baseline.png"), png);
    writeFileSync(join(expected, "stale.png"), png);
    const runtime = createHarnessRuntime({
      logger: silentLogger,
      writeFileSync(target, contents) {
        if (failure === "copy" && typeof target === "number") {
          throw new Error("snapshot copy failed");
        }
        writeFileSync(target, contents);
      },
      renameNoReplaceSync(source, target) {
        if (failure === "swap" && source.includes(".next-") && target === expected) {
          renameSync(source, target);
          throw new Error("snapshot swap failed");
        }
        renameSync(source, target);
      },
    });

    assert.throws(
      () => validateScreenshots(actual, expected, { update: true, logger: silentLogger, runtime }),
      new RegExp(`snapshot ${failure} failed`),
    );
    assert.deepEqual(readdirSync(expected).sort(), ["baseline.png", "stale.png"]);
    assert.deepEqual(readFileSync(join(expected, "baseline.png")), png);
    assert.deepEqual(readFileSync(join(expected, "stale.png")), png);
  }
});

test("snapshot staging refuses a destination symlink without overwriting its external target", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-staging-symlink-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const actual = join(root, "actual");
  const expected = join(root, "expected");
  const external = join(root, "external-sentinel");
  mkdirSync(actual);
  mkdirSync(expected);
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    "base64",
  );
  writeFileSync(join(actual, "fresh.png"), png);
  writeFileSync(join(expected, "baseline.png"), png);
  writeFileSync(external, "external data");
  let planted = false;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    openSync(path, flags, mode) {
      if (!planted && path.includes(".next-") && basename(path) === "fresh.png" && (flags & fsConstants.O_CREAT)) {
        planted = true;
        symlinkSync(external, path);
      }
      return openSync(path, flags, mode);
    },
  });

  assert.throws(
    () => validateScreenshots(actual, expected, { update: true, logger: silentLogger, runtime }),
    /EEXIST|file already exists/,
  );
  assert.equal(planted, true);
  assert.equal(readFileSync(external, "utf8"), "external data");
  assert.deepEqual(readdirSync(expected), ["baseline.png"]);
});

test("snapshot staging rejects a hard link created during descriptor-bound copy", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-staging-hardlink-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const actual = join(root, "actual");
  const expected = join(root, "expected");
  const externalLink = join(root, "external-link.png");
  mkdirSync(actual);
  mkdirSync(expected);
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    "base64",
  );
  writeFileSync(join(actual, "fresh.png"), png);
  writeFileSync(join(expected, "baseline.png"), png);
  let stagedPath;
  let linked = false;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    openSync(path, flags, mode) {
      const descriptor = openSync(path, flags, mode);
      if (path.includes(".next-") && basename(path) === "fresh.png" && (flags & fsConstants.O_CREAT)) {
        stagedPath = path;
      }
      return descriptor;
    },
    writeFileSync(target, contents) {
      if (!linked && typeof target === "number" && stagedPath) {
        linked = true;
        linkSync(stagedPath, externalLink);
      }
      writeFileSync(target, contents);
    },
  });

  assert.throws(
    () => validateScreenshots(actual, expected, { update: true, logger: silentLogger, runtime }),
    /destination changed while writing/,
  );
  assert.equal(linked, true);
  assert.deepEqual(readFileSync(externalLink), png);
  assert.deepEqual(readdirSync(expected), ["baseline.png"]);
});

test("snapshot publication rejects a hard link added while moving the old baseline", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-publication-hardlink-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const actual = join(root, "actual");
  const expected = join(root, "expected");
  const externalLink = join(root, "external-link.png");
  mkdirSync(actual);
  mkdirSync(expected);
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    "base64",
  );
  writeFileSync(join(actual, "fresh.png"), png);
  writeFileSync(join(expected, "baseline.png"), png);
  let stagedPath;
  let linked = false;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    openSync(path, flags, mode) {
      const descriptor = openSync(path, flags, mode);
      if (path.includes(".next-") && basename(path) === "fresh.png" && (flags & fsConstants.O_CREAT)) {
        stagedPath = path;
      }
      return descriptor;
    },
    renameNoReplaceSync(source, target) {
      if (!linked && source === expected && target.includes(".backup-")) {
        linked = true;
        linkSync(stagedPath, externalLink);
      }
      renameSync(source, target);
    },
  });

  assert.throws(
    () => validateScreenshots(actual, expected, { update: true, logger: silentLogger, runtime }),
    /entry changed: fresh\.png/,
  );
  assert.equal(linked, true);
  assert.deepEqual(readdirSync(expected), ["baseline.png"]);
  assert.deepEqual(readFileSync(join(expected, "baseline.png")), png);
  assert.deepEqual(readFileSync(externalLink), png);
});

test("snapshot cleanup cannot recursively delete a replacement staging directory", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-staging-replacement-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const actual = join(root, "actual");
  const expected = join(root, "expected");
  mkdirSync(actual);
  mkdirSync(expected);
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    "base64",
  );
  writeFileSync(join(actual, "fresh.png"), png);
  writeFileSync(join(expected, "baseline.png"), png);
  let copyFailed = false;
  let raced = false;
  let replacementPath;
  let movedPath;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    writeFileSync(target, contents) {
      if (!copyFailed && typeof target === "number") {
        copyFailed = true;
        throw new Error("snapshot copy failed");
      }
      writeFileSync(target, contents);
    },
    openSync(path, flags, mode) {
      if (copyFailed && !raced && basename(path).includes(".next-") && (flags & fsConstants.O_DIRECTORY)) {
        raced = true;
        replacementPath = path;
        movedPath = `${path}.moved`;
        renameSync(path, movedPath);
        mkdirSync(path);
        writeFileSync(join(path, "external-sentinel"), "must survive");
      }
      return openSync(path, flags, mode);
    },
  });

  assert.throws(
    () => validateScreenshots(actual, expected, { update: true, logger: silentLogger, runtime }),
    /snapshot copy failed.*changed while opening/,
  );
  assert.equal(raced, true);
  assert.equal(readFileSync(join(replacementPath, "external-sentinel"), "utf8"), "must survive");
  assert.equal(existsSync(movedPath), true);
  assert.deepEqual(readdirSync(expected), ["baseline.png"]);
});

test("snapshot rollback detects a completed baseline-to-backup rename after a late error", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-baseline-late-rename-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const actual = join(root, "actual");
  const expected = join(root, "expected");
  mkdirSync(actual);
  mkdirSync(expected);
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    "base64",
  );
  writeFileSync(join(actual, "fresh.png"), png);
  writeFileSync(join(expected, "baseline.png"), png);
  let failed = false;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    renameNoReplaceSync(source, target) {
      if (!failed && source === expected && target.includes(".backup-")) {
        failed = true;
        renameSync(source, target);
        throw new Error("baseline rename failed after completion");
      }
      renameSync(source, target);
    },
  });

  assert.throws(
    () => validateScreenshots(actual, expected, { update: true, logger: silentLogger, runtime }),
    /baseline rename failed after completion/,
  );
  assert.equal(failed, true);
  assert.deepEqual(readdirSync(expected), ["baseline.png"]);
  assert.deepEqual(readFileSync(join(expected, "baseline.png")), png);
});

test("snapshot rollback retries moving a failed new baseline aside before restoring the old baseline", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-swap-rollback-retry-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const actual = join(root, "actual");
  const expected = join(root, "expected");
  mkdirSync(actual);
  mkdirSync(expected);
  const oldPng = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    "base64",
  );
  const newPng = Buffer.from(oldPng);
  writeFileSync(join(actual, "fresh.png"), newPng);
  writeFileSync(join(expected, "baseline.png"), oldPng);
  let swapFailed = false;
  let rollbackFailures = 0;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    renameNoReplaceSync(source, target) {
      if (!swapFailed && source.includes(".next-") && target === expected) {
        swapFailed = true;
        renameSync(source, target);
        throw new Error("snapshot swap failed after completion");
      }
      if (swapFailed && source === expected && target.includes(".failed-") && rollbackFailures++ === 0) {
        throw new Error("first rollback move failed");
      }
      renameSync(source, target);
    },
  });

  assert.throws(
    () => validateScreenshots(actual, expected, { update: true, logger: silentLogger, runtime }),
    /snapshot swap failed after completion/,
  );
  assert.equal(swapFailed, true);
  assert.equal(rollbackFailures, 2);
  assert.deepEqual(readdirSync(expected), ["baseline.png"]);
  assert.deepEqual(readFileSync(join(expected, "baseline.png")), oldPng);
  assert.ok(readdirSync(root).some((entry) => entry.includes(".failed-")));
});

test("snapshot rollback preserves an occupied failed-baseline destination", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-failed-baseline-occupied-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const actual = join(root, "actual");
  const expected = join(root, "expected");
  mkdirSync(actual);
  mkdirSync(expected);
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    "base64",
  );
  writeFileSync(join(actual, "fresh.png"), png);
  writeFileSync(join(expected, "baseline.png"), png);
  const runtime = createHarnessRuntime({ logger: silentLogger });
  const renameNoReplaceSync = runtime.renameNoReplaceSync;
  let swapFailed = false;
  let failedMoveAttempts = 0;
  let newBaselineIdentity;
  let occupiedPath;
  let occupiedIdentity;
  runtime.renameNoReplaceSync = (source, target) => {
    if (!swapFailed && source.includes(".next-") && target === expected) {
      renameNoReplaceSync(source, target);
      swapFailed = true;
      newBaselineIdentity = fileIdentity(lstatSync(expected));
      throw new Error("snapshot swap failed after completion");
    }
    if (swapFailed && source === expected && target.includes(".failed-")) {
      failedMoveAttempts += 1;
      if (failedMoveAttempts === 1) {
        occupiedPath = target;
        mkdirSync(target);
        writeFileSync(join(target, "external-sentinel"), "must survive");
        occupiedIdentity = fileIdentity(lstatSync(target));
        try {
          renameNoReplaceSync(source, target);
        } catch (error) {
          assert.equal(error.code, "EEXIST");
          throw error;
        }
        assert.fail("occupied failed-baseline destination was replaced");
      }
    }
    renameNoReplaceSync(source, target);
  };

  assert.throws(
    () => validateScreenshots(actual, expected, { update: true, logger: silentLogger, runtime }),
    /snapshot swap failed after completion/,
  );
  assert.equal(failedMoveAttempts, 2);
  assert.deepEqual(readdirSync(expected), ["baseline.png"]);
  assert.deepEqual(readFileSync(join(expected, "baseline.png")), png);
  assert.deepEqual(fileIdentity(lstatSync(occupiedPath)), occupiedIdentity);
  assert.equal(readFileSync(join(occupiedPath, "external-sentinel"), "utf8"), "must survive");
  const failedPath = readdirSync(root)
    .map((entry) => join(root, entry))
    .find((path) => path !== occupiedPath && basename(path).includes(".failed-"));
  assert.ok(failedPath);
  assert.deepEqual(fileIdentity(lstatSync(failedPath)), newBaselineIdentity);
  assert.deepEqual(readdirSync(failedPath), ["fresh.png"]);
});

test("exhausted rollback accepts a fully validated new baseline that remains canonical", (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-screenshot-swap-committed-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const actual = join(root, "actual");
  const expected = join(root, "expected");
  mkdirSync(actual);
  mkdirSync(expected);
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    "base64",
  );
  writeFileSync(join(actual, "fresh.png"), png);
  writeFileSync(join(expected, "baseline.png"), png);
  let swapReportedFailure = false;
  let rollbackAttempts = 0;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    renameNoReplaceSync(source, target) {
      if (!swapReportedFailure && source.includes(".next-") && target === expected) {
        swapReportedFailure = true;
        renameSync(source, target);
        throw new Error("snapshot swap reported failure after completion");
      }
      if (swapReportedFailure && source === expected && target.includes(".failed-")) {
        rollbackAttempts += 1;
        throw new Error("rollback move unavailable");
      }
      renameSync(source, target);
    },
  });

  validateScreenshots(actual, expected, { update: true, logger: silentLogger, runtime });
  assert.equal(swapReportedFailure, true);
  assert.equal(rollbackAttempts, 8);
  assert.deepEqual(readdirSync(expected), ["fresh.png"]);
  assert.deepEqual(readFileSync(join(expected, "fresh.png")), png);
});

test("Compose startup failure captures diagnostics before teardown", async () => {
  const commands = [];
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    commandRunner({ args }) {
      const operation = composeOperation(args);
      commands.push(operation);
      return operation === "up" ? commandResult(1, { stderr: "startup failed" }) : commandResult();
    },
    writeFileSync() {},
  });
  const state = testState({
    runtime,
    plan: [{
      phase: "disabled",
      profile: "test",
      waltrService: "waltr-test",
      k6Service: "k6",
      upServices: ["waltr-test"],
      scripts: [],
    }],
  });

  const result = await executeComposeHarness(state);

  assert.deepEqual(commands, ["build", "up", "ps", "logs", "down"]);
  assert.equal(result.testExitCode, 1);
  assert.equal(result.cleanupFailed, false);
  assert.equal(state.composeRunning, false);
  assert.deepEqual([...state.diagnosticsCaptured], ["disabled"]);
});

test("stopCompose captures diagnostics and stops children before one retry", async () => {
  const commands = [];
  let downCalls = 0;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    commandRunner({ args }) {
      const operation = composeOperation(args);
      commands.push(operation);
      if (operation === "down" && downCalls++ === 0) {
        return commandResult(1, { stderr: "daemon unavailable" });
      }
      return commandResult();
    },
    writeFileSync() {},
  });
  const state = testState({ runtime, composeRunning: true, activePhase: "oidc" });

  assert.equal(await stopCompose(state), true);
  assert.deepEqual(commands, ["down", "ps", "logs", "down"]);
  assert.equal(state.composeRunning, false);
  assert.equal(state.composeTeardownAttempted, true);
  assert.equal(await stopCompose(state), true);
  assert.equal(downCalls, 2);
});

test("runDevK6 uses a deterministic name and registers daemon cleanup", async (context) => {
  const runRoot = mkdtempSync(join(tmpdir(), "waltr-dev-k6-contract-"));
  context.after(() => rmSync(runRoot, { force: true, recursive: true }));
  const commands = [];
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    commandRunner({ args }) {
      commands.push(args);
      if (args[0] === "run") {
        return commandResult(17);
      }
      return commandResult(1, { stderr: "No such container" });
    },
  });
  const state = testState({
    runtime,
    runRoot,
    options: { mode: "dev", access: "disabled", screenshots: false, live: false },
    dockerWebUrl: "http://172.17.0.1:5173",
    devK6ContainerName: "contract-project-dev-k6",
    devScreenshotContainerName: "contract-project-dev-screenshot-permissions",
    directContainerNames: new Set(),
  });

  assert.equal(await runDevK6(state, "tests/web/k6/status/landing.browser.js"), 17);
  assert.deepEqual([...state.directContainerNames], ["contract-project-dev-k6"]);
  const runArgs = commands.find((args) => args[0] === "run");
  assert.equal(runArgs[runArgs.indexOf("--name") + 1], "contract-project-dev-k6");
  assert.equal(runArgs.includes("--rm"), true);

  assert.equal(await cleanupHarness(state), true);
  assert.equal(commands.filter((args) => args[0] === "container" && args.at(-1) === "contract-project-dev-k6").length, 3);
  assert.equal(existsSync(runRoot), false);
});

test("two failed Compose downs retain running state and fail the run after child termination", async () => {
  const events = [];
  const child = fakeChild(5150);
  let groupAlive = true;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    getProcessIdentity: () => fakeProcessIdentity(5150, "leader-start"),
    inspectProcessGroup: () => ({ alive: groupAlive, reused: false }),
    spawn(command) {
      events.push(`spawn:${command}`);
      return child;
    },
    processKill(pid, signal) {
      events.push(`signal:${pid}:${signal}`);
      groupAlive = false;
      queueMicrotask(() => child.emit("close", null, signal));
    },
    commandRunner({ args }) {
      const operation = composeOperation(args);
      events.push(operation);
      return operation === "down" ? commandResult(1, { stderr: "down failed" }) : commandResult();
    },
    writeFileSync() {},
  });
  const state = testState({
    runtime,
    plan: [{
      phase: "disabled",
      profile: "test",
      waltrService: "waltr-test",
      k6Service: "k6",
      upServices: ["waltr-test"],
      scripts: [],
    }],
  });
  spawnRecord(state, "worker", [], {
    capture: false,
    cwd: "/tmp",
    env: {},
    ignoreAbort: false,
    stdio: "ignore",
    timeoutMs: null,
  });

  const result = await executeComposeHarness(state);

  assert.deepEqual(events, ["spawn:worker", "build", "up", "down", "ps", "logs", "signal:-5150:SIGTERM", "down"]);
  assert.equal(result.testExitCode, 0);
  assert.equal(result.cleanupFailed, true);
  assert.equal(exitCodeFor(result), 1);
  assert.equal(state.failure, true);
  assert.equal(state.composeRunning, true);
  assert.equal(state.children.size, 0);
});

test("cleanup removes direct containers around child shutdown and deletes the run root", async (context) => {
  const runRoot = mkdtempSync(join(tmpdir(), "waltr-cleanup-contract-"));
  context.after(() => rmSync(runRoot, { force: true, recursive: true }));
  const events = [];
  const child = fakeChild(4242);
  let groupAlive = true;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    getProcessIdentity: () => fakeProcessIdentity(4242, "cleanup-leader"),
    inspectProcessGroup: () => ({ alive: groupAlive, reused: false }),
    spawn(command, args, options) {
      events.push(`spawn:${command}`);
      assert.equal(options.detached, true);
      return child;
    },
    processKill(pid, signal) {
      events.push(`signal:${pid}:${signal}`);
      groupAlive = false;
      queueMicrotask(() => child.emit("close", null, signal));
    },
    commandRunner({ args }) {
      if (args[0] === "compose") {
        events.push(`compose:${composeOperation(args)}`);
      } else {
        events.push(`remove:${args.at(-1)}`);
      }
      return commandResult();
    },
  });
  const state = testState({
    runtime,
    runRoot,
    composeRunning: true,
    directContainerNames: new Set(["contract-dev-k6", "contract-dev-screenshot-permissions"]),
  });
  spawnRecord(state, "worker", [], {
    capture: false,
    cwd: runRoot,
    env: {},
    ignoreAbort: false,
    stdio: "ignore",
    timeoutMs: null,
  });

  assert.equal(await cleanupHarness(state), true);
  assert.deepEqual(events, [
    "spawn:worker",
    "compose:down",
    "remove:contract-dev-k6",
    "remove:contract-dev-screenshot-permissions",
    "signal:-4242:SIGTERM",
    "remove:contract-dev-k6",
    "remove:contract-dev-screenshot-permissions",
  ]);
  assert.equal(state.children.size, 0);
  assert.equal(existsSync(runRoot), false);
});

test("cleanup reports persistent direct container removal failures", async (context) => {
  const runRoot = mkdtempSync(join(tmpdir(), "waltr-cleanup-failure-contract-"));
  context.after(() => rmSync(runRoot, { force: true, recursive: true }));
  let removalCalls = 0;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    commandRunner() {
      removalCalls += 1;
      return commandResult(1, { stderr: "cannot remove container" });
    },
  });
  const state = testState({
    runtime,
    runRoot,
    directContainerNames: new Set(["contract-dev-k6"]),
  });

  assert.equal(await cleanupHarness(state), false);
  assert.equal(removalCalls, 2);
  assert.equal(existsSync(runRoot), false);
});

test("cleanup continues and removes runRoot when screenshot lock inspection fails", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-cleanup-lock-io-failure-"));
  const runRoot = join(root, "run");
  const lockPath = join(root, "screenshots.lock");
  mkdirSync(runRoot);
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const baseRuntime = createHarnessRuntime({ logger: silentLogger });
  const owner = createScreenshotLockOwner("cleanup-lock", baseRuntime);
  const lock = acquireScreenshotLock(lockPath, owner, baseRuntime);
  const events = [];
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    commandRunner({ args }) {
      events.push(args.at(-1));
      return commandResult(1, { stderr: "No such container" });
    },
    lstatSync(path) {
      if (path === lockPath) {
        throw new Error("lock stat failed");
      }
      return lstatSync(path);
    },
    rmSync(path, options) {
      events.push(`remove:${path}`);
      rmSync(path, options);
    },
  });
  const state = testState({
    runtime,
    runRoot,
    screenshotLock: lock,
    directContainerNames: new Set(["cleanup-direct-container"]),
  });

  assert.equal(await cleanupHarness(state), false);
  assert.equal(events.filter((event) => event === "cleanup-direct-container").length, 2);
  assert.equal(existsSync(runRoot), false);
  assert.equal(existsSync(lockPath), true, "the lock must survive failed ownership verification");
});

test("runRoot cleanup refuses a replacement and preserves both unrelated and original data", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-run-root-replacement-"));
  const tempRoot = join(root, "temp");
  mkdirSync(tempRoot);
  context.after(() => rmSync(root, { force: true, recursive: true }));
  let allocatedRoot;
  let movedRoot;
  let raced = false;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    logRoot: join(root, "logs"),
    tempRoot,
    mkdtempSync(prefix) {
      allocatedRoot = mkdtempSync(prefix);
      return allocatedRoot;
    },
    openSync(path, flags, mode) {
      if (!raced && path === allocatedRoot && (flags & fsConstants.O_DIRECTORY)) {
        raced = true;
        movedRoot = `${allocatedRoot}.original`;
        renameSync(allocatedRoot, movedRoot);
        mkdirSync(allocatedRoot);
        writeFileSync(join(allocatedRoot, "external-sentinel"), "must survive");
      }
      return openSync(path, flags, mode);
    },
  });
  const state = createRunContext({
    mode: "compose",
    access: "disabled",
    live: false,
    screenshots: false,
    updateScreenshots: false,
    script: null,
  }, [], runtime);
  writeFileSync(join(state.runRoot, "owned-sentinel"), "original root");

  assert.equal(await cleanupHarness(state), false);
  assert.equal(raced, true);
  assert.equal(readFileSync(join(allocatedRoot, "external-sentinel"), "utf8"), "must survive");
  assert.equal(readFileSync(join(movedRoot, "owned-sentinel"), "utf8"), "original root");
});

test("runRoot quarantine never replaces an occupied destination", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-run-root-occupied-quarantine-"));
  const runRoot = join(root, "run");
  const quarantinePath = join(root, `.remove-run-${"00".repeat(8)}`);
  mkdirSync(runRoot);
  mkdirSync(quarantinePath);
  const occupied = lstatSync(quarantinePath);
  const occupiedIdentity = { device: occupied.dev, inode: occupied.ino };
  writeFileSync(join(root, "unrelated-sentinel"), "must survive");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    randomBytes: (size) => Buffer.alloc(size),
  });
  const state = testState({ runtime, runRoot });

  assert.equal(await cleanupHarness(state), false);
  assert.deepEqual(
    fileIdentity(lstatSync(quarantinePath)),
    occupiedIdentity,
    "the occupied quarantine inode must survive",
  );
  assert.equal(existsSync(runRoot), true, "the owned source must not be moved over an occupied name");
  assert.equal(readFileSync(join(root, "unrelated-sentinel"), "utf8"), "must survive");
});

test("runRoot cleanup removes nested directories through the Linux helper", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-run-root-helper-descriptor-"));
  const runRoot = join(root, "run");
  mkdirSync(join(runRoot, "nested", "deeper"), { recursive: true });
  writeFileSync(join(runRoot, "nested", "deeper", "sentinel"), "owned");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const runtime = createHarnessRuntime({ logger: silentLogger });
  const state = testState({ runtime, runRoot });

  assert.equal(await cleanupHarness(state), true);
  assert.equal(existsSync(runRoot), false);
});

test("runRoot final removal preserves a quarantine identity mismatch", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-run-root-final-identity-race-"));
  const runRoot = join(root, "run");
  const quarantinePath = join(root, `.remove-run-${"00".repeat(8)}`);
  const movedOwned = join(root, "owned-original");
  mkdirSync(runRoot);
  writeFileSync(join(root, "unrelated-sentinel"), "must survive");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  let replacementIdentity;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    randomBytes: (size) => Buffer.alloc(size),
    removeDirectoryIfIdentitySync(path) {
      assert.equal(path, quarantinePath);
      renameSync(path, movedOwned);
      mkdirSync(path);
      replacementIdentity = fileIdentity(lstatSync(path));
      const error = new Error("directory identity changed before removal");
      error.code = "ESTALE";
      throw error;
    },
  });
  const state = testState({ runtime, runRoot });

  assert.equal(await cleanupHarness(state), false);
  assert.deepEqual(fileIdentity(lstatSync(quarantinePath)), replacementIdentity);
  assert.equal(existsSync(movedOwned), true, "the verified emptied inode must be preserved");
  assert.equal(readFileSync(join(root, "unrelated-sentinel"), "utf8"), "must survive");
});

test("descriptor cleanup refuses to cross a child mount identity", async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-run-root-mount-boundary-"));
  const runRoot = join(root, "run");
  const mountedChild = join(runRoot, "mounted-child");
  mkdirSync(mountedChild, { recursive: true });
  writeFileSync(join(mountedChild, "sentinel"), "must survive");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  const descriptorMounts = new Map();
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    openSync(path, flags, mode) {
      const descriptor = openSync(path, flags, mode);
      if (flags & fsConstants.O_DIRECTORY) {
        descriptorMounts.set(descriptor, basename(path) === "mounted-child" ? 22 : 11);
      }
      return descriptor;
    },
    getDescriptorMountId(descriptor) {
      return descriptorMounts.get(descriptor);
    },
  });
  const state = testState({ runtime, runRoot });

  assert.equal(await cleanupHarness(state), false);
  assert.equal(readFileSync(join(mountedChild, "sentinel"), "utf8"), "must survive");
  assert.equal(existsSync(runRoot), true);
});

test("command timeout terminates the process group and resolves only after close", async () => {
  const timers = [];
  const signals = [];
  const child = fakeChild(2718);
  let groupAlive = true;
  let spawnOptions;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    getProcessIdentity: () => fakeProcessIdentity(2718, "timeout-leader"),
    inspectProcessGroup: () => ({ alive: groupAlive, reused: false }),
    spawn(_command, _args, options) {
      spawnOptions = options;
      return child;
    },
    processKill(pid, signal) {
      signals.push([pid, signal]);
    },
    setTimeout(callback, milliseconds) {
      const timer = { callback, milliseconds, cleared: false };
      timers.push(timer);
      return timer;
    },
    clearTimeout(timer) {
      if (timer) {
        timer.cleared = true;
      }
    },
  });
  const state = testState({ runtime });
  const record = spawnRecord(state, "worker", [], {
    capture: false,
    cwd: "/tmp",
    env: {},
    ignoreAbort: false,
    timeoutMs: 50,
  });
  let settled = false;
  void record.closed.then(() => {
    settled = true;
  });

  assert.equal(spawnOptions.detached, true);
  assert.equal(timers[0].milliseconds, 50);
  timers[0].callback();
  await Promise.resolve();
  assert.equal(settled, false);
  assert.deepEqual(signals, [[-2718, "SIGTERM"]]);
  assert.equal(timers[1].milliseconds, 2_000);
  timers[1].callback();
  assert.deepEqual(signals, [[-2718, "SIGTERM"], [-2718, "SIGKILL"]]);
  assert.equal(settled, false);

  groupAlive = false;
  child.emit("close", null, "SIGKILL");
  const result = await record.closed;
  assert.equal(result.timedOut, true);
  assert.equal(result.signal, "SIGKILL");
  assert.equal(state.children.size, 0);
});

test("leader close does not cancel the armed process-group force kill", async () => {
  const timers = [];
  const signals = [];
  const child = fakeChild(3535);
  let groupAlive = true;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    getProcessIdentity: () => fakeProcessIdentity(3535, "force-kill-leader"),
    inspectProcessGroup: () => ({ alive: groupAlive, reused: false }),
    spawn: () => child,
    processKill(pid, signal) {
      signals.push([pid, signal]);
      if (signal === "SIGTERM") {
        child.emit("close", 0, null);
      } else if (signal === "SIGKILL") {
        groupAlive = false;
      }
    },
    setTimeout(callback, milliseconds) {
      const timer = { callback, milliseconds, cleared: false };
      timers.push(timer);
      return timer;
    },
    clearTimeout(timer) {
      if (timer) {
        timer.cleared = true;
      }
    },
  });
  const state = testState({ runtime });
  spawnRecord(state, "worker", [], {
    capture: false,
    cwd: "/tmp",
    env: {},
    ignoreAbort: false,
    timeoutMs: null,
  });

  requestSignalStop(state, 130, "SIGINT");
  assert.deepEqual(signals, [[-3535, "SIGTERM"]]);
  assert.equal(state.children.size, 1, "the surviving group must remain tracked");
  assert.equal(timers[0].milliseconds, 2_000);
  assert.equal(timers[0].cleared, false);

  timers[0].callback();
  assert.deepEqual(signals, [[-3535, "SIGTERM"], [-3535, "SIGKILL"]]);
  assert.equal(await stopChildren(state), true);
  assert.equal(state.children.size, 0);
});

test("stopChildren kills a TERM-ignoring descendant after its group leader exits", { skip: process.platform !== "linux" }, async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-process-group-contract-"));
  const childPidPath = join(root, "child.pid");
  const readyPath = join(root, "ready");
  const signalPath = join(root, "signal");
  const childProgram = [
    "const fs = require('node:fs');",
    "process.on('SIGTERM', () => { fs.writeFileSync(process.argv[2], 'SIGTERM-ignored'); });",
    "fs.writeFileSync(process.argv[1], 'ready');",
    "setInterval(() => {}, 1000);",
  ].join(" ");
  const parentProgram = [
    "const { spawn } = require('node:child_process');",
    "const fs = require('node:fs');",
    `const child = spawn(${JSON.stringify(process.execPath)}, ['-e', ${JSON.stringify(childProgram)}, ${JSON.stringify(readyPath)}, ${JSON.stringify(signalPath)}], { stdio: 'ignore' });`,
    `fs.writeFileSync(${JSON.stringify(childPidPath)}, String(child.pid));`,
    "process.on('SIGTERM', () => process.exit(0));",
  ].join(" ");
  const state = testState({ runtime: createHarnessRuntime({ logger: silentLogger }) });
  const record = spawnRecord(state, process.execPath, ["-e", parentProgram], {
    capture: false,
    cwd: root,
    env: process.env,
    ignoreAbort: false,
    stdio: "ignore",
    timeoutMs: null,
  });
  let descendantPid;
  context.after(async () => {
    if (record.child.pid) {
      try {
        process.kill(-record.child.pid, "SIGKILL");
      } catch (error) {
        if (error.code !== "ESRCH") {
          throw error;
        }
      }
    }
    if (descendantPid) {
      try {
        process.kill(descendantPid, "SIGKILL");
      } catch (error) {
        if (error.code !== "ESRCH") {
          throw error;
        }
      }
    }
    rmSync(root, { force: true, recursive: true });
  });

  await waitFor(() => existsSync(readyPath), 3_000);
  descendantPid = Number(readFileSync(childPidPath, "utf8"));
  assert.equal(Number.isInteger(descendantPid), true);
  const startedAt = Date.now();
  assert.equal(await stopChildren(state), true);
  assert.ok(Date.now() - startedAt >= 1_500, "cleanup must wait for the TERM grace period");
  await waitFor(() => existsSync(signalPath), 1_000);
  assert.equal(readFileSync(signalPath, "utf8"), "SIGTERM-ignored");
  await waitFor(() => !processIsAlive(descendantPid), 1_000);
  assert.equal(state.children.size, 0);
});

test("runCommandResult waits for a timed-out leader's TERM-ignoring descendant", { skip: process.platform !== "linux" }, async (context) => {
  const root = mkdtempSync(join(tmpdir(), "waltr-command-timeout-descendant-"));
  const childPidPath = join(root, "child.pid");
  const readyPath = join(root, "ready");
  const signalPath = join(root, "signal");
  let descendantPid;
  context.after(() => {
    if (descendantPid) {
      try {
        process.kill(descendantPid, "SIGKILL");
      } catch (error) {
        if (error.code !== "ESRCH") {
          throw error;
        }
      }
    }
    rmSync(root, { force: true, recursive: true });
  });
  const childProgram = [
    "const fs = require('node:fs');",
    "process.on('SIGTERM', () => { fs.writeFileSync(process.argv[2], 'SIGTERM-ignored'); });",
    "fs.writeFileSync(process.argv[1], 'ready');",
    "setInterval(() => {}, 1000);",
  ].join(" ");
  const parentProgram = [
    "const { spawn } = require('node:child_process');",
    "const fs = require('node:fs');",
    `const child = spawn(${JSON.stringify(process.execPath)}, ['-e', ${JSON.stringify(childProgram)}, ${JSON.stringify(readyPath)}, ${JSON.stringify(signalPath)}], { stdio: 'ignore' });`,
    `fs.writeFileSync(${JSON.stringify(childPidPath)}, String(child.pid));`,
    "process.on('SIGTERM', () => process.exit(0));",
    "setInterval(() => {}, 1000);",
  ].join(" ");
  const state = testState({
    runRoot: root,
    runtime: createHarnessRuntime({ logger: silentLogger }),
  });
  const startedAt = Date.now();
  const resultPromise = runCommandResult(state, process.execPath, ["-e", parentProgram], {
    stdio: "ignore",
    timeoutMs: 200,
  });
  await waitFor(() => existsSync(readyPath), 3_000);
  descendantPid = Number(readFileSync(childPidPath, "utf8"));
  const result = await resultPromise;

  assert.equal(result.timedOut, true);
  assert.ok(Date.now() - startedAt >= 1_500, "command completion must include the TERM grace period");
  assert.equal(readFileSync(signalPath, "utf8"), "SIGTERM-ignored");
  assert.equal(processIsAlive(descendantPid), false);
  assert.equal(state.children.size, 0);
  assert.equal(await cleanupHarness(state), true);
});

test("process-group identity reuse is never signaled", async () => {
  const child = fakeChild(6262);
  let groupState = { alive: true, reused: false };
  const signals = [];
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    getProcessIdentity: () => fakeProcessIdentity(6262, "original-start"),
    inspectProcessGroup: () => groupState,
    spawn: () => child,
    processKill(pid, signal) {
      signals.push([pid, signal]);
    },
  });
  const state = testState({ runtime });
  spawnRecord(state, "worker", [], {
    capture: false,
    cwd: "/tmp",
    env: {},
    ignoreAbort: false,
    timeoutMs: null,
  });
  groupState = { alive: false, reused: true };
  child.emit("close", 0, null);

  assert.equal(await stopChildren(state), true);
  assert.deepEqual(signals, []);
  assert.equal(state.children.size, 0);
});

test("missing launch identity is contained without signaling a later reused process group", { skip: process.platform !== "linux" }, async () => {
  const root = mkdtempSync(join(tmpdir(), "waltr-process-group-missing-identity-"));
  const child = fakeChild(7373);
  const directSignals = [];
  const groupSignals = [];
  child.kill = (signal) => {
    directSignals.push(signal);
    return true;
  };
  let identityReused = false;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    getProcessIdentity: () => identityReused ? fakeProcessIdentity(7373, "reused-start") : null,
    spawn: () => child,
    processKill(pid, signal) {
      groupSignals.push([pid, signal]);
    },
  });
  const state = testState({ runtime, runRoot: root });
  const record = spawnRecord(state, "fast-worker", [], {
    capture: false,
    cwd: root,
    env: {},
    ignoreAbort: false,
    stdio: "ignore",
    timeoutMs: null,
  });

  assert.deepEqual(directSignals, ["SIGKILL"], "the unowned leader must be contained immediately");
  assert.equal(record.hasProcessGroup, false);
  identityReused = true;
  child.emit("close", 0, null);
  const result = await record.closed;
  assert.match(result.error.message, /could not establish Linux process-group identity/);
  assert.equal(await cleanupHarness(state), false);
  assert.deepEqual(groupSignals, [], "a reused numeric PGID must never receive a negative signal");
  assert.equal(existsSync(root), false);
});

test("missing launch identity settles as cleanup failure when close never arrives", { skip: process.platform !== "linux" }, async () => {
  const timers = [];
  const child = fakeChild(7474);
  const directSignals = [];
  const groupSignals = [];
  child.kill = (signal) => {
    directSignals.push(signal);
    if (directSignals.filter((entry) => entry === "SIGKILL").length === 2) {
      queueMicrotask(() => child.emit("close", null, "SIGKILL"));
    }
    return true;
  };
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    getProcessIdentity: () => null,
    spawn: () => child,
    processKill(pid, signal) {
      groupSignals.push([pid, signal]);
    },
    setTimeout(callback, milliseconds) {
      const timer = {
        callback,
        milliseconds,
        cleared: false,
        handle: setTimeout(callback, 1),
      };
      timers.push(timer);
      return timer;
    },
    clearTimeout(timer) {
      if (timer) {
        timer.cleared = true;
        clearTimeout(timer.handle);
      }
    },
  });
  const state = testState({ runtime });
  const record = spawnRecord(state, "identityless-worker", [], {
    capture: false,
    cwd: "/tmp",
    env: {},
    ignoreAbort: false,
    stdio: "ignore",
    timeoutMs: null,
  });

  assert.deepEqual(directSignals, ["SIGKILL"]);
  assert.equal(record.terminationRequested, true);
  assert.equal(record.hasProcessGroup, false);
  const result = await record.closed;
  assert.ok(timers.some((timer) => timer.milliseconds === 7_000));
  assert.match(result.error.message, /could not establish Linux process-group identity/);
  assert.match(result.error.message, /did not exit after termination/);
  assert.equal(state.processGroupOwnershipFailed, true);
  assert.equal(state.children.size, 1, "settlement must retain cleanup ownership of a live child");

  assert.equal(await stopChildren(state), false, "launch identity failure remains a cleanup failure");
  assert.deepEqual(directSignals, ["SIGKILL", "SIGTERM", "SIGKILL"]);
  assert.deepEqual(groupSignals, [], "missing identity must never use negative-PGID signaling");
  assert.equal(state.children.size, 0, "late close must release retained ownership");
});

test("completion deadline clears timers and relinquishes handles while retaining live ownership", { skip: process.platform !== "linux" }, async () => {
  const timers = [];
  const child = fakeChild(7575);
  const streams = ["stdin", "stdout", "stderr"].map(() => ({
    destroyed: false,
    unreferenced: false,
    destroy() {
      this.destroyed = true;
    },
    unref() {
      this.unreferenced = true;
    },
    on() {
      return this;
    },
  }));
  [child.stdin, child.stdout, child.stderr] = streams;
  child.kill = () => true;
  child.unreferenced = false;
  child.unref = () => {
    child.unreferenced = true;
  };
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    getProcessIdentity: () => null,
    spawn: () => child,
    setTimeout(callback, milliseconds) {
      const timer = { callback, milliseconds, cleared: false, fired: false };
      timers.push(timer);
      return timer;
    },
    clearTimeout(timer) {
      if (timer) {
        timer.cleared = true;
      }
    },
  });
  const state = testState({ runtime });
  const record = spawnRecord(state, "long-timeout-worker", [], {
    capture: true,
    cwd: "/tmp",
    env: {},
    ignoreAbort: false,
    timeoutMs: 10 * 60_000,
  });
  const deadline = timers.find((timer) => timer.milliseconds === 7_000);
  assert.ok(deadline);
  deadline.fired = true;
  deadline.callback();
  await record.closed;

  assert.equal(timers.find((timer) => timer.milliseconds === 10 * 60_000).cleared, true);
  assert.equal(timers.filter((timer) => !timer.cleared && !timer.fired).length, 0);
  assert.equal(record.timeoutTimer, null);
  assert.equal(record.completionPollTimer, null);
  assert.equal(record.completionDeadlineTimer, null);
  assert.equal(child.unreferenced, true);
  assert.equal(streams.every((stream) => stream.destroyed && stream.unreferenced), true);
  assert.equal(state.children.size, 1);
  child.emit("close", null, "SIGKILL");
  assert.equal(state.children.size, 0);
});

test("retained-record cleanup keeps one poll armed and settles once after late close", { skip: process.platform !== "linux" }, async () => {
  const timers = [];
  const child = fakeChild(7676);
  let killCount = 0;
  child.kill = (signal) => {
    if (signal === "SIGKILL") {
      killCount += 1;
      if (killCount === 2) {
        queueMicrotask(() => child.emit("close", null, "SIGKILL"));
      }
    }
    return true;
  };
  let maxReferencedPolls = 0;
  const runtime = createHarnessRuntime({
    logger: silentLogger,
    getProcessIdentity: () => null,
    spawn: () => child,
    setTimeout(callback, milliseconds) {
      const timer = { callback, milliseconds, referenced: true, handle: null };
      timers.push(timer);
      if (milliseconds === 25) {
        maxReferencedPolls = Math.max(
          maxReferencedPolls,
          timers.filter((candidate) => candidate.milliseconds === 25 && candidate.referenced).length,
        );
      }
      timer.handle = setTimeout(() => {
        timer.referenced = false;
        callback();
      }, 1);
      return timer;
    },
    clearTimeout(timer) {
      if (timer) {
        timer.referenced = false;
        clearTimeout(timer.handle);
      }
    },
  });
  const state = testState({ runtime });
  const record = spawnRecord(state, "retained-worker", [], {
    capture: false,
    cwd: "/tmp",
    env: {},
    ignoreAbort: false,
    stdio: "ignore",
    timeoutMs: null,
  });
  await record.closed;
  assert.equal(state.children.size, 1);

  let settlements = 0;
  const stopping = stopChildren(state);
  void stopping.then(() => {
    settlements += 1;
  });
  assert.equal(await stopping, false);
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(maxReferencedPolls, 1);
  assert.equal(timers.filter((timer) => timer.milliseconds === 25 && timer.referenced).length, 0);
  assert.equal(settlements, 1);
  assert.equal(state.children.size, 0);
});

const silentLogger = { log() {}, error() {} };

function testState(overrides = {}) {
  const state = {
    options: { mode: "compose", access: "disabled", screenshots: false },
    plan: [],
    runtime: createHarnessRuntime({ logger: silentLogger }),
    projectName: "contract-project",
    baseEnvironment: { PATH: process.env.PATH ?? "" },
    children: new Set(),
    directContainerNames: new Set(),
    abortController: new AbortController(),
    signalCode: null,
    activePhase: null,
    composeRunning: false,
    composeTeardownAttempted: false,
    diagnosticsCaptured: new Set(),
    launchLogDir: "/tmp/waltr-contract-logs",
    runRoot: "/tmp/waltr-contract-root",
    screenshotLock: null,
    shuttingDown: false,
    runtimeError: null,
    failure: false,
    processGroupOwnershipFailed: false,
    ...overrides,
  };
  if (!state.runRootOwnership && existsSync(state.runRoot)) {
    state.runRootOwnership = directoryOwnership(state.runRoot, "web integration root");
  }
  return state;
}

function directoryOwnership(path, label) {
  const resolvedPath = resolve(path);
  const parent = dirname(resolvedPath);
  const directory = lstatSync(resolvedPath);
  const parentStat = lstatSync(parent);
  return {
    path: resolvedPath,
    parent,
    parentIdentity: { device: parentStat.dev, inode: parentStat.ino },
    fileIdentity: { device: directory.dev, inode: directory.ino },
    label,
  };
}

function fileIdentity(stat) {
  return { device: stat.dev, inode: stat.ino };
}

function screenshotOptions(overrides = {}) {
  return {
    mode: "compose",
    access: "disabled",
    live: false,
    screenshots: true,
    updateScreenshots: false,
    script: null,
    ...overrides,
  };
}

function commandResult(code = 0, overrides = {}) {
  return {
    code,
    signal: null,
    error: null,
    timedOut: false,
    stdout: "",
    stderr: "",
    ...overrides,
  };
}

function composeOperation(args) {
  return ["build", "up", "exec", "cp", "run", "ps", "logs", "down"]
    .find((operation) => args.includes(operation));
}

function fakeChild(pid) {
  const child = new EventEmitter();
  child.pid = pid;
  child.exitCode = null;
  child.signalCode = null;
  child.kill = (signal) => child.emit("close", null, signal);
  return child;
}

function fakeProcessIdentity(pid, startIdentity) {
  return { pid, processGroupId: pid, sessionId: pid, startIdentity };
}

function processIsAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if (error.code === "ESRCH") {
      return false;
    }
    throw error;
  }
}

async function waitFor(predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) {
      throw new Error(`condition was not met within ${timeoutMs}ms`);
    }
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}
