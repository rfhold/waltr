import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const project = `waltr-worker-smoke-${process.pid}`;
const base = ["compose", "--project-name", project, "--profile", "test"];

function compose(args, { timeout = 600_000, quiet = false } = {}) {
  const result = spawnSync("docker", [...base, ...args], {
    cwd: root,
    encoding: "utf8",
    timeout,
  });
  if (!quiet || result.status !== 0) {
    if (result.stdout) process.stdout.write(result.stdout);
    if (result.stderr) process.stderr.write(result.stderr);
  }
  if (result.error || result.status !== 0) {
    throw new Error(`docker ${[...base, ...args].join(" ")} failed: ${result.error?.message ?? result.status}`);
  }
  return result.stdout.trim();
}

async function waitForReady(expected) {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    const code = compose([
      "exec", "-T", "waltr-test", "curl", "--silent", "--output", "/dev/null",
      "--write-out", "%{http_code}", "http://127.0.0.1:14333/ready",
    ], { quiet: true, timeout: 10_000 });
    if (code === expected) return;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`server /ready never returned ${expected}`);
}

try {
  compose(["up", "--build", "--detach", "--wait", "--wait-timeout", "120", "waltr-test"], { quiet: true });
  compose([
    "exec", "-T", "waltr-test-worker", "sh", "-c",
    "test -S /run/waltr-sandbox/gateway.sock && printf workspace > /workspace/.waltr-sandbox-smoke && test \"$(cat /workspace/.waltr-sandbox-smoke)\" = workspace",
  ]);
  compose(["exec", "-T", "waltr-test", "sh", "-c", "test ! -e /workspace/.waltr-sandbox-smoke"]);
  compose(["stop", "waltr-test-worker"]);
  await waitForReady("503");
  compose(["exec", "-T", "waltr-test", "curl", "--fail", "--silent", "http://127.0.0.1:14333/health"]);
  compose(["start", "waltr-test-worker"]);
  await waitForReady("200");
  console.log("container worker socket, workspace separation, and readiness recovery passed");
} catch (error) {
  console.error(error);
  try { compose(["logs", "--no-color", "waltr-test", "waltr-test-worker"]); } catch { /* startup may not have created containers */ }
  process.exitCode = 1;
} finally {
  try { compose(["down", "--volumes", "--remove-orphans"]); } catch (error) {
    console.error(error);
    process.exitCode = 1;
  }
}
