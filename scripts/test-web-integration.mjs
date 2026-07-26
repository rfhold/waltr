import { spawn, spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import {
  constants as fsConstants,
  chmodSync,
  closeSync,
  copyFileSync,
  existsSync,
  fchmodSync,
  fstatSync,
  fsyncSync,
  linkSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  renameSync,
  rmdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { networkInterfaces, tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import net from "node:net";
import { inflateSync } from "node:zlib";

const scriptPath = fileURLToPath(import.meta.url);
const linuxDirectoryHelper = fileURLToPath(new URL("./linux-directory-helper.py", import.meta.url));
const repoRoot = fileURLToPath(new URL("..", import.meta.url));
const logRoot = fileURLToPath(new URL("../.dev/logs", import.meta.url));
const screenshotRoot = fileURLToPath(new URL("../.dev/web-screenshots", import.meta.url));
const screenshotLockPath = `${screenshotRoot}/.lock`;
const modelsDevFixture = `${repoRoot}/tests/web/k6/support/models-dev-api.fixture.json`;
const emptyConfigFixture = `${repoRoot}/tests/web/k6/support/empty-config.fixture.toml`;
const sessionsRuntimeConfigFixture = `${repoRoot}/tests/web/k6/support/sessions-runtime-config.fixture.toml`;
const screenshotSnapshotDir = `${repoRoot}/tests/web/k6/snapshots/screenshots`;
const BUILD_TIMEOUT_MS = 20 * 60_000;
const COMPOSE_UP_TIMEOUT_MS = 3 * 60_000;
const K6_TIMEOUT_MS = 10 * 60_000;
const DOCKER_TIMEOUT_MS = 60_000;
const COMPOSE_DOWN_TIMEOUT_MS = 30_000;
const HTTP_READY_TIMEOUT_MS = 30_000;
const CAPTURE_LIMIT = 10 * 1024 * 1024;
const PROCESS_GROUP_GRACE_MS = 2_000;
const PROCESS_GROUP_KILL_WAIT_MS = 5_000;
const PROCESS_GROUP_POLL_MS = 25;
const SCREENSHOT_LOCK_STALE_MS = 10_000;
const SCREENSHOT_LOCK_READ_RETRIES = 3;
const SCREENSHOT_LOCK_RETRY_MS = 25;
const SCREENSHOT_LOCK_ACQUIRE_ATTEMPTS = 8;
const SCREENSHOT_RUN_MARKER = ".waltr-web-screenshot-run.json";

export function parseCliArgs(args) {
  const options = {
    mode: "compose",
    access: "both",
    live: false,
    screenshots: false,
    updateScreenshots: false,
    script: null,
  };
  const seen = new Set();

  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--") {
      continue;
    }

    if (argument === "--mode" || argument === "--access" || argument === "--script") {
      if (seen.has(argument)) {
        throw new Error(`${argument} may only be provided once`);
      }
      const value = args[index + 1];
      if (!value || value.startsWith("--")) {
        throw new Error(`${argument} requires a value`);
      }
      seen.add(argument);
      index += 1;
      if (argument === "--mode") {
        options.mode = value;
      } else if (argument === "--access") {
        options.access = value;
      } else {
        options.script = value;
      }
      continue;
    }

    if (argument === "--live" || argument === "--screenshots" || argument === "--update-screenshots") {
      if (seen.has(argument)) {
        throw new Error(`${argument} may only be provided once`);
      }
      seen.add(argument);
      if (argument === "--live") {
        options.live = true;
      } else if (argument === "--screenshots") {
        options.screenshots = true;
      } else {
        options.updateScreenshots = true;
      }
      continue;
    }

    throw new Error(`unknown web integration argument: ${argument}`);
  }

  if (!new Set(["compose", "dev"]).has(options.mode)) {
    throw new Error(`--mode must be compose or dev, received ${options.mode}`);
  }
  if (!new Set(["both", "disabled", "oidc"]).has(options.access)) {
    throw new Error(`--access must be both, disabled, or oidc, received ${options.access}`);
  }
  if (options.mode === "dev" && options.access !== "disabled") {
    throw new Error("--mode dev requires --access disabled");
  }
  if (options.updateScreenshots && !options.screenshots) {
    throw new Error("--update-screenshots requires --screenshots");
  }

  return options;
}

export function cleanEnvironment(environment) {
  return Object.fromEntries(
    Object.entries(environment).filter(([name]) => (
      !name.startsWith("WALTR_") && !name.startsWith("K6_") && !name.startsWith("COMPOSE_")
    )),
  );
}

export function createHarnessRuntime(overrides = {}) {
  const platform = overrides.platform ?? process.platform;
  const runtime = {
    spawn,
    spawnSync,
    processKill: process.kill,
    setTimeout,
    clearTimeout,
    chmodSync,
    closeSync,
    copyFileSync,
    existsSync,
    fchmodSync,
    fstatSync,
    fsyncSync,
    linkSync,
    lstatSync,
    mkdirSync,
    mkdtempSync,
    openSync,
    readdirSync,
    readFileSync,
    readlinkSync,
    realpathSync,
    renameSync,
    rmdirSync,
    rmSync,
    symlinkSync,
    writeFileSync,
    randomBytes,
    environment: process.env,
    pid: process.pid,
    now: Date.now,
    platform,
    supportsProcessGroups: platform !== "win32",
    tempRoot: tmpdir(),
    logRoot,
    screenshotRoot,
    screenshotTrustRoot: repoRoot,
    screenshotLockPath,
    screenshotLockStaleMs: SCREENSHOT_LOCK_STALE_MS,
    sleepSync: defaultSleepSync,
    logger: console,
    ...overrides,
  };
  if (overrides.screenshotRoot && overrides.screenshotLockPath === undefined) {
    runtime.screenshotLockPath = `${runtime.screenshotRoot}/.lock`;
  }
  if (overrides.screenshotRoot && overrides.screenshotTrustRoot === undefined) {
    runtime.screenshotTrustRoot = dirname(resolve(runtime.screenshotRoot));
  }
  runtime.getProcessIdentity ??= (pid) => readLinuxProcessIdentity(pid);
  runtime.inspectProcessGroup ??= (identity) => inspectLinuxProcessGroup(identity, runtime);
  runtime.getDescriptorMountId ??= (descriptor) => readLinuxDescriptorMountId(descriptor, runtime);
  runtime.renameNoReplaceSync ??= (source, destination) => {
    const sourceAt = descriptorChildPath(source);
    const destinationAt = descriptorChildPath(destination);
    if (sourceAt || destinationAt) {
      if (!sourceAt || !destinationAt || sourceAt.descriptor !== destinationAt.descriptor) {
        throw new Error("atomic directory rename requires one verified parent descriptor");
      }
      return runLinuxDirectoryHelper(
        runtime,
        ["rename-noreplace-at", sourceAt.name, destinationAt.name],
        sourceAt.descriptor,
      );
    }
    return runLinuxDirectoryHelper(runtime, ["rename-noreplace", source, destination]);
  };
  runtime.removeDirectoryIfIdentitySync ??= (path, identity) => {
    const pathAt = descriptorChildPath(path);
    if (pathAt) {
      return runLinuxDirectoryHelper(
        runtime,
        ["remove-if-identity-at", pathAt.name, String(identity.device), String(identity.inode)],
        pathAt.descriptor,
      );
    }
    return runLinuxDirectoryHelper(
      runtime,
      ["remove-if-identity", path, String(identity.device), String(identity.inode)],
    );
  };
  return runtime;
}

function descriptorChildPath(path) {
  const match = /^\/proc\/self\/fd\/(\d+)\/([^/]+)$/.exec(path);
  return match ? { descriptor: Number(match[1]), name: match[2] } : null;
}

function runLinuxDirectoryHelper(runtime, args, inheritedDescriptor = null) {
  if (runtime.platform !== "linux") {
    throw new Error("identity-bound directory cleanup requires Linux");
  }
  const result = runtime.spawnSync("python3", [linuxDirectoryHelper, ...args], {
    encoding: "utf8",
    stdio: inheritedDescriptor === null
      ? ["ignore", "ignore", "pipe"]
      : ["ignore", "ignore", "pipe", inheritedDescriptor],
  });
  if (!result.error && result.status === 0 && result.signal === null) {
    return;
  }
  let details;
  try {
    details = JSON.parse(result.stderr?.trim() ?? "");
  } catch {
    details = null;
  }
  const error = result.error ?? new Error(
    details?.message ?? `Linux directory helper failed with ${result.signal ?? result.status}`,
  );
  if (details?.code) {
    error.code = details.code;
  }
  throw error;
}

function defaultSleepSync(milliseconds) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, milliseconds);
}

function readLinuxDescriptorMountId(descriptor, runtime) {
  if (runtime.platform !== "linux") {
    return null;
  }
  const contents = runtime.readFileSync(`/proc/self/fdinfo/${descriptor}`, "utf8");
  const match = contents.match(/^mnt_id:\s*(\d+)$/m);
  if (!match) {
    throw new Error(`could not read mount identity for descriptor ${descriptor}`);
  }
  return Number(match[1]);
}

function readLinuxProcessIdentity(pid) {
  if (process.platform !== "linux") {
    return null;
  }
  try {
    const value = readFileSync(`/proc/${pid}/stat`, "utf8");
    const commandEnd = value.lastIndexOf(") ");
    if (commandEnd < 0) {
      return null;
    }
    const fields = value.slice(commandEnd + 2).trim().split(/\s+/);
    const bootId = readFileSync("/proc/sys/kernel/random/boot_id", "utf8").trim();
    return {
      pid,
      processGroupId: Number(fields[2]),
      sessionId: Number(fields[3]),
      startIdentity: `${bootId}:${fields[19]}`,
    };
  } catch (error) {
    if (error.code === "ENOENT" || error.code === "ESRCH") {
      return null;
    }
    throw error;
  }
}

function inspectLinuxProcessGroup(identity, runtime) {
  if (!identity) {
    return { alive: false, reused: false };
  }
  if (runtime.platform !== "linux") {
    return { alive: !identity.leaderClosed, reused: false };
  }

  const leader = runtime.getProcessIdentity(identity.processGroupId);
  if (leader && (
    (identity.leaderStartIdentity !== null && leader.startIdentity !== identity.leaderStartIdentity) ||
    leader.processGroupId !== identity.processGroupId ||
    leader.sessionId !== identity.sessionId
  )) {
    return { alive: false, reused: true };
  }
  try {
    runtime.processKill(-identity.processGroupId, 0);
    return { alive: true, reused: false };
  } catch (error) {
    if (error.code === "ESRCH") {
      return { alive: false, reused: false };
    }
    if (error.code === "EPERM") {
      return { alive: true, reused: false };
    }
    throw error;
  }
}

export function integrationPhases(mode, access) {
  if (mode === "dev") {
    if (access !== "disabled") {
      throw new Error("dev integration runs only support disabled access authentication");
    }
    return ["disabled"];
  }
  if (mode !== "compose") {
    throw new Error(`unknown web integration mode: ${mode}`);
  }
  if (access === "both") {
    return ["disabled", "oidc"];
  }
  if (access === "disabled" || access === "oidc") {
    return [access];
  }
  throw new Error(`unknown web integration access selection: ${access}`);
}

export function profileForPhase(phase) {
  if (phase === "disabled") {
    return {
      profile: "test",
      waltrService: "waltr-test",
      k6Service: "k6",
      upServices: ["waltr-test"],
    };
  }
  if (phase === "oidc") {
    return {
      profile: "test-auth",
      waltrService: "waltr-auth",
      k6Service: "k6-auth",
      upServices: ["waltr-auth", "mcp-auth"],
    };
  }
  throw new Error(`unknown web integration phase: ${phase}`);
}

export function exitCodeFor({ testExitCode = 0, cleanupFailed = false, signalCode = null }) {
  if (signalCode !== null) {
    return signalCode;
  }
  if (testExitCode !== 0) {
    return testExitCode;
  }
  return cleanupFailed ? 1 : 0;
}

export function createCleanupOnce(cleanup) {
  let cleanupPromise;
  return () => {
    cleanupPromise ??= Promise.resolve().then(cleanup);
    return cleanupPromise;
  };
}

export async function executeWithCleanup({ execute, cleanup, getSignalCode = () => null }) {
  const cleanupOnce = createCleanupOnce(cleanup);
  let testExitCode = 0;
  let cleanupFailed = false;
  let executionError;
  let cleanupError;

  try {
    const result = await execute({ cleanup: cleanupOnce });
    if (typeof result === "number") {
      testExitCode = result;
    } else if (result) {
      testExitCode = result.testExitCode ?? 0;
      cleanupFailed = result.cleanupFailed ?? false;
    }
  } catch (error) {
    executionError = error;
    testExitCode = 1;
  }

  try {
    const cleaned = await cleanupOnce();
    cleanupFailed ||= cleaned === false;
  } catch (error) {
    cleanupError = error;
    cleanupFailed = true;
  }

  return {
    exitCode: exitCodeFor({ testExitCode, cleanupFailed, signalCode: getSignalCode() }),
    testExitCode,
    cleanupFailed,
    executionError,
    cleanupError,
  };
}

export function buildRunPlan(options) {
  const phases = integrationPhases(options.mode, options.access).map((phase) => ({
    phase,
    ...profileForPhase(phase),
    scripts: browserScriptsForPhase(options.mode, options.live, options.script, phase),
  })).filter(({ scripts }) => scripts.length > 0);

  if (options.script && phases.length === 0) {
    throw new Error(`selected web integration script is not enabled for this run: ${options.script}`);
  }
  return phases;
}

export async function runCli(args, dependencies = {}) {
  const logger = dependencies.logger ?? console;
  const runtime = dependencies.runtime ?? createHarnessRuntime({ logger });
  let options;
  let plan;

  try {
    options = parseCliArgs(args);
    plan = buildRunPlan(options);
  } catch (error) {
    logger.error(errorMessage(error));
    return 1;
  }

  const signalHandlers = (dependencies.installSignalHandlers ?? installSignalHandlers)(
    dependencies.signalProcess ?? process,
  );
  let state;
  try {
    try {
      state = await (dependencies.createContext ?? createRunContext)(options, plan, runtime);
    } catch (error) {
      if (signalHandlers.signalCode === null) {
        logger.error(errorMessage(error));
      }
      return signalHandlers.signalCode ?? 1;
    }
    state.runtime ??= runtime;
    signalHandlers.attach(state);

    const result = await executeWithCleanup({
      execute: () => (dependencies.execute ?? executeHarness)(state),
      cleanup: () => (dependencies.cleanup ?? cleanupHarness)(state),
      getSignalCode: () => state.signalCode ?? signalHandlers.signalCode,
    });

    if (result.executionError && state.signalCode === null) {
      logger.error(errorMessage(result.executionError));
    }
    if (result.cleanupError) {
      logger.error(`web integration cleanup failed: ${errorMessage(result.cleanupError)}`);
    }
    return result.exitCode;
  } catch (error) {
    logger.error(errorMessage(error));
    return signalHandlers.signalCode ?? 1;
  } finally {
    signalHandlers.remove();
  }
}

function browserScriptsForPhase(mode, live, selectedScript, phase) {
  const scripts = live ? ["tests/web/k6/sessions/session-live.browser.js"] : [
    "tests/web/k6/status/landing.browser.js",
    "tests/web/k6/sessions/sessions.browser.js",
    "tests/web/k6/config/watch.browser.js",
    "tests/web/k6/config/providers.browser.js",
    "tests/web/k6/config/mcp.browser.js",
    "tests/web/k6/config/defaults.browser.js",
  ];

  if (!live && mode === "dev") {
    scripts.splice(2, 0, "tests/web/k6/sessions/prompts.browser.js");
  }
  if (!live && phase === "oidc") {
    scripts.unshift(
      "tests/web/k6/access/trust.browser.js",
      "tests/web/k6/access/login.browser.js",
    );
  }
  if (!selectedScript) {
    return scripts;
  }

  const normalized = selectedScript.replace(/^\.\//, "");
  const match = scripts.find((script) => script === normalized || script.endsWith(`/${normalized}`));
  return match ? [match] : [];
}

export function createRunContext(options, plan, runtime = createHarnessRuntime()) {
  const runId = `${runtime.pid}-${runtime.now().toString(36)}-${runtime.randomBytes(4).toString("hex")}`;
  const runRootParent = resolve(runtime.tempRoot);
  const runRootParentIdentity = verifyPlainDirectory(
    runRootParent,
    runtime,
    "web integration root parent",
  ).fileIdentity;
  const runRoot = runtime.mkdtempSync(join(runtime.tempRoot, `waltr-web-integration-${runId}-`));
  let runRootOwnership = null;
  let screenshotLock = null;
  let screenshotPaths = null;
  let screenshotSetup = null;

  try {
    runRootOwnership = captureOwnedDirectory(
      runRoot,
      runRootParent,
      runRootParentIdentity,
      runtime,
      "web integration root",
    );
    const launchLogDir = `${runtime.logRoot}/web-integration-${runId}`;
    runtime.mkdirSync(launchLogDir, { recursive: true });
    let screenshotsDir = null;
    if (options.screenshots) {
      screenshotPaths = screenshotPathsForRun(runId, runtime);
      const screenshotLayout = prepareScreenshotLayout(screenshotPaths, runtime);
      screenshotLock = acquireScreenshotLock(
        screenshotPaths.lockPath,
        createScreenshotLockOwner(runId, runtime),
        runtime,
      );
      screenshotSetup = prepareScreenshotDirectories(
        screenshotPaths,
        screenshotLock.owner,
        screenshotLayout,
        runtime,
      );
      screenshotsDir = screenshotPaths.runDir;
    }

    const projectName = `waltr-web-integration-${runId}`;
    const devK6ContainerName = `${projectName}-dev-k6`;
    const devScreenshotContainerName = `${projectName}-dev-screenshot-permissions`;
    const state = {
      options,
      plan,
      runtime,
      runId,
      runRoot,
      runRootOwnership,
      launchLogDir,
      screenshotsDir,
      screenshotCurrentPath: screenshotPaths?.currentPath ?? null,
      screenshotLock,
      projectName,
      devK6ContainerName,
      devScreenshotContainerName,
      baseEnvironment: cleanEnvironment(runtime.environment),
      children: new Set(),
      directContainerNames: options.mode === "dev"
        ? new Set([devK6ContainerName, devScreenshotContainerName])
        : new Set(),
      abortController: new AbortController(),
      signalCode: null,
      activePhase: null,
      composeRunning: false,
      composeTeardownAttempted: false,
      diagnosticsCaptured: new Set(),
      shuttingDown: false,
      runtimeError: null,
      failure: false,
      processGroupOwnershipFailed: false,
    };

    runtime.logger.log(`web integration logs: ${launchLogDir}`);
    runtime.logger.log(`web integration server root: ${runRoot}`);
    runtime.logger.log(`web integration mode: ${options.mode}`);
    runtime.logger.log(`web integration access: ${options.access}`);
    if (options.script) {
      runtime.logger.log(`web integration script: ${options.script}`);
    }
    if (screenshotsDir) {
      runtime.logger.log(`web integration screenshots: ${screenshotsDir}`);
      runtime.logger.log(`web integration screenshot snapshots: ${screenshotSnapshotDir}`);
    }
    return state;
  } catch (error) {
    const cleanupErrors = [];
    try {
      if (screenshotSetup) {
        cleanupErrors.push(...removeIncompleteScreenshotRun(screenshotSetup, runtime));
      }
      if (screenshotLock) {
        try {
          if (!releaseScreenshotLock(screenshotLock, runtime)) {
            cleanupErrors.push(new Error(`could not release screenshot lock ${screenshotLock.path}`));
          }
        } catch (cleanupError) {
          cleanupErrors.push(cleanupError);
        }
      }
    } finally {
      try {
        if (!runRootOwnership) {
          cleanupErrors.push(new Error(`could not establish ownership of web integration root ${runRoot}`));
        } else if (!removeOwnedDirectoryTree(runRootOwnership, runtime)) {
          cleanupErrors.push(new Error(`could not safely remove web integration root ${runRoot}`));
        }
      } catch (cleanupError) {
        cleanupErrors.push(cleanupError);
      }
    }
    throwWithCleanupErrors(error, cleanupErrors, "partial web integration context cleanup");
  }
}

export function screenshotPathsForRun(runId, runtime = createHarnessRuntime()) {
  if (!/^[a-zA-Z0-9][a-zA-Z0-9-]*$/.test(runId)) {
    throw new Error(`invalid web screenshot run id: ${runId}`);
  }
  const root = resolve(runtime.screenshotRoot);
  const runsRoot = join(root, "runs");
  const runDir = join(runsRoot, runId);
  return {
    root,
    runsRoot,
    runDir,
    currentPath: join(root, "current"),
    currentTarget: relative(root, runDir),
    lockPath: resolve(runtime.screenshotLockPath),
    trustRoot: resolve(runtime.screenshotTrustRoot),
  };
}

export function createScreenshotLockOwner(runId, runtime = createHarnessRuntime()) {
  const identity = runtime.getProcessIdentity(runtime.pid);
  if (runtime.platform === "linux" && !identity) {
    throw new Error(`could not read process start identity for screenshot owner ${runtime.pid}`);
  }
  return {
    pid: runtime.pid,
    processStartIdentity: identity?.startIdentity ?? null,
    createdAtMs: runtime.now(),
    runId,
    token: runtime.randomBytes(16).toString("hex"),
  };
}

export function acquireScreenshotLock(path, owner, runtime = createHarnessRuntime()) {
  if (!validScreenshotLockOwner(owner)) {
    throw new Error("invalid web screenshot lock owner metadata");
  }
  const resolvedPath = resolve(path);
  const parentIdentity = verifyLockParent(resolvedPath, runtime);
  recoverStalePendingLocks(resolvedPath, parentIdentity, runtime);
  const pendingPath = `${resolvedPath}.pending-${owner.pid}-${owner.token}`;
  let descriptor = null;
  let pendingIdentity = null;
  let published = false;

  try {
    descriptor = runtime.openSync(pendingPath, "wx", 0o600);
    pendingIdentity = fileIdentityFromStat(runtime.fstatSync(descriptor));
    runtime.writeFileSync(descriptor, `${JSON.stringify(owner)}\n`);
    runtime.fsyncSync(descriptor);
    if (!sameFileIdentity(pendingIdentity, fileIdentityFromStat(runtime.fstatSync(descriptor)))) {
      throw new Error(`pending screenshot lock changed while writing ${pendingPath}`);
    }
    runtime.closeSync(descriptor);
    descriptor = null;

    for (let attempt = 0; attempt < SCREENSHOT_LOCK_ACQUIRE_ATTEMPTS; attempt += 1) {
      verifyLockParent(resolvedPath, runtime, parentIdentity);
      try {
        runtime.linkSync(pendingPath, resolvedPath);
        published = true;
      } catch (error) {
        if (error.code !== "EEXIST") {
          try {
            const publishedSnapshot = readScreenshotLockSnapshot(resolvedPath, runtime);
            if (publishedSnapshot?.owner?.token === owner.token &&
              sameFileIdentity(publishedSnapshot.fileIdentity, pendingIdentity)) {
              published = true;
            }
          } catch {
            // The acquisition error remains authoritative; cleanup stays fail-closed.
          }
          throw error;
        }
        const snapshot = readScreenshotLockSnapshot(resolvedPath, runtime);
        if (!snapshot) {
          continue;
        }
        const staleReason = screenshotLockStaleReason(snapshot, runtime);
        if (staleReason && quarantineScreenshotLock(snapshot, "stale", runtime, parentIdentity)) {
          continue;
        }
        if (snapshot.owner) {
          throw new Error(`web screenshot run ${snapshot.owner.runId} already holds ${resolvedPath}`);
        }
        const ageMs = Math.max(0, runtime.now() - snapshot.mtimeMs);
        throw new Error(
          `web screenshot lock metadata at ${resolvedPath} is incomplete (${Math.round(ageMs)}ms old); retry after ${runtime.screenshotLockStaleMs}ms`,
        );
      }

      const snapshot = readScreenshotLockSnapshot(resolvedPath, runtime);
      if (!snapshot || snapshot.owner?.token !== owner.token ||
        !sameFileIdentity(snapshot.fileIdentity, pendingIdentity)) {
        throw new Error(`web screenshot lock publication changed at ${resolvedPath}`);
      }
      if (!removeOwnedFile(pendingPath, pendingIdentity, runtime)) {
        throw new Error(`could not remove pending screenshot lock ${pendingPath}`);
      }
      return {
        path: resolvedPath,
        owner,
        fileIdentity: pendingIdentity,
        parentIdentity,
      };
    }
    throw new Error(`web screenshot lock changed too often while acquiring ${resolvedPath}`);
  } catch (error) {
    const cleanupErrors = [];
    if (descriptor !== null) {
      try {
        runtime.closeSync(descriptor);
      } catch (cleanupError) {
        cleanupErrors.push(cleanupError);
      }
    }
    if (published && pendingIdentity) {
      try {
        if (!releaseScreenshotLock({
          path: resolvedPath,
          owner,
          fileIdentity: pendingIdentity,
          parentIdentity,
        }, runtime)) {
          cleanupErrors.push(new Error(`could not retract screenshot lock ${resolvedPath}`));
        }
      } catch (cleanupError) {
        cleanupErrors.push(cleanupError);
      }
    }
    if (pendingIdentity) {
      try {
        if (!removeOwnedFile(pendingPath, pendingIdentity, runtime)) {
          cleanupErrors.push(new Error(`could not remove pending screenshot lock ${pendingPath}`));
        }
      } catch (cleanupError) {
        cleanupErrors.push(cleanupError);
      }
    }
    throwWithCleanupErrors(error, cleanupErrors, "screenshot lock acquisition rollback");
  }
}

function recoverStalePendingLocks(path, parentIdentity, runtime) {
  const parent = dirname(path);
  const prefix = `${basename(path)}.pending-`;
  verifyLockParent(path, runtime, parentIdentity);
  for (const entry of runtime.readdirSync(parent)) {
    if (!entry.startsWith(prefix)) {
      continue;
    }
    const pendingPath = join(parent, entry);
    assertDirectChild(parent, pendingPath, "pending screenshot lock");
    const snapshot = readScreenshotLockSnapshot(pendingPath, runtime);
    if (!snapshot) {
      continue;
    }
    const ageMs = Math.max(0, runtime.now() - snapshot.mtimeMs);
    const ownerIdentity = snapshot.owner ? runtime.getProcessIdentity(snapshot.owner.pid) : null;
    const ownerGone = snapshot.owner && (
      !ownerIdentity || ownerIdentity.startIdentity !== snapshot.owner.processStartIdentity
    );
    if (!ownerGone && ageMs < runtime.screenshotLockStaleMs) {
      continue;
    }
    if (!quarantineScreenshotLock(snapshot, "pending-stale", runtime, parentIdentity)) {
      throw new Error(`could not recover stale pending screenshot lock ${pendingPath}`);
    }
  }
}

export function releaseScreenshotLock(lock, runtime = createHarnessRuntime()) {
  if (!lock) {
    return true;
  }
  const snapshot = readScreenshotLockSnapshot(lock.path, runtime);
  if (!snapshot) {
    return true;
  }
  if (!snapshot.owner || snapshot.owner.token !== lock.owner.token ||
    !sameFileIdentity(snapshot.fileIdentity, lock.fileIdentity)) {
    return false;
  }
  return quarantineScreenshotLock(snapshot, "release", runtime, lock.parentIdentity);
}

function prepareScreenshotLayout(paths, runtime) {
  assertDirectChild(paths.root, paths.runsRoot, "screenshot runs root");
  assertDirectChild(paths.root, paths.currentPath, "screenshot current link");
  assertDirectChild(paths.root, paths.lockPath, "screenshot lock");
  assertDirectChild(paths.runsRoot, paths.runDir, "screenshot run directory");

  const trusted = verifyPlainDirectory(paths.trustRoot, runtime, "screenshot trust root");
  const trustedRealPath = runtime.realpathSync(paths.trustRoot);
  const rootIdentity = ensurePlainDirectoryChain(
    paths.trustRoot,
    paths.root,
    trustedRealPath,
    runtime,
    "screenshot root",
  );
  const rootRealPath = runtime.realpathSync(paths.root);
  const runsRootIdentity = ensurePlainDirectoryChain(
    paths.root,
    paths.runsRoot,
    rootRealPath,
    runtime,
    "screenshot runs root",
  );
  return {
    trustedIdentity: trusted.fileIdentity,
    trustedRealPath,
    rootIdentity,
    runsRootIdentity,
  };
}

function prepareScreenshotDirectories(paths, owner, layout, runtime) {
  const setup = {
    paths,
    layout,
    owner,
    previousCurrent: null,
    runOwnership: null,
    currentIdentity: null,
    currentReplaced: false,
  };

  try {
    verifyScreenshotLayout(paths, layout, runtime);
    setup.previousCurrent = readScreenshotCurrent(paths, layout, runtime);
    if (lstatIfExists(paths.runDir, runtime)) {
      throw new Error(`refusing to replace existing screenshot run directory ${paths.runDir}`);
    }

    runtime.mkdirSync(paths.runDir, { mode: 0o777 });
    const runStat = verifyPlainDirectory(paths.runDir, runtime, "screenshot run directory");
    assertPathContained(runtime.realpathSync(paths.runsRoot), runtime.realpathSync(paths.runDir), false, "screenshot run directory");
    setup.runOwnership = {
      path: paths.runDir,
      runId: owner.runId,
      token: owner.token,
      fileIdentity: runStat.fileIdentity,
      allowUnmarked: true,
    };
    writeScreenshotRunMarker(setup.runOwnership, runtime);
    setup.runOwnership.allowUnmarked = false;
    runtime.chmodSync(paths.runDir, 0o777);

    replaceScreenshotCurrent(setup, runtime);
    pruneOwnedScreenshotRuns(setup, runtime);
    return setup;
  } catch (error) {
    const cleanupErrors = removeIncompleteScreenshotRun(setup, runtime);
    throwWithCleanupErrors(error, cleanupErrors, "screenshot setup rollback");
  }
}

function removeIncompleteScreenshotRun(setup, runtime) {
  const cleanupErrors = [];
  let currentSafe = false;
  try {
    currentSafe = rollbackScreenshotCurrent(setup, runtime);
    if (!currentSafe) {
      cleanupErrors.push(new Error(`could not restore screenshot current link ${setup.paths.currentPath}`));
    }
  } catch (error) {
    cleanupErrors.push(error);
  }
  if (currentSafe && setup.runOwnership) {
    try {
      removeOwnedScreenshotRun(setup.paths, setup.layout, setup.runOwnership, "rollback", runtime);
    } catch (error) {
      cleanupErrors.push(error);
    }
  }
  return cleanupErrors;
}

function replaceScreenshotCurrent(setup, runtime) {
  const { paths, layout, owner, previousCurrent } = setup;
  verifyScreenshotLayout(paths, layout, runtime);
  verifyCurrentUnchanged(paths, previousCurrent, runtime);
  const nextCurrent = join(paths.root, `.current-next-${owner.token}`);
  if (lstatIfExists(nextCurrent, runtime)) {
    throw new Error(`refusing to replace existing screenshot current staging path ${nextCurrent}`);
  }

  runtime.symlinkSync(paths.currentTarget, nextCurrent, "dir");
  const nextStat = runtime.lstatSync(nextCurrent);
  const nextIdentity = fileIdentityFromStat(nextStat);
  if (!nextStat.isSymbolicLink()) {
    throw new Error(`screenshot current staging path is not a symlink: ${nextCurrent}`);
  }
  try {
    verifyScreenshotLayout(paths, layout, runtime);
    verifyCurrentUnchanged(paths, previousCurrent, runtime);
    try {
      // Replacing current is the publication; its inode and target are checked below.
      runtime.renameSync(nextCurrent, paths.currentPath);
    } catch (error) {
      const currentAfterError = lstatIfExists(paths.currentPath, runtime);
      if (currentAfterError?.isSymbolicLink() &&
        sameFileIdentity(fileIdentityFromStat(currentAfterError), nextIdentity) &&
        runtime.readlinkSync(paths.currentPath) === paths.currentTarget) {
        setup.currentIdentity = nextIdentity;
        setup.currentReplaced = true;
      }
      throw error;
    }
    const currentStat = runtime.lstatSync(paths.currentPath);
    setup.currentIdentity = nextIdentity;
    setup.currentReplaced = true;
    if (!currentStat.isSymbolicLink() ||
      !sameFileIdentity(fileIdentityFromStat(currentStat), nextIdentity) ||
      runtime.readlinkSync(paths.currentPath) !== paths.currentTarget) {
      throw new Error(`screenshot current link changed during replacement: ${paths.currentPath}`);
    }
  } catch (error) {
    try {
      removeOwnedSymlink(nextCurrent, nextIdentity, runtime);
    } catch (cleanupError) {
      runtime.logger.error(`failed to remove screenshot current staging link: ${errorMessage(cleanupError)}`);
    }
    throw error;
  }
}

function rollbackScreenshotCurrent(setup, runtime) {
  if (!setup.currentReplaced) {
    return true;
  }
  const { paths, layout, owner, previousCurrent, currentIdentity } = setup;
  verifyScreenshotLayout(paths, layout, runtime);
  const current = runtime.lstatSync(paths.currentPath);
  if (!current.isSymbolicLink() ||
    !sameFileIdentity(fileIdentityFromStat(current), currentIdentity) ||
    runtime.readlinkSync(paths.currentPath) !== paths.currentTarget) {
    return false;
  }

  if (!previousCurrent) {
    const removedPath = join(paths.root, `.current-rollback-${owner.token}`);
    if (lstatIfExists(removedPath, runtime)) {
      return false;
    }
    verifyScreenshotLayout(paths, layout, runtime);
    try {
      runtime.renameNoReplaceSync(paths.currentPath, removedPath);
    } catch (error) {
      const movedAfterError = lstatIfExists(removedPath, runtime);
      if (!movedAfterError?.isSymbolicLink() ||
        !sameFileIdentity(fileIdentityFromStat(movedAfterError), currentIdentity) ||
        lstatIfExists(paths.currentPath, runtime)) {
        throw error;
      }
    }
    const moved = runtime.lstatSync(removedPath);
    if (!moved.isSymbolicLink() || !sameFileIdentity(fileIdentityFromStat(moved), currentIdentity)) {
      return false;
    }
    return removeOwnedSymlink(removedPath, currentIdentity, runtime);
  }

  verifyPreviousCurrentTarget(paths, previousCurrent, runtime);
  const restorePath = join(paths.root, `.current-restore-${owner.token}`);
  if (lstatIfExists(restorePath, runtime)) {
    return false;
  }
  runtime.symlinkSync(previousCurrent.target, restorePath, "dir");
  const restoreIdentity = fileIdentityFromStat(runtime.lstatSync(restorePath));
  try {
    const beforeRestore = runtime.lstatSync(paths.currentPath);
    if (!beforeRestore.isSymbolicLink() ||
      !sameFileIdentity(fileIdentityFromStat(beforeRestore), currentIdentity)) {
      return false;
    }
    try {
      // Restoring current intentionally replaces our verified publication.
      runtime.renameSync(restorePath, paths.currentPath);
    } catch (error) {
      const restoredAfterError = lstatIfExists(paths.currentPath, runtime);
      if (!restoredAfterError?.isSymbolicLink() ||
        !sameFileIdentity(fileIdentityFromStat(restoredAfterError), restoreIdentity) ||
        runtime.readlinkSync(paths.currentPath) !== previousCurrent.target) {
        throw error;
      }
    }
    const restored = runtime.lstatSync(paths.currentPath);
    return restored.isSymbolicLink() &&
      sameFileIdentity(fileIdentityFromStat(restored), restoreIdentity) &&
      runtime.readlinkSync(paths.currentPath) === previousCurrent.target;
  } finally {
    removeOwnedSymlink(restorePath, restoreIdentity, runtime);
  }
}

function readScreenshotCurrent(paths, layout, runtime) {
  verifyScreenshotLayout(paths, layout, runtime);
  const before = lstatIfExists(paths.currentPath, runtime);
  if (!before) {
    return null;
  }
  if (!before.isSymbolicLink()) {
    throw new Error(`refusing non-symlink screenshot current path ${paths.currentPath}`);
  }
  const target = runtime.readlinkSync(paths.currentPath);
  const after = runtime.lstatSync(paths.currentPath);
  if (!after.isSymbolicLink() ||
    !sameFileIdentity(fileIdentityFromStat(before), fileIdentityFromStat(after))) {
    throw new Error(`screenshot current link changed while inspecting ${paths.currentPath}`);
  }
  const targetPath = resolve(paths.root, target);
  assertDirectChild(paths.runsRoot, targetPath, "screenshot current target");
  const targetStat = verifyPlainDirectory(targetPath, runtime, "screenshot current target");
  assertPathContained(runtime.realpathSync(paths.runsRoot), runtime.realpathSync(targetPath), false, "screenshot current target");
  return {
    target,
    targetPath,
    runName: basename(targetPath),
    fileIdentity: fileIdentityFromStat(after),
    targetIdentity: targetStat.fileIdentity,
  };
}

function verifyCurrentUnchanged(paths, previousCurrent, runtime) {
  const current = lstatIfExists(paths.currentPath, runtime);
  if (!previousCurrent) {
    if (current) {
      throw new Error(`screenshot current path appeared during setup: ${paths.currentPath}`);
    }
    return;
  }
  if (!current?.isSymbolicLink() ||
    !sameFileIdentity(fileIdentityFromStat(current), previousCurrent.fileIdentity) ||
    runtime.readlinkSync(paths.currentPath) !== previousCurrent.target) {
    throw new Error(`screenshot current link changed during setup: ${paths.currentPath}`);
  }
}

function verifyPreviousCurrentTarget(paths, previousCurrent, runtime) {
  const target = verifyPlainDirectory(previousCurrent.targetPath, runtime, "previous screenshot current target");
  if (!sameFileIdentity(target.fileIdentity, previousCurrent.targetIdentity)) {
    throw new Error(`previous screenshot current target changed: ${previousCurrent.targetPath}`);
  }
  assertPathContained(runtime.realpathSync(paths.runsRoot), runtime.realpathSync(previousCurrent.targetPath), false, "previous screenshot current target");
}

function writeScreenshotRunMarker(ownership, runtime) {
  const markerPath = join(ownership.path, SCREENSHOT_RUN_MARKER);
  let descriptor = null;
  try {
    descriptor = runtime.openSync(markerPath, "wx", 0o600);
    runtime.writeFileSync(descriptor, `${JSON.stringify({
      version: 1,
      runId: ownership.runId,
      token: ownership.token,
    })}\n`);
    runtime.fsyncSync(descriptor);
    runtime.closeSync(descriptor);
    descriptor = null;
  } finally {
    if (descriptor !== null) {
      runtime.closeSync(descriptor);
    }
  }
}

function pruneOwnedScreenshotRuns(setup, runtime) {
  const { paths, layout, previousCurrent, runOwnership } = setup;
  verifyScreenshotLayout(paths, layout, runtime);
  for (const entry of runtime.readdirSync(paths.runsRoot)) {
    if (entry === basename(paths.runDir) || entry === previousCurrent?.runName) {
      continue;
    }
    const path = join(paths.runsRoot, entry);
    const stat = runtime.lstatSync(path);
    if (stat.isSymbolicLink()) {
      throw new Error(`refusing symlinked screenshot run entry ${path}`);
    }
    if (!stat.isDirectory()) {
      continue;
    }
    const ownership = readScreenshotRunOwnership(paths, path, runtime);
    if (!ownership) {
      continue;
    }
    removeOwnedScreenshotRun(paths, layout, ownership, "prune", runtime);
  }
  verifyOwnedScreenshotRun(paths, runOwnership, runtime);
}

function readScreenshotRunOwnership(paths, path, runtime) {
  const directory = verifyPlainDirectory(path, runtime, "screenshot run entry");
  assertDirectChild(paths.runsRoot, path, "screenshot run entry");
  assertPathContained(runtime.realpathSync(paths.runsRoot), runtime.realpathSync(path), false, "screenshot run entry");
  const markerPath = join(path, SCREENSHOT_RUN_MARKER);
  const before = lstatIfExists(markerPath, runtime);
  if (!before) {
    return null;
  }
  if (before.isSymbolicLink() || !before.isFile()) {
    throw new Error(`refusing unsafe screenshot run marker ${markerPath}`);
  }
  const contents = runtime.readFileSync(markerPath, "utf8");
  const after = runtime.lstatSync(markerPath);
  if (!after.isFile() ||
    !sameFileIdentity(fileIdentityFromStat(before), fileIdentityFromStat(after))) {
    throw new Error(`screenshot run marker changed while inspecting ${markerPath}`);
  }
  let marker;
  try {
    marker = JSON.parse(contents);
  } catch {
    return null;
  }
  if (marker?.version !== 1 || marker.runId !== basename(path) ||
    typeof marker.token !== "string" || marker.token.length < 16) {
    return null;
  }
  return {
    path,
    runId: marker.runId,
    token: marker.token,
    fileIdentity: directory.fileIdentity,
    allowUnmarked: false,
  };
}

function verifyOwnedScreenshotRun(paths, ownership, runtime) {
  assertDirectChild(paths.runsRoot, ownership.path, "owned screenshot run");
  const directory = verifyPlainDirectory(ownership.path, runtime, "owned screenshot run");
  if (!sameFileIdentity(directory.fileIdentity, ownership.fileIdentity)) {
    throw new Error(`owned screenshot run changed: ${ownership.path}`);
  }
  assertPathContained(runtime.realpathSync(paths.runsRoot), runtime.realpathSync(ownership.path), false, "owned screenshot run");
  if (ownership.allowUnmarked) {
    return;
  }
  const current = readScreenshotRunOwnership(paths, ownership.path, runtime);
  if (!current || current.token !== ownership.token ||
    !sameFileIdentity(current.fileIdentity, ownership.fileIdentity)) {
    throw new Error(`screenshot run ownership marker changed: ${ownership.path}`);
  }
}

function removeOwnedScreenshotRun(paths, layout, ownership, purpose, runtime) {
  verifyScreenshotLayout(paths, layout, runtime);
  verifyOwnedScreenshotRun(paths, ownership, runtime);
  const quarantinePath = join(
    paths.runsRoot,
    `.${purpose}-${ownership.runId}-${runtime.randomBytes(8).toString("hex")}`,
  );
  if (lstatIfExists(quarantinePath, runtime)) {
    throw new Error(`screenshot run quarantine path already exists: ${quarantinePath}`);
  }
  verifyScreenshotLayout(paths, layout, runtime);
  verifyOwnedScreenshotRun(paths, ownership, runtime);
  runtime.renameNoReplaceSync(ownership.path, quarantinePath);
  const moved = verifyPlainDirectory(quarantinePath, runtime, "quarantined screenshot run");
  if (!sameFileIdentity(moved.fileIdentity, ownership.fileIdentity)) {
    throw new Error(`screenshot run changed while quarantining ${ownership.path}`);
  }
  assertPathContained(runtime.realpathSync(paths.runsRoot), runtime.realpathSync(quarantinePath), false, "quarantined screenshot run");
  verifyScreenshotLayout(paths, layout, runtime);
  const beforeRemoval = verifyPlainDirectory(quarantinePath, runtime, "quarantined screenshot run");
  if (!sameFileIdentity(beforeRemoval.fileIdentity, ownership.fileIdentity)) {
    throw new Error(`quarantined screenshot run changed before removal: ${quarantinePath}`);
  }
  removeOwnedFlatDirectory({
    path: quarantinePath,
    parent: paths.runsRoot,
    parentIdentity: layout.runsRootIdentity,
    fileIdentity: ownership.fileIdentity,
    label: "quarantined screenshot run",
  }, null, runtime);
}

function readScreenshotLockSnapshot(path, runtime) {
  let lastSnapshot;
  for (let attempt = 0; attempt < SCREENSHOT_LOCK_READ_RETRIES; attempt += 1) {
    try {
      const before = runtime.lstatSync(path);
      if (before.isSymbolicLink() || !before.isFile()) {
        throw new Error(`refusing unsafe web screenshot lock path ${path}`);
      }
      const contents = runtime.readFileSync(path, "utf8");
      const after = runtime.lstatSync(path);
      if (after.isSymbolicLink() || !after.isFile()) {
        throw new Error(`web screenshot lock changed to an unsafe file at ${path}`);
      }
      if (!sameFileIdentity(fileIdentityFromStat(before), fileIdentityFromStat(after))) {
        lastSnapshot = null;
      } else {
        let owner = null;
        try {
          const candidate = JSON.parse(contents);
          owner = validScreenshotLockOwner(candidate) ? candidate : null;
        } catch {
          // Legacy malformed locks are recoverable only after the stale threshold.
        }
        lastSnapshot = {
          path,
          owner,
          contents,
          fileIdentity: fileIdentityFromStat(after),
          mtimeMs: after.mtimeMs,
        };
        if (owner) {
          return lastSnapshot;
        }
      }
    } catch (error) {
      if (error.code === "ENOENT") {
        return null;
      }
      throw error;
    }
    if (attempt + 1 < SCREENSHOT_LOCK_READ_RETRIES) {
      runtime.sleepSync(SCREENSHOT_LOCK_RETRY_MS);
    }
  }
  return lastSnapshot;
}

function screenshotLockStaleReason(snapshot, runtime) {
  if (!snapshot.owner) {
    return runtime.now() - snapshot.mtimeMs >= runtime.screenshotLockStaleMs ? "malformed" : null;
  }
  const identity = runtime.getProcessIdentity(snapshot.owner.pid);
  if (!identity) {
    return "owner exited";
  }
  if (identity.startIdentity !== snapshot.owner.processStartIdentity) {
    return "pid reused";
  }
  return null;
}

function quarantineScreenshotLock(snapshot, purpose, runtime, expectedParentIdentity = null) {
  const parentIdentity = verifyLockParent(snapshot.path, runtime, expectedParentIdentity);
  const quarantinePath = `${snapshot.path}.${purpose}-${runtime.pid}-${runtime.randomBytes(8).toString("hex")}`;
  if (lstatIfExists(quarantinePath, runtime)) {
    throw new Error(`screenshot lock quarantine path already exists: ${quarantinePath}`);
  }
  try {
    verifyLockParent(snapshot.path, runtime, parentIdentity);
    runtime.renameNoReplaceSync(snapshot.path, quarantinePath);
  } catch (error) {
    if (error.code === "ENOENT") {
      return false;
    }
    throw error;
  }

  let movedIdentity;
  try {
    const moved = runtime.lstatSync(quarantinePath);
    if (moved.isSymbolicLink() || !moved.isFile()) {
      throw new Error(`quarantined screenshot lock is unsafe: ${quarantinePath}`);
    }
    movedIdentity = fileIdentityFromStat(moved);
  } catch (error) {
    if (error.code === "ENOENT") {
      return false;
    }
    throw error;
  }
  if (!sameFileIdentity(snapshot.fileIdentity, movedIdentity)) {
    restoreQuarantinedLock(quarantinePath, snapshot.path, runtime);
    return false;
  }
  const beforeRemoval = runtime.lstatSync(quarantinePath);
  if (beforeRemoval.isSymbolicLink() || !beforeRemoval.isFile() ||
    !sameFileIdentity(fileIdentityFromStat(beforeRemoval), movedIdentity)) {
    return false;
  }
  runtime.rmSync(quarantinePath, { force: true });
  return true;
}

function restoreQuarantinedLock(quarantinePath, path, runtime) {
  try {
    runtime.linkSync(quarantinePath, path);
    runtime.rmSync(quarantinePath, { force: true });
  } catch (error) {
    runtime.logger.error(`could not restore changed screenshot lock ${quarantinePath}: ${errorMessage(error)}`);
  }
}

function verifyLockParent(path, runtime, expectedIdentity = null) {
  const parent = dirname(resolve(path));
  const stat = verifyPlainDirectory(parent, runtime, "screenshot lock parent");
  if (expectedIdentity && !sameFileIdentity(stat.fileIdentity, expectedIdentity)) {
    throw new Error(`screenshot lock parent changed: ${parent}`);
  }
  return stat.fileIdentity;
}

function removeOwnedFile(path, identity, runtime) {
  const stat = lstatIfExists(path, runtime);
  if (!stat) {
    return true;
  }
  if (stat.isSymbolicLink() || !stat.isFile() ||
    !sameFileIdentity(fileIdentityFromStat(stat), identity)) {
    return false;
  }
  runtime.rmSync(path, { force: true });
  return true;
}

function removeOwnedSymlink(path, identity, runtime) {
  const stat = lstatIfExists(path, runtime);
  if (!stat) {
    return true;
  }
  if (!stat.isSymbolicLink() || !sameFileIdentity(fileIdentityFromStat(stat), identity)) {
    return false;
  }
  runtime.rmSync(path, { force: true });
  return true;
}

function ensurePlainDirectoryChain(base, target, baseRealPath, runtime, label) {
  const relativePath = relative(resolve(base), resolve(target));
  if (!relativePath || relativePath === ".." || relativePath.startsWith(`..${sep}`) || isAbsolute(relativePath)) {
    throw new Error(`${label} must be contained below ${base}: ${target}`);
  }
  let current = resolve(base);
  for (const component of relativePath.split(sep)) {
    current = join(current, component);
    let stat = lstatIfExists(current, runtime);
    if (!stat) {
      runtime.mkdirSync(current);
      stat = runtime.lstatSync(current);
    }
    if (stat.isSymbolicLink() || !stat.isDirectory()) {
      throw new Error(`refusing symlinked or non-directory ${label}: ${current}`);
    }
    assertPathContained(baseRealPath, runtime.realpathSync(current), false, label);
  }
  return fileIdentityFromStat(runtime.lstatSync(target));
}

function verifyScreenshotLayout(paths, layout, runtime) {
  const trusted = verifyPlainDirectory(paths.trustRoot, runtime, "screenshot trust root");
  if (!sameFileIdentity(trusted.fileIdentity, layout.trustedIdentity)) {
    throw new Error(`screenshot trust root changed: ${paths.trustRoot}`);
  }
  const root = verifyPlainDirectory(paths.root, runtime, "screenshot root");
  if (!sameFileIdentity(root.fileIdentity, layout.rootIdentity)) {
    throw new Error(`screenshot root changed: ${paths.root}`);
  }
  assertPathContained(layout.trustedRealPath, runtime.realpathSync(paths.root), false, "screenshot root");
  const runsRoot = verifyPlainDirectory(paths.runsRoot, runtime, "screenshot runs root");
  if (!sameFileIdentity(runsRoot.fileIdentity, layout.runsRootIdentity)) {
    throw new Error(`screenshot runs root changed: ${paths.runsRoot}`);
  }
  assertPathContained(runtime.realpathSync(paths.root), runtime.realpathSync(paths.runsRoot), false, "screenshot runs root");
}

function verifyPlainDirectory(path, runtime, label) {
  const stat = runtime.lstatSync(path);
  if (stat.isSymbolicLink() || !stat.isDirectory()) {
    throw new Error(`refusing symlinked or non-directory ${label}: ${path}`);
  }
  return { stat, fileIdentity: fileIdentityFromStat(stat) };
}

function captureOwnedDirectory(path, parent, parentIdentity, runtime, label) {
  assertDirectChild(parent, path, label);
  const actualParent = verifyPlainDirectory(parent, runtime, `${label} parent`);
  if (!sameFileIdentity(actualParent.fileIdentity, parentIdentity)) {
    throw new Error(`${label} parent changed: ${parent}`);
  }
  const directory = verifyPlainDirectory(path, runtime, label);
  assertPathContained(runtime.realpathSync(parent), runtime.realpathSync(path), false, label);
  return {
    path: resolve(path),
    parent: resolve(parent),
    parentIdentity,
    fileIdentity: directory.fileIdentity,
    label,
  };
}

function verifyOwnedDirectoryIdentity(ownership, runtime) {
  const current = captureOwnedDirectory(
    ownership.path,
    ownership.parent,
    ownership.parentIdentity,
    runtime,
    ownership.label,
  );
  if (!sameFileIdentity(current.fileIdentity, ownership.fileIdentity)) {
    throw new Error(`${ownership.label} changed: ${ownership.path}`);
  }
}

function openOwnedDirectory(ownership, runtime) {
  verifyOwnedDirectoryIdentity(ownership, runtime);
  const descriptor = runtime.openSync(
    ownership.path,
    fsConstants.O_RDONLY | fsConstants.O_DIRECTORY | fsConstants.O_NOFOLLOW,
  );
  const stat = runtime.fstatSync(descriptor);
  if (!stat.isDirectory() || !sameFileIdentity(fileIdentityFromStat(stat), ownership.fileIdentity)) {
    runtime.closeSync(descriptor);
    throw new Error(`${ownership.label} changed while opening ${ownership.path}`);
  }
  if (runtime.platform !== "linux") {
    runtime.closeSync(descriptor);
    throw new Error(`descriptor-bound ${ownership.label} cleanup requires Linux`);
  }
  let mountId;
  try {
    mountId = runtime.getDescriptorMountId(descriptor);
  } catch (error) {
    runtime.closeSync(descriptor);
    throw error;
  }
  if (!Number.isInteger(mountId)) {
    runtime.closeSync(descriptor);
    throw new Error(`could not read mount identity for ${ownership.label} ${ownership.path}`);
  }
  return { descriptor, descriptorPath: `/proc/self/fd/${descriptor}`, mountId };
}

function removeOwnedDirectoryTree(ownership, runtime) {
  if (!ownership) {
    return false;
  }
  const current = lstatIfExists(ownership.path, runtime);
  if (!current) {
    return true;
  }
  let opened;
  try {
    opened = openOwnedDirectory(ownership, runtime);
    removeDescriptorDirectoryContents(opened, runtime, ownership.label);
    removeOpenedOwnedDirectory(ownership, opened, runtime);
  } finally {
    if (opened) {
      runtime.closeSync(opened.descriptor);
    }
  }
  return true;
}

function removeDescriptorDirectoryContents(openedDirectory, runtime, label) {
  for (const entry of runtime.readdirSync(openedDirectory.descriptorPath, { withFileTypes: true })) {
    const entryPath = join(openedDirectory.descriptorPath, entry.name);
    const before = runtime.lstatSync(entryPath);
    if (before.isSymbolicLink() || before.isFile()) {
      runtime.rmSync(entryPath, { force: true });
      continue;
    }
    if (!before.isDirectory()) {
      throw new Error(`refusing unexpected ${label} entry ${entryPath}`);
    }
    const childIdentity = fileIdentityFromStat(before);
    const childDescriptor = runtime.openSync(
      entryPath,
      fsConstants.O_RDONLY | fsConstants.O_DIRECTORY | fsConstants.O_NOFOLLOW,
    );
    try {
      const opened = runtime.fstatSync(childDescriptor);
      if (!opened.isDirectory() || !sameFileIdentity(fileIdentityFromStat(opened), childIdentity)) {
        throw new Error(`${label} child changed while opening ${entryPath}`);
      }
      const childMountId = runtime.getDescriptorMountId(childDescriptor);
      if (!Number.isInteger(childMountId)) {
        throw new Error(`could not read mount identity for ${label} child ${entryPath}`);
      }
      if (childMountId !== openedDirectory.mountId) {
        throw new Error(`refusing to cross mount boundary while removing ${label} child ${entryPath}`);
      }
      const openedChild = {
        descriptor: childDescriptor,
        descriptorPath: `/proc/self/fd/${childDescriptor}`,
        mountId: childMountId,
      };
      removeDescriptorDirectoryContents(openedChild, runtime, label);
      removeOpenedDirectoryEntry(
        entryPath,
        childIdentity,
        openedDirectory,
        openedChild,
        runtime,
        `${label} child`,
      );
    } finally {
      runtime.closeSync(childDescriptor);
    }
  }
}

function removeOwnedFlatDirectory(ownership, expectedFiles, runtime) {
  if (!lstatIfExists(ownership.path, runtime)) {
    return true;
  }
  let opened;
  try {
    opened = openOwnedDirectory(ownership, runtime);
    const entries = runtime.readdirSync(opened.descriptorPath).sort();
    if (expectedFiles && (
      entries.length !== expectedFiles.size || entries.some((entry) => !expectedFiles.has(entry))
    )) {
      throw new Error(`unexpected entries in ${ownership.label} ${ownership.path}`);
    }
    const verified = new Map();
    for (const entry of entries) {
      const entryPath = join(opened.descriptorPath, entry);
      const stat = runtime.lstatSync(entryPath);
      const identity = fileIdentityFromStat(stat);
      if (stat.isSymbolicLink() || !stat.isFile()) {
        throw new Error(`refusing unexpected ${ownership.label} entry ${entryPath}`);
      }
      if (expectedFiles && !sameFileIdentity(expectedFiles.get(entry), identity)) {
        throw new Error(`${ownership.label} entry changed: ${entry}`);
      }
      verified.set(entry, identity);
    }
    for (const [entry, identity] of verified) {
      const entryPath = join(opened.descriptorPath, entry);
      const stat = runtime.lstatSync(entryPath);
      if (!stat.isFile() || stat.isSymbolicLink() ||
        !sameFileIdentity(fileIdentityFromStat(stat), identity)) {
        throw new Error(`${ownership.label} entry changed before removal: ${entry}`);
      }
      runtime.rmSync(entryPath, { force: true });
    }
    removeOpenedOwnedDirectory(ownership, opened, runtime);
  } finally {
    if (opened) {
      runtime.closeSync(opened.descriptor);
    }
  }
  return true;
}

function removeOpenedOwnedDirectory(ownership, opened, runtime) {
  const verifyParent = () => {
    const parent = verifyPlainDirectory(ownership.parent, runtime, `${ownership.label} parent`);
    if (!sameFileIdentity(parent.fileIdentity, ownership.parentIdentity)) {
      throw new Error(`${ownership.label} parent changed: ${ownership.parent}`);
    }
  };
  removeOpenedDirectoryEntry(
    ownership.path,
    ownership.fileIdentity,
    { descriptorPath: ownership.parent, mountId: opened.mountId },
    opened,
    runtime,
    ownership.label,
    verifyParent,
  );
}

function removeOpenedDirectoryEntry(
  path,
  identity,
  parentOpened,
  opened,
  runtime,
  label,
  verifyParent = null,
) {
  const parentPath = parentOpened.descriptorPath;
  const quarantinePath = join(
    parentPath,
    `.remove-${basename(path)}-${runtime.randomBytes(8).toString("hex")}`,
  );
  const verifyOpenParent = () => {
    if (verifyParent) {
      verifyParent();
    } else {
      const parentStat = runtime.fstatSync(parentOpened.descriptor);
      if (!parentStat.isDirectory() || runtime.getDescriptorMountId(parentOpened.descriptor) !== parentOpened.mountId) {
        throw new Error(`${label} parent changed before removal: ${parentPath}`);
      }
    }
  };
  verifyOpenParent();
  const before = runtime.lstatSync(path);
  if (!before.isDirectory() || !sameFileIdentity(fileIdentityFromStat(before), identity)) {
    throw new Error(`${label} changed before quarantine: ${path}`);
  }
  const openedBefore = runtime.fstatSync(opened.descriptor);
  if (!openedBefore.isDirectory() || !sameFileIdentity(fileIdentityFromStat(openedBefore), identity) ||
    runtime.getDescriptorMountId(opened.descriptor) !== opened.mountId) {
    throw new Error(`${label} descriptor changed before quarantine: ${path}`);
  }
  verifyOpenParent();
  try {
    runtime.renameNoReplaceSync(path, quarantinePath);
  } catch (error) {
    throw new Error(`${label} could not be quarantined without replacement: ${errorMessage(error)}`, {
      cause: error,
    });
  }
  const moved = lstatIfExists(quarantinePath, runtime);
  const source = lstatIfExists(path, runtime);
  if (!moved?.isDirectory() || !sameFileIdentity(fileIdentityFromStat(moved), identity) || source) {
    if (moved && !source) {
      try {
        runtime.renameNoReplaceSync(quarantinePath, path);
      } catch (restoreError) {
        throw new AggregateError(
          [restoreError],
          `${label} changed while quarantining and could not be restored: ${path}`,
        );
      }
    }
    throw new Error(`${label} changed while quarantining: ${path}`);
  }
  const openedAfter = runtime.fstatSync(opened.descriptor);
  if (!openedAfter.isDirectory() || !sameFileIdentity(fileIdentityFromStat(openedAfter), identity) ||
    runtime.getDescriptorMountId(opened.descriptor) !== opened.mountId) {
    throw new Error(`${label} descriptor changed after quarantine: ${quarantinePath}`);
  }
  runtime.removeDirectoryIfIdentitySync(quarantinePath, identity);
}

function lstatIfExists(path, runtime) {
  try {
    return runtime.lstatSync(path);
  } catch (error) {
    if (error.code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

function assertDirectChild(parent, child, label) {
  if (dirname(resolve(child)) !== resolve(parent)) {
    throw new Error(`${label} must be a direct child of ${parent}: ${child}`);
  }
}

function assertPathContained(root, path, allowRoot, label) {
  const relativePath = relative(resolve(root), resolve(path));
  const contained = (allowRoot && relativePath === "") || (
    relativePath !== "" && relativePath !== ".." &&
    !relativePath.startsWith(`..${sep}`) && !isAbsolute(relativePath)
  );
  if (!contained) {
    throw new Error(`${label} escapes owned root ${root}: ${path}`);
  }
}

function throwWithCleanupErrors(error, cleanupErrors, label) {
  if (cleanupErrors.length === 0) {
    throw error;
  }
  throw new AggregateError(
    [error, ...cleanupErrors],
    `${errorMessage(error)}; ${label} failed: ${cleanupErrors.map(errorMessage).join("; ")}`,
    { cause: error },
  );
}

function validScreenshotLockOwner(owner) {
  return Number.isInteger(owner?.pid) &&
    (typeof owner.processStartIdentity === "string" || owner.processStartIdentity === null) &&
    Number.isFinite(owner.createdAtMs) &&
    typeof owner.runId === "string" && owner.runId.length > 0 &&
    typeof owner.token === "string" && owner.token.length >= 16;
}

function fileIdentityFromStat(stat) {
  return { device: stat.dev, inode: stat.ino };
}

function sameFileIdentity(left, right) {
  return left?.device === right?.device && left?.inode === right?.inode;
}

export function installSignalHandlers(processLike = process) {
  let state = null;
  let signalCode = null;
  let signalName = null;
  const receive = (code, name) => {
    if (signalCode !== null) {
      return;
    }
    signalCode = code;
    signalName = name;
    if (state) {
      requestSignalStop(state, code, name);
    }
  };
  const onSigint = () => receive(130, "SIGINT");
  const onSigterm = () => receive(143, "SIGTERM");
  processLike.on("SIGINT", onSigint);
  processLike.on("SIGTERM", onSigterm);
  return {
    get signalCode() {
      return signalCode;
    },
    attach(nextState) {
      state = nextState;
      if (signalCode !== null) {
        requestSignalStop(state, signalCode, signalName);
      }
    },
    remove() {
      processLike.removeListener("SIGINT", onSigint);
      processLike.removeListener("SIGTERM", onSigterm);
    },
  };
}

export function requestSignalStop(state, code, signal) {
  if (state.signalCode !== null) {
    return;
  }
  state.signalCode = code;
  state.abortController.abort(new Error(`web integration interrupted by ${signal}`));
  for (const record of state.children) {
    terminateRecord(record);
  }
}

export async function executeHarness(state) {
  if (state.options.mode === "dev") {
    return executeDevHarness(state);
  }
  return executeComposeHarness(state);
}

export async function executeDevHarness(state) {
  let testExitCode = 0;
  try {
    await startDevStack(state);
    for (const browserScript of state.plan[0].scripts) {
      seedDevConfig(state, browserScript);
      testExitCode = await runDevK6(state, browserScript);
      if (state.screenshotsDir) {
        try {
          await fixDevScreenshotPermissions(state);
        } catch (error) {
          if (testExitCode === 0) {
            throw error;
          }
          runtimeFor(state).logger.error(`failed to repair screenshots after k6 failure: ${errorMessage(error)}`);
        }
      }
      if (testExitCode !== 0 || state.signalCode !== null) {
        break;
      }
    }
    if (state.screenshotsDir && testExitCode === 0 && state.signalCode === null) {
      validateScreenshots(state.screenshotsDir, screenshotSnapshotDir, {
        update: state.options.updateScreenshots,
      });
    }
  } catch (error) {
    state.runtimeError ??= error;
    if (state.signalCode === null) {
      runtimeFor(state).logger.error(errorMessage(state.runtimeError));
    }
    testExitCode ||= 1;
  }
  state.failure = testExitCode !== 0;
  return { testExitCode, cleanupFailed: false };
}

export async function executeComposeHarness(state) {
  let testExitCode = 0;
  let cleanupFailed = false;

  for (const phase of state.plan) {
    state.activePhase = phase.phase;
    runtimeFor(state).logger.log(`web integration phase: ${phase.phase}`);
    try {
      await runComposeChecked(state, ["--profile", phase.profile, "build"], BUILD_TIMEOUT_MS);
      state.composeRunning = true;
      state.composeTeardownAttempted = false;
      await runComposeChecked(state, [
        "--profile",
        phase.profile,
        "up",
        "--detach",
        "--wait",
        "--wait-timeout",
        "120",
        ...phase.upServices,
      ], COMPOSE_UP_TIMEOUT_MS);

      for (const browserScript of phase.scripts) {
        await seedComposeConfig(state, phase, browserScript);
        testExitCode = await runComposeK6(state, phase, browserScript);
        if (state.screenshotsDir) {
          try {
            await fixComposeScreenshotPermissions(state, phase);
          } catch (error) {
            if (testExitCode === 0) {
              throw error;
            }
            runtimeFor(state).logger.error(`failed to repair screenshots after k6 failure: ${errorMessage(error)}`);
          }
        }
        if (testExitCode !== 0 || state.signalCode !== null) {
          break;
        }
      }
    } catch (error) {
      state.runtimeError ??= error;
      if (state.signalCode === null) {
        runtimeFor(state).logger.error(errorMessage(state.runtimeError));
      }
      testExitCode ||= 1;
    }

    if (testExitCode !== 0 && state.signalCode === null) {
      state.failure = true;
      await captureComposeDiagnostics(state, phase.phase);
    }
    const stopped = await stopCompose(state);
    cleanupFailed ||= !stopped;
    if (testExitCode !== 0 || cleanupFailed || state.signalCode !== null) {
      break;
    }
  }

  if (state.screenshotsDir && testExitCode === 0 && !cleanupFailed && state.signalCode === null) {
    try {
      validateScreenshots(state.screenshotsDir, screenshotSnapshotDir, {
        update: state.options.updateScreenshots,
      });
    } catch (error) {
      runtimeFor(state).logger.error(errorMessage(error));
      testExitCode = 1;
    }
  }
  state.failure ||= testExitCode !== 0 || cleanupFailed;
  return { testExitCode, cleanupFailed };
}

async function startDevStack(state) {
  const serverPort = await getFreePort();
  const webPort = await getFreePort();
  state.serverUrl = `http://127.0.0.1:${serverPort}`;
  state.webUrl = `http://127.0.0.1:${webPort}`;
  state.dockerWebUrl = `http://${getHostAddress()}:${webPort}`;

  await runChecked(state, "pnpm", ["--filter", "@rfhold/waltr-web", "wasm:build"], {
    timeoutMs: BUILD_TIMEOUT_MS,
  });
  await runChecked(state, "cargo", ["build", "-p", "waltr-server"], {
    timeoutMs: BUILD_TIMEOUT_MS,
  });

  startLogged(state, "server", `${repoRoot}/target/debug/waltr-server`, [], {
    cwd: state.runRoot,
    env: {
      WALTR_SERVER_ADDR: `127.0.0.1:${serverPort}`,
      WALTR_AUTH_MODE: "disabled",
      WALTR_DATABASE_URL: `sqlite://${state.runRoot}/waltr.sqlite3`,
      WALTR_MCP_DATABASE_URL: `sqlite://${state.runRoot}/mcp.sqlite3`,
      WALTR_AUTH_PATH: `${state.runRoot}/auth.json`,
      WALTR_MODELS_DEV_PATH: modelsDevFixture,
      WALTR_SESSION_FILESYSTEM_ROOT: `${state.runRoot}/sessions`,
      WALTR_SESSION_STORE: "filesystem",
    },
  });
  await waitForHttp(state, `${state.serverUrl}/health`, "server");

  startLogged(state, "web", "pnpm", [
    "--filter",
    "@rfhold/waltr-web",
    "dev:vite",
    "--host",
    "0.0.0.0",
    "--port",
    String(webPort),
    "--strictPort",
  ], {
    env: { WALTR_WEB_PROXY_TARGET: state.serverUrl },
  });
  await waitForHttp(state, state.webUrl, "web");
}

function seedDevConfig(state, browserScript) {
  const configDir = `${state.runRoot}/.waltr`;
  const configPath = `${configDir}/waltr.toml`;
  const nextPath = `${configPath}.next`;
  mkdirSync(configDir, { recursive: true });
  copyFileSync(fixtureForBrowserScript(browserScript), nextPath);
  const staged = lstatSync(nextPath);
  if (staged.isSymbolicLink() || !staged.isFile()) {
    throw new Error(`refusing unsafe staged dev config ${nextPath}`);
  }
  const stagedIdentity = fileIdentityFromStat(staged);
  // Replacing config is intentional publication between browser scripts.
  renameSync(nextPath, configPath);
  const published = lstatSync(configPath);
  if (published.isSymbolicLink() || !published.isFile() ||
    !sameFileIdentity(fileIdentityFromStat(published), stagedIdentity)) {
    throw new Error(`dev config changed during publication: ${configPath}`);
  }
}

async function seedComposeConfig(state, phase, browserScript) {
  const target = "/data/.waltr/waltr.toml";
  const staged = "/tmp/waltr.toml.seed";
  const next = `${target}.next`;
  await runComposeChecked(state, [
    "--profile",
    phase.profile,
    "exec",
    "-T",
    phase.waltrService,
    "mkdir",
    "-p",
    "/data/.waltr",
  ], DOCKER_TIMEOUT_MS);
  await runComposeChecked(state, [
    "--profile",
    phase.profile,
    "cp",
    fixtureForBrowserScript(browserScript),
    `${phase.waltrService}:${staged}`,
  ], DOCKER_TIMEOUT_MS);
  await runComposeChecked(state, [
    "--profile",
    phase.profile,
    "exec",
    "-T",
    phase.waltrService,
    "cp",
    staged,
    next,
  ], DOCKER_TIMEOUT_MS);
  await runComposeChecked(state, [
    "--profile",
    phase.profile,
    "exec",
    "-T",
    phase.waltrService,
    "mv",
    next,
    target,
  ], DOCKER_TIMEOUT_MS);
}

function fixtureForBrowserScript(browserScript) {
  if (browserScript === "tests/web/k6/sessions/sessions.browser.js") {
    return sessionsRuntimeConfigFixture;
  }
  return emptyConfigFixture;
}

export async function runDevK6(state, browserScript) {
  state.directContainerNames ??= new Set();
  state.directContainerNames.add(state.devK6ContainerName);
  if (!await removeDirectContainer(state, state.devK6ContainerName)) {
    throw new Error(`failed to clear direct container ${state.devK6ContainerName} before k6`);
  }
  const args = buildDevK6Args(state, browserScript);
  const result = await runCommandResult(state, "docker", args, { timeoutMs: K6_TIMEOUT_MS });
  return commandExitCode(result);
}

export function buildDevK6Args(state, browserScript) {
  const args = [
    "run",
    "--rm",
    "--name",
    state.devK6ContainerName,
    "--volume",
    `${repoRoot}:/workspace:ro`,
  ];
  if (state.screenshotsDir) {
    args.push("--volume", `${state.screenshotsDir}:/screenshots:rw`);
  }
  args.push(
    "--workdir",
    "/workspace",
    "--env",
    "K6_BROWSER_HEADLESS=true",
    "--env",
    `WALTR_WEB_BASE_URL=${state.dockerWebUrl}`,
    "--env",
    `WALTR_WEB_SCREENSHOTS=${state.options.screenshots ? "1" : "0"}`,
    "--env",
    "WALTR_WEB_SCREENSHOTS_DIR=/screenshots",
    "--env",
    `WALTR_AGENT_LIVE_GATEWAY=${state.options.live ? "1" : "0"}`,
    "grafana/k6:1.3.0-with-browser",
    "run",
    browserScript,
  );
  return args;
}

async function runComposeK6(state, phase, browserScript) {
  const args = ["--profile", phase.profile, "run", "--rm", "--no-deps"];
  if (state.screenshotsDir) {
    args.push("--volume", `${state.screenshotsDir}:/screenshots:rw`);
  }
  args.push(
    "--env",
    `WALTR_WEB_SCREENSHOTS=${state.options.screenshots ? "1" : "0"}`,
    "--env",
    `WALTR_AGENT_LIVE_GATEWAY=${state.options.live ? "1" : "0"}`,
    phase.k6Service,
    "run",
    browserScript,
  );
  const result = await runComposeResult(state, args, { timeoutMs: K6_TIMEOUT_MS });
  return commandExitCode(result);
}

async function fixDevScreenshotPermissions(state) {
  state.directContainerNames ??= new Set();
  state.directContainerNames.add(state.devScreenshotContainerName);
  if (!await removeDirectContainer(state, state.devScreenshotContainerName)) {
    throw new Error(`failed to clear direct container ${state.devScreenshotContainerName} before chmod`);
  }
  await runChecked(state, "docker", [
    "run",
    "--rm",
    "--name",
    state.devScreenshotContainerName,
    "--user",
    "0:0",
    "--volume",
    `${state.screenshotsDir}:/screenshots:rw`,
    "--entrypoint",
    "chmod",
    "grafana/k6:1.3.0-with-browser",
    "-R",
    "a+rwX",
    "/screenshots",
  ], { timeoutMs: DOCKER_TIMEOUT_MS });
}

async function fixComposeScreenshotPermissions(state, phase) {
  await runComposeChecked(state, [
    "--profile",
    phase.profile,
    "run",
    "--rm",
    "--no-deps",
    "--user",
    "0:0",
    "--volume",
    `${state.screenshotsDir}:/screenshots:rw`,
    "--entrypoint",
    "chmod",
    phase.k6Service,
    "-R",
    "a+rwX",
    "/screenshots",
  ], DOCKER_TIMEOUT_MS);
}

function composeArgs(state, args) {
  return [
    "compose",
    "--file",
    `${repoRoot}/compose.yml`,
    "--project-name",
    state.projectName,
    ...args,
  ];
}

function composeEnvironment(state) {
  return { ...state.baseEnvironment, COMPOSE_DISABLE_ENV_FILE: "1" };
}

function runComposeResult(state, args, options = {}) {
  return runCommandResult(state, "docker", composeArgs(state, args), {
    ...options,
    absoluteEnvironment: composeEnvironment(state),
  });
}

async function runComposeChecked(state, args, timeoutMs) {
  const result = await runComposeResult(state, args, { timeoutMs });
  assertCommandSuccess("docker", composeArgs(state, args), result);
}

export async function stopCompose(state) {
  if (!state.composeRunning) {
    return true;
  }
  if (state.composeTeardownAttempted) {
    return false;
  }
  state.composeTeardownAttempted = true;
  const downArgs = [
    "--profile",
    "test",
    "--profile",
    "test-auth",
    "down",
    "--volumes",
    "--remove-orphans",
    "--timeout",
    "5",
  ];

  const first = await runComposeResult(state, downArgs, {
    timeoutMs: COMPOSE_DOWN_TIMEOUT_MS,
    ignoreAbort: true,
  });
  if (commandSucceeded(first)) {
    state.composeRunning = false;
    return true;
  }

  runtimeFor(state).logger.error(`docker compose down failed: ${commandResultMessage(first)}`);
  await captureComposeDiagnostics(state, state.activePhase ?? "cleanup");
  const childrenStopped = await stopChildren(state);
  const second = await runComposeResult(state, downArgs, {
    timeoutMs: COMPOSE_DOWN_TIMEOUT_MS,
    ignoreAbort: true,
  });
  if (commandSucceeded(second)) {
    state.composeRunning = false;
    return childrenStopped;
  }
  runtimeFor(state).logger.error(`docker compose down retry failed: ${commandResultMessage(second)}`);
  return false;
}

async function captureComposeDiagnostics(state, phase) {
  if (state.diagnosticsCaptured.has(phase)) {
    return;
  }
  state.diagnosticsCaptured.add(phase);
  const commands = [
    ["ps", ["--profile", "test", "--profile", "test-auth", "ps", "--all"]],
    ["logs", ["--profile", "test", "--profile", "test-auth", "logs", "--no-color"]],
  ];

  for (const [name, args] of commands) {
    try {
      const result = await runComposeResult(state, args, {
        timeoutMs: DOCKER_TIMEOUT_MS,
        capture: true,
        ignoreAbort: true,
      });
      const path = `${state.launchLogDir}/compose-${phase}-${name}.log`;
      runtimeFor(state).writeFileSync(path, `${result.stdout}${result.stderr}`);
      runtimeFor(state).logger.error(`web integration compose ${name}: ${path}`);
      if (!commandSucceeded(result)) {
        runtimeFor(state).logger.error(`failed to capture compose ${name}: ${commandResultMessage(result)}`);
      }
    } catch (error) {
      runtimeFor(state).logger.error(`failed to capture compose ${name}: ${errorMessage(error)}`);
    }
  }
}

export async function cleanupHarness(state) {
  state.shuttingDown = true;
  let cleaned = true;
  try {
    if (state.composeRunning) {
      try {
        if (state.failure && state.signalCode === null) {
          await captureComposeDiagnostics(state, state.activePhase ?? "cleanup");
        }
        cleaned = (await stopCompose(state)) && cleaned;
      } catch (error) {
        runtimeFor(state).logger.error(`failed to stop web integration Compose project: ${errorMessage(error)}`);
        cleaned = false;
      }
    }
    if (state.directContainerNames?.size > 0) {
      try {
        cleaned = (await removeDirectContainers(state)) && cleaned;
      } catch (error) {
        runtimeFor(state).logger.error(`failed to remove direct containers before child shutdown: ${errorMessage(error)}`);
        cleaned = false;
      }
    }
    try {
      cleaned = (await stopChildren(state)) && cleaned;
    } catch (error) {
      runtimeFor(state).logger.error(`failed to stop web integration children: ${errorMessage(error)}`);
      cleaned = false;
    }
    if (state.directContainerNames?.size > 0) {
      try {
        cleaned = (await removeDirectContainers(state)) && cleaned;
      } catch (error) {
        runtimeFor(state).logger.error(`failed to remove direct containers after child shutdown: ${errorMessage(error)}`);
        cleaned = false;
      }
    }
    if (state.screenshotLock) {
      try {
        cleaned = releaseScreenshotLock(state.screenshotLock, runtimeFor(state)) && cleaned;
      } catch (error) {
        runtimeFor(state).logger.error(`failed to release screenshot lock: ${errorMessage(error)}`);
        cleaned = false;
      } finally {
        state.screenshotLock = null;
      }
    }
  } finally {
    try {
      const runtime = runtimeFor(state);
      if (state.runRootOwnership) {
        cleaned = removeOwnedDirectoryTree(state.runRootOwnership, runtime) && cleaned;
      } else if (lstatIfExists(state.runRoot, runtime)) {
        runtime.logger.error(`refusing to remove unowned web integration root ${state.runRoot}`);
        cleaned = false;
      }
    } catch (error) {
      runtimeFor(state).logger.error(`failed to remove web integration root ${state.runRoot}: ${errorMessage(error)}`);
      cleaned = false;
    }
  }
  return cleaned;
}

export async function removeDirectContainers(state) {
  let removed = true;
  for (const name of state.directContainerNames ?? []) {
    removed = (await removeDirectContainer(state, name)) && removed;
  }
  return removed;
}

async function removeDirectContainer(state, name) {
  const result = await runCommandResult(state, "docker", ["container", "rm", "--force", name], {
    capture: true,
    ignoreAbort: true,
    timeoutMs: DOCKER_TIMEOUT_MS,
  });
  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  if (commandSucceeded(result) || /No such container/i.test(output)) {
    return true;
  }
  runtimeFor(state).logger.error(`failed to remove direct container ${name}: ${commandResultMessage(result)}`);
  return false;
}

function runChecked(state, command, args, options = {}) {
  return runCommandResult(state, command, args, options).then((result) => {
    assertCommandSuccess(command, args, result);
  });
}

function runtimeFor(state) {
  state.runtime ??= createHarnessRuntime();
  return state.runtime;
}

export async function runCommandResult(state, command, args, options = {}) {
  if (state.abortController.signal.aborted && !options.ignoreAbort) {
    return {
      code: null,
      signal: null,
      error: state.abortController.signal.reason ?? new Error("web integration aborted"),
      timedOut: false,
      stdout: "",
      stderr: "",
    };
  }
  const environment = options.absoluteEnvironment ?? {
    ...state.baseEnvironment,
    ...options.env,
  };
  const runtime = runtimeFor(state);
  if (runtime.commandRunner) {
    return runtime.commandRunner({
      state,
      command,
      args,
      options: { ...options, cwd: options.cwd ?? repoRoot, env: environment },
    });
  }
  const record = spawnRecord(state, command, args, {
    cwd: options.cwd ?? repoRoot,
    env: environment,
    capture: options.capture ?? false,
    stdio: options.stdio,
    detached: options.detached,
    timeoutMs: options.timeoutMs,
    ignoreAbort: options.ignoreAbort ?? false,
  });
  return record.closed;
}

export function spawnRecord(state, command, args, options) {
  const runtime = runtimeFor(state);
  const capture = options.capture;
  const detached = options.detached ?? runtime.supportsProcessGroups;
  const child = runtime.spawn(command, args, {
    cwd: options.cwd,
    detached,
    env: options.env,
    stdio: options.stdio ?? (capture ? ["ignore", "pipe", "pipe"] : "inherit"),
  });
  const processGroupRequested = detached && runtime.supportsProcessGroups && Boolean(child.pid);
  let leaderIdentity = null;
  let ownershipError = null;
  if (processGroupRequested) {
    try {
      leaderIdentity = runtime.getProcessIdentity(child.pid);
    } catch (error) {
      ownershipError = error;
    }
    if (runtime.platform === "linux" && !validLaunchedProcessGroupIdentity(leaderIdentity, child.pid)) {
      ownershipError ??= new Error(
        `could not establish Linux process-group identity for ${command} leader ${child.pid}`,
      );
    }
  }
  const hasProcessGroup = processGroupRequested && !ownershipError;
  const record = {
    state,
    child,
    command,
    args,
    detached,
    hasProcessGroup,
    ownershipError,
    groupIdentity: hasProcessGroup ? {
      processGroupId: child.pid,
      sessionId: leaderIdentity?.sessionId ?? child.pid,
      leaderStartIdentity: leaderIdentity?.startIdentity ?? null,
      leaderClosed: false,
    } : null,
    runtime,
    closed: null,
    closeResult: null,
    leaderClosed: false,
    terminationRequested: false,
    onAbort: null,
    forceTimer: null,
    timeoutTimer: null,
    completionPollTimer: null,
    completionDeadlineTimer: null,
    resolveClosed: null,
    settled: false,
  };
  state.children.add(record);
  let stdout = "";
  let stderr = "";
  let spawnError = ownershipError;
  let timedOut = false;

  if (capture) {
    child.stdout?.on("data", (chunk) => {
      stdout = appendCaptured(stdout, chunk);
    });
    child.stderr?.on("data", (chunk) => {
      stderr = appendCaptured(stderr, chunk);
    });
  }
  child.once("error", (error) => {
    spawnError = error;
  });

  const onAbort = () => terminateRecord(record);
  record.onAbort = onAbort;
  if (!options.ignoreAbort) {
    state.abortController.signal.addEventListener("abort", onAbort, { once: true });
  }
  if (options.timeoutMs) {
    record.timeoutTimer = runtime.setTimeout(() => {
      timedOut = true;
      terminateRecord(record);
    }, options.timeoutMs);
  }

  record.closed = new Promise((resolveClosed) => {
    record.resolveClosed = resolveClosed;
    child.once("close", (code, signal) => {
      record.leaderClosed = true;
      if (record.groupIdentity) {
        record.groupIdentity.leaderClosed = true;
      }
      if (record.settled) {
        finalizeRecordIfGone(record);
        return;
      }
      runtime.clearTimeout(record.timeoutTimer);
      record.timeoutTimer = null;
      record.closeResult = { code, signal, error: spawnError, timedOut, stdout, stderr };
      handleRecordLeaderClose(record);
    });
  });
  if (ownershipError) {
    state.processGroupOwnershipFailed = true;
    runtime.logger.error(errorMessage(ownershipError));
    record.terminationRequested = true;
    try {
      if (child.pid && child.exitCode == null && child.signalCode == null) {
        child.kill("SIGKILL");
      }
    } catch (error) {
      if (error.code !== "ESRCH") {
        runtime.logger.error(`failed to contain unowned ${command} leader ${child.pid}: ${errorMessage(error)}`);
      }
    }
    armRecordCompletion(record);
  }
  if (!options.ignoreAbort && state.abortController.signal.aborted) {
    terminateRecord(record);
  }
  return record;
}

function appendCaptured(current, chunk) {
  if (current.length >= CAPTURE_LIMIT) {
    return current;
  }
  const value = chunk.toString();
  return `${current}${value.slice(0, CAPTURE_LIMIT - current.length)}`;
}

function startLogged(state, label, command, args, options = {}) {
  const runtime = runtimeFor(state);
  const log = runtime.openSync(`${state.launchLogDir}/${label}.log`, "wx");
  let record;
  try {
    record = spawnRecord(state, command, args, {
      cwd: options.cwd ?? repoRoot,
      env: { ...state.baseEnvironment, ...options.env },
      capture: false,
      stdio: ["ignore", log, log],
      detached: runtime.supportsProcessGroups,
      timeoutMs: null,
      ignoreAbort: false,
    });
  } finally {
    runtime.closeSync(log);
  }

  void record.closed.then((result) => {
    if (state.shuttingDown || state.abortController.signal.aborted) {
      return;
    }
    state.runtimeError = new Error(`${label} process stopped unexpectedly: ${commandResultMessage(result)}`);
    state.abortController.abort(state.runtimeError);
  });
  return record;
}

function terminateRecord(record) {
  if (record.terminationRequested || finalizeRecordIfGone(record)) {
    return;
  }
  record.terminationRequested = true;
  record.forceTimer ??= record.runtime.setTimeout(() => {
    record.forceTimer = null;
    if (!finalizeRecordIfGone(record)) {
      sendRecordSignal(record, "SIGKILL");
    }
  }, PROCESS_GROUP_GRACE_MS);
  sendRecordSignal(record, "SIGTERM");
  armRecordCompletion(record);
}

function handleRecordLeaderClose(record) {
  if (finalizeRecordIfGone(record)) {
    return;
  }
  if (!record.terminationRequested) {
    record.closeResult.error ??= new Error(
      `${record.command} process group survived its leader exit`,
    );
    terminateRecord(record);
  } else {
    armRecordCompletion(record);
  }
}

function armRecordCompletion(record) {
  if (!record.completionDeadlineTimer) {
    record.completionDeadlineTimer = record.runtime.setTimeout(() => {
      record.completionDeadlineTimer = null;
      if (finalizeRecordIfGone(record)) {
        return;
      }
      record.state.processGroupOwnershipFailed = true;
      const completionError = new Error(
        `${record.hasProcessGroup ? "process group" : "process"} for ${record.command} did not exit after termination`,
      );
      const error = record.ownershipError
        ? new AggregateError(
          [record.ownershipError, completionError],
          `${errorMessage(record.ownershipError)}; ${completionError.message}`,
        )
        : completionError;
      if (record.closeResult) {
        record.closeResult.error ??= error;
      } else {
        record.closeResult = {
          code: null,
          signal: null,
          error,
          timedOut: true,
          stdout: "",
          stderr: "",
        };
      }
      relinquishRecordChild(record);
      settleRecord(record);
    }, PROCESS_GROUP_GRACE_MS + PROCESS_GROUP_KILL_WAIT_MS);
  }
  scheduleRecordCompletionPoll(record);
}

function scheduleRecordCompletionPoll(record) {
  if (record.completionPollTimer || record.settled) {
    return;
  }
  record.completionPollTimer = record.runtime.setTimeout(() => {
    record.completionPollTimer = null;
    if (!finalizeRecordIfGone(record) && !record.settled) {
      scheduleRecordCompletionPoll(record);
    }
  }, PROCESS_GROUP_POLL_MS);
}

function settleRecord(record) {
  if (record.settled || !record.closeResult) {
    return;
  }
  record.settled = true;
  clearRecordResources(record);
  record.resolveClosed(record.closeResult);
}

function clearRecordResources(record) {
  record.state.abortController.signal.removeEventListener("abort", record.onAbort);
  record.runtime.clearTimeout(record.timeoutTimer);
  record.runtime.clearTimeout(record.forceTimer);
  record.runtime.clearTimeout(record.completionPollTimer);
  record.runtime.clearTimeout(record.completionDeadlineTimer);
  record.timeoutTimer = null;
  record.forceTimer = null;
  record.completionPollTimer = null;
  record.completionDeadlineTimer = null;
}

function relinquishRecordChild(record) {
  for (const stream of [record.child.stdin, record.child.stdout, record.child.stderr]) {
    stream?.destroy?.();
    stream?.unref?.();
  }
  record.child.channel?.unref?.();
  record.child.unref?.();
}

function sendRecordSignal(record, signal) {
  try {
    if (record.hasProcessGroup) {
      if (record.runtime.platform === "linux" && !validTrackedProcessGroupIdentity(record.groupIdentity)) {
        record.state.processGroupOwnershipFailed = true;
        record.runtime.logger.error(`refusing to signal unverified process group for ${record.command}`);
        return false;
      }
      const group = record.runtime.inspectProcessGroup(record.groupIdentity);
      if (!group.alive) {
        if (group.reused) {
          record.runtime.logger.error(
            `refusing to signal reused process group ${record.groupIdentity.processGroupId} for ${record.command}`,
          );
        }
        finalizeRecordIfGone(record, group);
        return false;
      }
      record.runtime.processKill(-record.groupIdentity.processGroupId, signal);
      return true;
    }
    if (!record.child.pid || record.leaderClosed || record.child.exitCode != null || record.child.signalCode != null) {
      finalizeRecordIfGone(record);
      return false;
    }
    if (record.runtime.platform === "win32") {
      const args = ["/pid", String(record.child.pid), "/T"];
      if (signal === "SIGKILL") {
        args.push("/F");
      }
      const result = record.runtime.spawnSync("taskkill", args, { stdio: "ignore" });
      if (result.error || result.status !== 0) {
        record.child.kill(signal);
      }
    } else {
      record.child.kill(signal);
    }
    return true;
  } catch (error) {
    if (error.code !== "ESRCH") {
      record.runtime.logger.error(`failed to send ${signal} to ${record.command}: ${errorMessage(error)}`);
    }
    finalizeRecordIfGone(record);
    return false;
  }
}

export async function stopChildren(state) {
  const ownershipSafe = state.processGroupOwnershipFailed !== true;
  const records = [...state.children].filter((record) => !finalizeRecordIfGone(record));
  if (records.length === 0) {
    return ownershipSafe;
  }
  for (const record of records) {
    sendRecordSignal(record, "SIGTERM");
  }
  if (await waitForRecords(records, PROCESS_GROUP_GRACE_MS)) {
    return ownershipSafe;
  }
  for (const record of records) {
    sendRecordSignal(record, "SIGKILL");
  }
  return (await waitForRecords(records, PROCESS_GROUP_KILL_WAIT_MS)) && ownershipSafe;
}

function waitForRecords(records, timeoutMs) {
  const runtime = records[0]?.runtime ?? createHarnessRuntime();
  return new Promise((resolveWait) => {
    let settled = false;
    let pollTimer = null;
    const finish = (result) => {
      if (!settled) {
        settled = true;
        runtime.clearTimeout(timeoutTimer);
        runtime.clearTimeout(pollTimer);
        pollTimer = null;
        resolveWait(result);
      }
    };
    const check = () => {
      if (records.every((record) => finalizeRecordIfGone(record))) {
        finish(true);
      } else if (!settled && pollTimer === null) {
        pollTimer = runtime.setTimeout(poll, PROCESS_GROUP_POLL_MS);
      }
    };
    const poll = () => {
      pollTimer = null;
      check();
    };
    const timeoutTimer = runtime.setTimeout(() => {
      finish(records.every((record) => finalizeRecordIfGone(record)));
    }, timeoutMs);
    for (const record of records) {
      void record.closed.then(check);
    }
    check();
  });
}

function finalizeRecordIfGone(record, inspectedGroup) {
  if (!record.leaderClosed) {
    return false;
  }
  if (record.hasProcessGroup) {
    let group;
    try {
      group = inspectedGroup ?? record.runtime.inspectProcessGroup(record.groupIdentity);
    } catch (error) {
      record.runtime.logger.error(
        `failed to inspect process group ${record.groupIdentity.processGroupId}: ${errorMessage(error)}`,
      );
      return false;
    }
    if (group.alive) {
      return false;
    }
  }
  record.state.children.delete(record);
  settleRecord(record);
  return true;
}

function validLaunchedProcessGroupIdentity(identity, pid) {
  return identity?.pid === pid &&
    identity.processGroupId === pid &&
    identity.sessionId === pid &&
    typeof identity.startIdentity === "string" && identity.startIdentity.length > 0;
}

function validTrackedProcessGroupIdentity(identity) {
  return Number.isInteger(identity?.processGroupId) &&
    Number.isInteger(identity.sessionId) &&
    typeof identity.leaderStartIdentity === "string" && identity.leaderStartIdentity.length > 0;
}

function assertCommandSuccess(command, args, result) {
  if (!commandSucceeded(result)) {
    throw new Error(`${command} ${args.join(" ")} failed: ${commandResultMessage(result)}`);
  }
}

function commandSucceeded(result) {
  return !result.error && !result.timedOut && result.signal === null && result.code === 0;
}

function commandExitCode(result) {
  if (commandSucceeded(result)) {
    return 0;
  }
  if (result.error || result.timedOut) {
    return 1;
  }
  if (result.code !== null) {
    return result.code || 1;
  }
  if (result.signal === "SIGINT") {
    return 130;
  }
  if (result.signal === "SIGTERM") {
    return 143;
  }
  return 1;
}

function commandResultMessage(result) {
  if (result.error) {
    return result.error.message;
  }
  if (result.timedOut) {
    return "timed out";
  }
  if (result.signal) {
    return `terminated by ${result.signal}`;
  }
  return `exited with ${result.code}`;
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}

function getFreePort() {
  return new Promise((resolvePort, rejectPort) => {
    const server = net.createServer();
    server.once("error", rejectPort);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close((error) => {
        if (error) {
          rejectPort(error);
          return;
        }
        if (!address || typeof address === "string") {
          rejectPort(new Error("free port lookup did not return a TCP address"));
          return;
        }
        resolvePort(address.port);
      });
    });
  });
}

function getHostAddress() {
  for (const addresses of Object.values(networkInterfaces())) {
    for (const address of addresses ?? []) {
      if (address.family === "IPv4" && !address.internal) {
        return address.address;
      }
    }
  }
  throw new Error("web integration could not find a host IPv4 address for the dev browser container");
}

async function waitForHttp(state, url, label) {
  const deadline = Date.now() + HTTP_READY_TIMEOUT_MS;
  let lastError;
  while (Date.now() < deadline) {
    if (state.runtimeError) {
      throw state.runtimeError;
    }
    if (state.abortController.signal.aborted) {
      throw state.abortController.signal.reason;
    }
    try {
      const response = await fetch(url, {
        signal: AbortSignal.any([state.abortController.signal, AbortSignal.timeout(2_000)]),
      });
      if (response.ok) {
        return;
      }
      lastError = new Error(`${label} returned HTTP ${response.status}`);
    } catch (error) {
      if (state.abortController.signal.aborted) {
        throw state.abortController.signal.reason;
      }
      lastError = error;
    }
    await sleep(250, state.abortController.signal);
  }
  throw new Error(`${label} did not become ready at ${url}: ${errorMessage(lastError)}`);
}

function sleep(milliseconds, signal) {
  return new Promise((resolveSleep, rejectSleep) => {
    let onAbort;
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolveSleep();
    }, milliseconds);
    if (!signal) {
      return;
    }
    onAbort = () => {
      clearTimeout(timer);
      rejectSleep(signal.reason);
    };
    signal.addEventListener("abort", onAbort, { once: true });
  });
}

export function validateScreenshots(actualDir, expectedDir, options) {
  const actualFiles = readPngFiles(actualDir);

  if (actualFiles.length === 0) {
    throw new Error(`no screenshots were written to ${actualDir}`);
  }

  if (options.update) {
    updateScreenshotSnapshots(actualDir, expectedDir, actualFiles, options);
    (options.logger ?? console).log(`updated ${actualFiles.length} screenshot snapshots in ${expectedDir}`);
    return;
  }

  mkdirSync(expectedDir, { recursive: true });

  const expectedFiles = readPngFiles(expectedDir);
  const actualSet = new Set(actualFiles);
  const expectedSet = new Set(expectedFiles);
  const failures = [];

  for (const file of actualFiles) {
    if (!expectedSet.has(file)) {
      failures.push(`${file}: missing snapshot; run pnpm test:web:screenshots:update`);
      continue;
    }

    const result = comparePngs(join(actualDir, file), join(expectedDir, file));

    if (!result.ok) {
      failures.push(`${file}: ${result.message}`);
    }
  }

  for (const file of expectedFiles) {
    if (!actualSet.has(file)) {
      failures.push(`${file}: snapshot was not generated by the current k6 flows`);
    }
  }

  if (failures.length > 0) {
    throw new Error(`screenshot snapshot validation failed:\n${failures.map((failure) => `- ${failure}`).join("\n")}`);
  }

  (options.logger ?? console).log(`validated ${actualFiles.length} screenshot snapshots against ${expectedDir}`);
}

function updateScreenshotSnapshots(actualDir, expectedDir, actualFiles, options) {
  const runtime = options.runtime ?? createHarnessRuntime({ logger: options.logger ?? console });
  const resolvedExpected = resolve(expectedDir);
  const parent = dirname(resolvedExpected);
  runtime.mkdirSync(parent, { recursive: true });
  const parentIdentity = verifyPlainDirectory(parent, runtime, "screenshot snapshot parent").fileIdentity;
  const existing = lstatIfExists(resolvedExpected, runtime);
  if (existing && (existing.isSymbolicLink() || !existing.isDirectory())) {
    throw new Error(`refusing unsafe screenshot snapshot directory ${resolvedExpected}`);
  }
  const existingIdentity = existing ? fileIdentityFromStat(existing) : null;
  if (existingIdentity) {
    verifyFlatPngDirectory(resolvedExpected, existingIdentity, parent, parentIdentity, runtime);
  }
  const transactionId = `${runtime.pid}-${runtime.randomBytes(8).toString("hex")}`;
  const stagingDir = join(parent, `.${basename(resolvedExpected)}.next-${transactionId}`);
  const backupDir = join(parent, `.${basename(resolvedExpected)}.backup-${transactionId}`);
  if (lstatIfExists(stagingDir, runtime) || lstatIfExists(backupDir, runtime)) {
    throw new Error(`screenshot snapshot transaction path already exists for ${resolvedExpected}`);
  }

  runtime.mkdirSync(stagingDir, { mode: 0o700 });
  const stagingIdentity = verifyPlainDirectory(
    stagingDir,
    runtime,
    "screenshot snapshot staging directory",
  ).fileIdentity;
  const stagingOwnership = {
    path: stagingDir,
    parent,
    parentIdentity,
    fileIdentity: stagingIdentity,
    label: "screenshot snapshot staging directory",
  };
  const stagedFiles = new Map();
  let baselineLocation = existingIdentity ? "expected" : null;
  let stagingLocation = "staging";
  let committed = false;
  const cleanupErrors = [];

  try {
    for (const file of actualFiles) {
      const stagedPath = join(stagingDir, file);
      stagedFiles.set(file, copyRegularFileExclusive(
        join(actualDir, file),
        stagedPath,
        runtime,
        "screenshot snapshot",
      ));
    }
    verifyExactFlatFiles(stagingOwnership, stagedFiles, runtime);
    for (const file of actualFiles) {
      readPng(join(stagingDir, file));
    }

    verifyExactFlatFiles(stagingOwnership, stagedFiles, runtime);
    if (existingIdentity) {
      verifyOwnedDirectory(resolvedExpected, existingIdentity, parent, parentIdentity, runtime);
      const move = renameOwnedDirectory(
        resolvedExpected,
        backupDir,
        existingIdentity,
        parent,
        parentIdentity,
        runtime,
      );
      baselineLocation = "backup";
      verifyOwnedDirectory(backupDir, existingIdentity, parent, parentIdentity, runtime);
      if (move.error) {
        throw move.error;
      }
    }
    verifyExactFlatFiles(stagingOwnership, stagedFiles, runtime);
    const swap = renameOwnedDirectory(
      stagingDir,
      resolvedExpected,
      stagingIdentity,
      parent,
      parentIdentity,
      runtime,
    );
    stagingLocation = "expected";
    verifyExactFlatFiles({
      ...stagingOwnership,
      path: resolvedExpected,
    }, stagedFiles, runtime);
    if (swap.error) {
      throw swap.error;
    }
    committed = true;
  } catch (error) {
    if (!committed && stagingLocation === "expected") {
      try {
        stagingLocation = moveOwnedDirectoryAsideWithRetries(
          resolvedExpected,
          stagingIdentity,
          parent,
          parentIdentity,
          join(parent, `.${basename(resolvedExpected)}.failed-${transactionId}`),
          runtime,
        );
      } catch (rollbackError) {
        cleanupErrors.push(rollbackError);
      }
    }
    if (!committed && baselineLocation === "backup" && stagingLocation !== "expected") {
      try {
        restoreOwnedDirectoryWithRetries(
          backupDir,
          resolvedExpected,
          existingIdentity,
          parent,
          parentIdentity,
          runtime,
        );
        baselineLocation = "expected";
      } catch (rollbackError) {
        cleanupErrors.push(rollbackError);
      }
    }
    if (!committed && stagingLocation === "staging") {
      try {
        removeOwnedFlatDirectory(stagingOwnership, stagedFiles, runtime);
        stagingLocation = null;
      } catch (cleanupError) {
        cleanupErrors.push(cleanupError);
      }
    }
    if (!committed && stagingLocation === "expected") {
      try {
        verifyOwnedDirectory(resolvedExpected, stagingIdentity, parent, parentIdentity, runtime);
        verifyExactFlatFiles({
          ...stagingOwnership,
          path: resolvedExpected,
        }, stagedFiles, runtime);
        committed = true;
        for (const cleanupError of cleanupErrors) {
          (options.logger ?? console).error(
            `screenshot snapshot rollback failed after verified publication: ${errorMessage(cleanupError)}`,
          );
        }
      } catch (publicationError) {
        cleanupErrors.push(publicationError);
      }
    }
    if (!committed) {
      throwWithCleanupErrors(error, cleanupErrors, "screenshot snapshot rollback");
    }
  }

  if (baselineLocation === "backup") {
    try {
      removeOwnedFlatDirectory({
        path: backupDir,
        parent,
        parentIdentity,
        fileIdentity: existingIdentity,
        label: "prior screenshot snapshot backup",
      }, null, runtime);
    } catch (error) {
      (options.logger ?? console).error(`failed to remove prior screenshot snapshot backup: ${errorMessage(error)}`);
    }
  }
}

function copyRegularFileExclusive(source, destination, runtime, label) {
  let sourceDescriptor = null;
  let destinationDescriptor = null;
  try {
    sourceDescriptor = runtime.openSync(source, fsConstants.O_RDONLY | fsConstants.O_NOFOLLOW);
    const sourceBefore = runtime.fstatSync(sourceDescriptor);
    if (!sourceBefore.isFile()) {
      throw new Error(`refusing unsafe source ${label}: ${source}`);
    }
    const bytes = runtime.readFileSync(sourceDescriptor);
    const sourceAfter = runtime.fstatSync(sourceDescriptor);
    if (!sourceAfter.isFile() ||
      !sameFileIdentity(fileIdentityFromStat(sourceBefore), fileIdentityFromStat(sourceAfter)) ||
      sourceBefore.size !== sourceAfter.size) {
      throw new Error(`${label} source changed while reading: ${source}`);
    }

    destinationDescriptor = runtime.openSync(
      destination,
      fsConstants.O_WRONLY | fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_NOFOLLOW,
      0o600,
    );
    const destinationBefore = runtime.fstatSync(destinationDescriptor);
    if (!destinationBefore.isFile() || destinationBefore.nlink !== 1) {
      throw new Error(`refusing unsafe destination ${label}: ${destination}`);
    }
    const destinationIdentity = fileIdentityFromStat(destinationBefore);
    runtime.writeFileSync(destinationDescriptor, bytes);
    runtime.fchmodSync(destinationDescriptor, 0o644);
    runtime.fsyncSync(destinationDescriptor);
    const destinationAfter = runtime.fstatSync(destinationDescriptor);
    if (!destinationAfter.isFile() ||
      destinationAfter.nlink !== 1 ||
      !sameFileIdentity(fileIdentityFromStat(destinationAfter), destinationIdentity)) {
      throw new Error(`${label} destination changed while writing: ${destination}`);
    }
    runtime.closeSync(destinationDescriptor);
    destinationDescriptor = null;
    const linked = runtime.lstatSync(destination);
    if (linked.isSymbolicLink() || !linked.isFile() ||
      linked.nlink !== 1 ||
      !sameFileIdentity(fileIdentityFromStat(linked), destinationIdentity)) {
      throw new Error(`${label} destination changed after writing: ${destination}`);
    }
    return destinationIdentity;
  } finally {
    if (destinationDescriptor !== null) {
      runtime.closeSync(destinationDescriptor);
    }
    if (sourceDescriptor !== null) {
      runtime.closeSync(sourceDescriptor);
    }
  }
}

function verifyExactFlatFiles(ownership, expectedFiles, runtime) {
  verifyOwnedDirectoryIdentity(ownership, runtime);
  const entries = runtime.readdirSync(ownership.path).sort();
  const expectedEntries = [...expectedFiles.keys()].sort();
  if (entries.length !== expectedEntries.length ||
    entries.some((entry, index) => entry !== expectedEntries[index])) {
    throw new Error(`unexpected entries in ${ownership.label} ${ownership.path}`);
  }
  for (const entry of entries) {
    const stat = runtime.lstatSync(join(ownership.path, entry));
    if (stat.isSymbolicLink() || !stat.isFile() ||
      stat.nlink !== 1 ||
      !sameFileIdentity(fileIdentityFromStat(stat), expectedFiles.get(entry))) {
      throw new Error(`${ownership.label} entry changed: ${entry}`);
    }
  }
}

function verifyFlatPngDirectory(path, identity, parent, parentIdentity, runtime) {
  verifyOwnedDirectory(path, identity, parent, parentIdentity, runtime);
  for (const entry of runtime.readdirSync(path)) {
    if (!entry.endsWith(".png")) {
      throw new Error(`refusing unexpected screenshot snapshot entry ${join(path, entry)}`);
    }
    verifyRegularFile(join(path, entry), runtime, "screenshot snapshot");
  }
}

function renameOwnedDirectory(source, destination, identity, parent, parentIdentity, runtime) {
  if (lstatIfExists(destination, runtime)) {
    throw new Error(`screenshot transaction destination already exists: ${destination}`);
  }
  verifyOwnedDirectory(source, identity, parent, parentIdentity, runtime);
  try {
    runtime.renameNoReplaceSync(source, destination);
  } catch (error) {
    if (ownedDirectoryExists(destination, identity, parent, parentIdentity, runtime) &&
      !lstatIfExists(source, runtime)) {
      return { error };
    }
    throw error;
  }
  verifyOwnedDirectory(destination, identity, parent, parentIdentity, runtime);
  if (lstatIfExists(source, runtime)) {
    throw new Error(`screenshot transaction source remained after rename: ${source}`);
  }
  return { error: null };
}

function ownedDirectoryExists(path, identity, parent, parentIdentity, runtime) {
  try {
    verifyOwnedDirectory(path, identity, parent, parentIdentity, runtime);
    return true;
  } catch (error) {
    if (error.code === "ENOENT") {
      return false;
    }
    return false;
  }
}

function moveOwnedDirectoryAsideWithRetries(source, identity, parent, parentIdentity, prefix, runtime) {
  let lastError;
  for (let attempt = 0; attempt < SCREENSHOT_LOCK_ACQUIRE_ATTEMPTS; attempt += 1) {
    const destination = `${prefix}-${attempt}-${runtime.randomBytes(4).toString("hex")}`;
    try {
      renameOwnedDirectory(source, destination, identity, parent, parentIdentity, runtime);
      return destination;
    } catch (error) {
      lastError = error;
      if (!ownedDirectoryExists(source, identity, parent, parentIdentity, runtime)) {
        throw error;
      }
    }
  }
  throw new Error(`could not move failed screenshot snapshot aside: ${errorMessage(lastError)}`);
}

function restoreOwnedDirectoryWithRetries(source, destination, identity, parent, parentIdentity, runtime) {
  let lastError;
  for (let attempt = 0; attempt < SCREENSHOT_LOCK_ACQUIRE_ATTEMPTS; attempt += 1) {
    try {
      const move = renameOwnedDirectory(source, destination, identity, parent, parentIdentity, runtime);
      if (move.error) {
        return;
      }
      return;
    } catch (error) {
      lastError = error;
      if (ownedDirectoryExists(destination, identity, parent, parentIdentity, runtime) &&
        !lstatIfExists(source, runtime)) {
        return;
      }
    }
  }
  throw new Error(`could not restore screenshot snapshot baseline: ${errorMessage(lastError)}`);
}

function readPngFilesWithRuntime(dir, runtime) {
  const stat = lstatIfExists(dir, runtime);
  if (!stat) {
    return [];
  }
  if (stat.isSymbolicLink() || !stat.isDirectory()) {
    throw new Error(`refusing unsafe screenshot directory ${dir}`);
  }
  return runtime.readdirSync(dir).filter((file) => file.endsWith(".png")).sort();
}

function verifyRegularFile(path, runtime, label) {
  const stat = runtime.lstatSync(path);
  if (stat.isSymbolicLink() || !stat.isFile()) {
    throw new Error(`refusing unsafe ${label}: ${path}`);
  }
  return fileIdentityFromStat(stat);
}

function verifyOwnedDirectory(path, identity, parent, parentIdentity, runtime) {
  const actualParent = verifyPlainDirectory(parent, runtime, "screenshot transaction parent");
  if (!sameFileIdentity(actualParent.fileIdentity, parentIdentity)) {
    throw new Error(`screenshot transaction parent changed: ${parent}`);
  }
  assertDirectChild(parent, path, "screenshot transaction directory");
  const directory = verifyPlainDirectory(path, runtime, "screenshot transaction directory");
  if (!sameFileIdentity(directory.fileIdentity, identity)) {
    throw new Error(`screenshot transaction directory changed: ${path}`);
  }
  assertPathContained(runtime.realpathSync(parent), runtime.realpathSync(path), false, "screenshot transaction directory");
}

function readPngFiles(dir) {
  if (!existsSync(dir)) {
    return [];
  }

  return readdirSync(dir).filter((file) => file.endsWith(".png")).sort();
}

function comparePngs(actualPath, expectedPath) {
  const actual = readPng(actualPath);
  const expected = readPng(expectedPath);

  if (actual.width !== expected.width || actual.height !== expected.height) {
    return {
      ok: false,
      message: `size changed from ${expected.width}x${expected.height} to ${actual.width}x${actual.height}`,
    };
  }

  const maxChannelDelta = 8;
  const maxDiffRatio = 0.01;
  let diffPixels = 0;
  const pixelCount = actual.width * actual.height;

  for (let offset = 0; offset < actual.data.length; offset += 4) {
    if (
      Math.abs(actual.data[offset] - expected.data[offset]) > maxChannelDelta ||
      Math.abs(actual.data[offset + 1] - expected.data[offset + 1]) > maxChannelDelta ||
      Math.abs(actual.data[offset + 2] - expected.data[offset + 2]) > maxChannelDelta ||
      Math.abs(actual.data[offset + 3] - expected.data[offset + 3]) > maxChannelDelta
    ) {
      diffPixels += 1;
    }
  }

  const diffRatio = diffPixels / pixelCount;

  if (diffRatio > maxDiffRatio) {
    return { ok: false, message: `${formatPercent(diffRatio)} pixels changed; max is ${formatPercent(maxDiffRatio)}` };
  }

  return { ok: true };
}

function formatPercent(value) {
  return `${(value * 100).toFixed(2)}%`;
}

function readPng(path) {
  const bytes = readFileSync(path);
  const signature = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

  for (let index = 0; index < signature.length; index += 1) {
    if (bytes[index] !== signature[index]) {
      throw new Error(`${path} is not a PNG file`);
    }
  }

  let offset = signature.length;
  let width = 0;
  let height = 0;
  let bitDepth = 0;
  let colorType = 0;
  let interlaceMethod = 0;
  const idatChunks = [];

  while (offset < bytes.length) {
    const length = bytes.readUInt32BE(offset);
    const type = bytes.toString("ascii", offset + 4, offset + 8);
    const dataStart = offset + 8;
    const dataEnd = dataStart + length;

    if (type === "IHDR") {
      width = bytes.readUInt32BE(dataStart);
      height = bytes.readUInt32BE(dataStart + 4);
      bitDepth = bytes[dataStart + 8];
      colorType = bytes[dataStart + 9];
      interlaceMethod = bytes[dataStart + 12];
    } else if (type === "IDAT") {
      idatChunks.push(bytes.subarray(dataStart, dataEnd));
    } else if (type === "IEND") {
      break;
    }

    offset = dataEnd + 4;
  }

  if (bitDepth !== 8) {
    throw new Error(`${path} uses unsupported PNG bit depth ${bitDepth}`);
  }

  if (interlaceMethod !== 0) {
    throw new Error(`${path} uses unsupported interlaced PNG encoding`);
  }

  const bytesPerPixel = getPngBytesPerPixel(colorType, path);
  const inflated = inflateSync(Buffer.concat(idatChunks));
  const scanlineLength = width * bytesPerPixel;
  const raw = new Uint8Array(height * scanlineLength);
  let sourceOffset = 0;

  for (let y = 0; y < height; y += 1) {
    const filter = inflated[sourceOffset];
    sourceOffset += 1;
    const rowOffset = y * scanlineLength;
    const previousRowOffset = rowOffset - scanlineLength;

    for (let x = 0; x < scanlineLength; x += 1) {
      const current = inflated[sourceOffset + x];
      const left = x >= bytesPerPixel ? raw[rowOffset + x - bytesPerPixel] : 0;
      const up = y > 0 ? raw[previousRowOffset + x] : 0;
      const upLeft = y > 0 && x >= bytesPerPixel ? raw[previousRowOffset + x - bytesPerPixel] : 0;

      raw[rowOffset + x] = unfilterPngByte(filter, current, left, up, upLeft);
    }

    sourceOffset += scanlineLength;
  }

  return { width, height, data: pngToRgba(raw, width, height, colorType, bytesPerPixel, path) };
}

function getPngBytesPerPixel(colorType, path) {
  if (colorType === 0) {
    return 1;
  }

  if (colorType === 2) {
    return 3;
  }

  if (colorType === 4) {
    return 2;
  }

  if (colorType === 6) {
    return 4;
  }

  throw new Error(`${path} uses unsupported PNG color type ${colorType}`);
}

function unfilterPngByte(filter, current, left, up, upLeft) {
  if (filter === 0) {
    return current;
  }

  if (filter === 1) {
    return (current + left) & 0xff;
  }

  if (filter === 2) {
    return (current + up) & 0xff;
  }

  if (filter === 3) {
    return (current + Math.floor((left + up) / 2)) & 0xff;
  }

  if (filter === 4) {
    return (current + paeth(left, up, upLeft)) & 0xff;
  }

  throw new Error(`unsupported PNG filter ${filter}`);
}

function paeth(left, up, upLeft) {
  const estimate = left + up - upLeft;
  const leftDistance = Math.abs(estimate - left);
  const upDistance = Math.abs(estimate - up);
  const upLeftDistance = Math.abs(estimate - upLeft);

  if (leftDistance <= upDistance && leftDistance <= upLeftDistance) {
    return left;
  }

  if (upDistance <= upLeftDistance) {
    return up;
  }

  return upLeft;
}

function pngToRgba(raw, width, height, colorType, bytesPerPixel, path) {
  const rgba = new Uint8Array(width * height * 4);
  let rawOffset = 0;
  let rgbaOffset = 0;

  for (let index = 0; index < width * height; index += 1) {
    if (colorType === 0) {
      const gray = raw[rawOffset];
      rgba[rgbaOffset] = gray;
      rgba[rgbaOffset + 1] = gray;
      rgba[rgbaOffset + 2] = gray;
      rgba[rgbaOffset + 3] = 255;
    } else if (colorType === 2) {
      rgba[rgbaOffset] = raw[rawOffset];
      rgba[rgbaOffset + 1] = raw[rawOffset + 1];
      rgba[rgbaOffset + 2] = raw[rawOffset + 2];
      rgba[rgbaOffset + 3] = 255;
    } else if (colorType === 4) {
      const gray = raw[rawOffset];
      rgba[rgbaOffset] = gray;
      rgba[rgbaOffset + 1] = gray;
      rgba[rgbaOffset + 2] = gray;
      rgba[rgbaOffset + 3] = raw[rawOffset + 1];
    } else if (colorType === 6) {
      rgba[rgbaOffset] = raw[rawOffset];
      rgba[rgbaOffset + 1] = raw[rawOffset + 1];
      rgba[rgbaOffset + 2] = raw[rawOffset + 2];
      rgba[rgbaOffset + 3] = raw[rawOffset + 3];
    } else {
      throw new Error(`${path} uses unsupported PNG color type ${colorType}`);
    }

    rawOffset += bytesPerPixel;
    rgbaOffset += 4;
  }

  return rgba;
}

if (process.argv[1] && resolve(process.argv[1]) === resolve(scriptPath)) {
  process.exitCode = await runCli(process.argv.slice(2));
}
