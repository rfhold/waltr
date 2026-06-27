import { spawn } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, openSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { networkInterfaces, tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import net from "node:net";
import { inflateSync } from "node:zlib";

const repoRoot = fileURLToPath(new URL("..", import.meta.url));
const logDir = fileURLToPath(new URL("../.dev/logs", import.meta.url));
const launchLogDir = `${logDir}/web-integration-${new Date().toISOString().replaceAll(/[:.]/g, "-")}`;
const serverRoot = mkdtempSync(join(tmpdir(), "waltr-web-integration-"));
const children = new Set();
let stopping = false;

const serverPort = await getFreePort();
const webPort = await getFreePort();
const serverUrl = `http://127.0.0.1:${serverPort}`;
const webUrl = `http://127.0.0.1:${webPort}`;
const dockerWebUrl = `http://${getHostAddress()}:${webPort}`;
const modelsDevFixture = `${repoRoot}/tests/web/k6/support/models-dev-api.fixture.json`;
const screenshotsEnabled = process.env.WALTR_WEB_SCREENSHOTS === "1";
const screenshotsUpdate = process.env.WALTR_WEB_UPDATE_SCREENSHOTS === "1";
const liveEnabled = process.env.WALTR_WEB_LIVE === "1";
const screenshotsDir = screenshotsEnabled
  ? `${repoRoot}/.dev/web-screenshots/current`
  : null;
const screenshotSnapshotDir = `${repoRoot}/tests/web/k6/snapshots/screenshots`;
const browserScripts = liveEnabled
  ? ["tests/web/k6/sessions/session-live.browser.js"]
  : [
    "tests/web/k6/status/landing.browser.js",
    "tests/web/k6/sessions/sessions.browser.js",
    "tests/web/k6/config/watch.browser.js",
    "tests/web/k6/config/providers.browser.js",
    "tests/web/k6/config/mcp.browser.js",
    "tests/web/k6/config/defaults.browser.js",
  ];

process.once("SIGINT", () => stop(130));
process.once("SIGTERM", () => stop(143));

mkdirSync(launchLogDir, { recursive: true });
console.log(`web integration logs: ${launchLogDir}`);
console.log(`web integration server root: ${serverRoot}`);
if (screenshotsDir) {
  rmSync(screenshotsDir, { force: true, recursive: true });
  mkdirSync(screenshotsDir, { recursive: true });
  chmodSync(screenshotsDir, 0o777);
  console.log(`web integration screenshots: ${screenshotsDir}`);
  console.log(`web integration screenshot snapshots: ${screenshotSnapshotDir}`);
}
if (liveEnabled && process.env.WALTR_AGENT_LIVE_GATEWAY !== "1") {
  console.log("skipping web live tests: set WALTR_AGENT_LIVE_GATEWAY=1");
  process.exit(0);
}

try {
  await run("pnpm", ["--filter", "@rfhold/waltr-web", "wasm:build"]);
  await run("cargo", ["build", "-p", "waltr-server"]);

  startLogged("server", `${repoRoot}/target/debug/waltr-server`, [], {
    cwd: serverRoot,
    env: {
      WALTR_SERVER_ADDR: `127.0.0.1:${serverPort}`,
      WALTR_MODELS_DEV_PATH: modelsDevFixture,
    },
  });
  await waitForHttp(`${serverUrl}/health`, "server");

  startLogged(
    "web",
    "pnpm",
    [
      "--filter",
      "@rfhold/waltr-web",
      "dev:vite",
      "--host",
      "0.0.0.0",
      "--port",
      String(webPort),
      "--strictPort",
    ],
    {
      env: { WALTR_WEB_PROXY_TARGET: serverUrl },
    },
  );
  await waitForHttp(webUrl, "web");

  let k6ExitCode = 0;

  for (const browserScript of browserScripts) {
    const dockerArgs = [
      "run",
      "--rm",
      "-v",
      `${repoRoot}:/workspace:ro`,
    ];

    if (screenshotsDir) {
      dockerArgs.push("-v", `${screenshotsDir}:/screenshots:rw`);
    }

    dockerArgs.push(
      "-w",
      "/workspace",
      "-e",
      `K6_BROWSER_HEADLESS=${process.env.K6_BROWSER_HEADLESS ?? "true"}`,
      "-e",
      `WALTR_WEB_BASE_URL=${dockerWebUrl}`,
      "-e",
      `WALTR_WEB_SCREENSHOTS=${process.env.WALTR_WEB_SCREENSHOTS ?? "0"}`,
      "-e",
      "WALTR_WEB_SCREENSHOTS_DIR=/screenshots",
      "-e",
      `WALTR_AGENT_LIVE_GATEWAY=${process.env.WALTR_AGENT_LIVE_GATEWAY ?? "0"}`,
      "grafana/k6:master-with-browser",
      "run",
      browserScript,
    );

    k6ExitCode = await runForeground("docker", dockerArgs, {
      env: {
        DOCKER_BUILDKIT: process.env.DOCKER_BUILDKIT ?? "1",
      },
    });

    if (screenshotsDir) {
      await fixScreenshotPermissions(screenshotsDir);
    }

    if (k6ExitCode !== 0) {
      break;
    }
  }

  if (screenshotsDir && k6ExitCode === 0) {
    validateScreenshots(screenshotsDir, screenshotSnapshotDir, { update: screenshotsUpdate });
  }

  stop(k6ExitCode);
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  stop(1);
}

function run(command, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      cwd: options.cwd ?? repoRoot,
      env: { ...process.env, ...options.env },
      stdio: "inherit",
    });

    child.once("error", reject);

    child.once("exit", (code, signal) => {
      if (code === 0) {
        resolve();
        return;
      }

      reject(new Error(`${command} exited with ${signal ?? code}`));
    });
  });
}

function startLogged(label, command, args, options = {}) {
  const log = openSync(`${launchLogDir}/${label}.log`, "wx");
  const child = spawn(command, args, {
    cwd: options.cwd ?? repoRoot,
    detached: true,
    env: { ...process.env, ...options.env },
    stdio: ["ignore", log, log],
  });

  child.detachedProcessGroup = true;
  children.add(child);

  child.once("error", (error) => {
    children.delete(child);

    if (!stopping) {
      console.error(`${command} failed to start: ${error.message}`);
      stop(1);
    }
  });

  child.once("exit", (code, signal) => {
    children.delete(child);

    if (!stopping) {
      stop(signal === "SIGINT" ? 130 : code ?? 1);
    }
  });

  return child;
}

function runForeground(command, args, options = {}) {
  return new Promise((resolve) => {
    const child = spawn(command, args, {
      cwd: options.cwd ?? repoRoot,
      env: { ...process.env, ...options.env },
      stdio: "inherit",
    });

    children.add(child);

    child.once("error", (error) => {
      children.delete(child);
      console.error(`${command} failed to start: ${error.message}`);
      resolve(1);
    });

    child.once("exit", (code, signal) => {
      children.delete(child);
      resolve(signal === "SIGINT" ? 130 : code ?? 1);
    });
  });
}

function killProcessGroup(child, signal) {
  if (!child.pid) {
    return;
  }

  try {
    process.kill(-child.pid, signal);
  } catch (error) {
    if (error.code !== "ESRCH") {
      throw error;
    }
  }
}

function stop(exitCode = 0) {
  if (stopping) {
    return;
  }

  stopping = true;

  for (const child of children) {
    if (child.killed) {
      continue;
    }

    if (child.detachedProcessGroup) {
      killProcessGroup(child, "SIGTERM");
    } else {
      child.kill("SIGTERM");
    }
  }

  setTimeout(() => {
    for (const child of children) {
      if (child.killed) {
        continue;
      }

      if (child.detachedProcessGroup) {
        killProcessGroup(child, "SIGKILL");
      } else {
        child.kill("SIGKILL");
      }
    }

    rmSync(serverRoot, { force: true, recursive: true });
    process.exit(exitCode);
  }, 2_000).unref();
}

function getFreePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();

    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => {
        if (!address || typeof address === "string") {
          reject(new Error("free port lookup did not return a TCP address"));
          return;
        }

        resolve(address.port);
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

  return "127.0.0.1";
}

async function fixScreenshotPermissions(screenshotsDir) {
  const exitCode = await runForeground("docker", [
    "run",
    "--rm",
    "--user",
    "0:0",
    "-v",
    `${screenshotsDir}:/screenshots:rw`,
    "--entrypoint",
    "chmod",
    "grafana/k6:master-with-browser",
    "-R",
    "a+rwX",
    "/screenshots",
  ]);

  if (exitCode !== 0) {
    throw new Error(`failed to repair screenshot permissions in ${screenshotsDir}`);
  }
}

function validateScreenshots(actualDir, expectedDir, options) {
  const actualFiles = readPngFiles(actualDir);

  if (actualFiles.length === 0) {
    throw new Error(`no screenshots were written to ${actualDir}`);
  }

  mkdirSync(expectedDir, { recursive: true });

  if (options.update) {
    for (const file of actualFiles) {
      const expectedPath = join(expectedDir, file);

      copyFileSync(join(actualDir, file), expectedPath);
      chmodSync(expectedPath, 0o644);
    }

    console.log(`updated ${actualFiles.length} screenshot snapshots in ${expectedDir}`);
    return;
  }

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

  console.log(`validated ${actualFiles.length} screenshot snapshots against ${expectedDir}`);
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

async function waitForHttp(url, label) {
  const deadline = Date.now() + 30_000;
  let lastError;

  while (Date.now() < deadline) {
    try {
      const response = await fetch(url);

      if (response.ok) {
        return;
      }

      lastError = new Error(`${label} returned HTTP ${response.status}`);
    } catch (error) {
      lastError = error;
    }

    await sleep(250);
  }

  throw new Error(`${label} did not become ready at ${url}: ${lastError}`);
}

function sleep(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}
