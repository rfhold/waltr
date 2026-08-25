import { spawn } from "node:child_process";
import { mkdirSync, openSync } from "node:fs";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("..", import.meta.url));
const logDir = fileURLToPath(new URL("../.dev/logs", import.meta.url));
const launchLogDir = `${logDir}/${new Date().toISOString().replaceAll(/[:.]/g, "-")}`;
const children = new Set();
let stopping = false;

function run(command, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      cwd: repoRoot,
      env: { ...process.env, ...options.env },
      stdio: "inherit",
    });

    child.once("exit", (code, signal) => {
      if (code === 0) {
        resolve();
        return;
      }

      reject(new Error(`${command} exited with ${signal ?? code}`));
    });
  });
}

function start(command, args, options = {}) {
  const child = spawn(command, args, {
    cwd: repoRoot,
    detached: true,
    env: { ...process.env, ...options.env },
    stdio: options.stdio ?? "inherit",
  });

  child.detachedProcessGroup = true;

  children.add(child);

  child.once("exit", (code, signal) => {
    children.delete(child);

    if (!stopping) {
      stop(signal === "SIGINT" ? 130 : code ?? 1);
    }
  });

  return child;
}

function startLogged(label, command, args, options = {}) {
  mkdirSync(launchLogDir, { recursive: true });
  const log = openSync(`${launchLogDir}/${label}.log`, "wx");

  return start(command, args, {
    ...options,
    stdio: ["ignore", log, log],
  });
}

function runForeground(command, args, options = {}) {
  return new Promise((resolve) => {
    const child = spawn(command, args, {
      cwd: repoRoot,
      env: { ...process.env, ...options.env },
      stdio: "inherit",
    });

    children.add(child);

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
    process.exit(exitCode);
  }, 2_000).unref();
}

process.once("SIGINT", () => stop(130));
process.once("SIGTERM", () => stop(143));

await run("pnpm", ["--filter", "@rfhold/waltr-web", "wasm:build"]);

console.log(`web/server logs: ${launchLogDir}`);

startLogged("server", "cargo", ["run", "-p", "waltr-server"], {
  env: {
    WALTR_SERVER_ADDR: "127.0.0.1:14333",
    WALTR_SESSION_STORE: "filesystem",
  },
});
startLogged("web", "pnpm", ["--filter", "@rfhold/waltr-web", "dev:vite"], {
  env: { WALTR_WEB_PROXY_TARGET: "http://127.0.0.1:14333" },
});

const tuiExitCode = await runForeground("cargo", ["run", "-p", "waltr-tui"], {
  env: { WALTR_SERVER_ENDPOINT: "http://127.0.0.1:14333" },
});

stop(tuiExitCode);
