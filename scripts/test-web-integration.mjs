import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, openSync, rmSync } from "node:fs";
import { networkInterfaces, tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import net from "node:net";

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
const browserScripts = [
  "tests/web/k6/status/landing.browser.js",
  "tests/web/k6/sessions/sessions.browser.js",
  "tests/web/k6/config/watch.browser.js",
  "tests/web/k6/config/providers.browser.js",
  "tests/web/k6/config/mcp.browser.js",
];

process.once("SIGINT", () => stop(130));
process.once("SIGTERM", () => stop(143));

mkdirSync(launchLogDir, { recursive: true });
console.log(`web integration logs: ${launchLogDir}`);
console.log(`web integration server root: ${serverRoot}`);

try {
  await run("pnpm", ["--filter", "@rfhold/waltr-web", "wasm:build"]);
  await run("cargo", ["build", "-p", "waltr-server"]);

  startLogged("server", `${repoRoot}/target/debug/waltr-server`, [], {
    cwd: serverRoot,
    env: { WALTR_SERVER_ADDR: `127.0.0.1:${serverPort}` },
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
    k6ExitCode = await runForeground("docker", [
      "run",
      "--rm",
      "-v",
      `${repoRoot}:/workspace:ro`,
      "-w",
      "/workspace",
      "-e",
      `K6_BROWSER_HEADLESS=${process.env.K6_BROWSER_HEADLESS ?? "true"}`,
      "-e",
      `WALTR_WEB_BASE_URL=${dockerWebUrl}`,
      "grafana/k6:master-with-browser",
      "run",
      browserScript,
    ], {
      env: {
        DOCKER_BUILDKIT: process.env.DOCKER_BUILDKIT ?? "1",
      },
    });

    if (k6ExitCode !== 0) {
      break;
    }
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
