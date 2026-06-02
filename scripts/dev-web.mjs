import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("..", import.meta.url));
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
    stdio: "inherit",
  });

  children.add(child);

  child.once("exit", (code, signal) => {
    children.delete(child);

    if (!stopping) {
      stop(signal === "SIGINT" ? 130 : code ?? 1);
    }
  });

  return child;
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
    killProcessGroup(child, "SIGTERM");
  }

  setTimeout(() => {
    for (const child of children) {
      killProcessGroup(child, "SIGKILL");
    }
    process.exit(exitCode);
  }, 2_000).unref();
}

process.once("SIGINT", () => stop(130));
process.once("SIGTERM", () => stop(143));

await run("pnpm", ["--filter", "@rfhold/waltr-web", "wasm:build"]);

start("cargo", ["run", "-p", "waltr-server"], {
  env: { WALTR_SERVER_ADDR: "127.0.0.1:8080" },
});
start("pnpm", ["--filter", "@rfhold/waltr-web", "dev:vite"]);
