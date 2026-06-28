import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const webRoot = fileURLToPath(new URL("..", import.meta.url));
const repoRoot = fileURLToPath(new URL("../../..", import.meta.url));
const targetDir = process.env.CARGO_TARGET_DIR ?? `${repoRoot}/target`;
const wasmPath = `${targetDir}/wasm32-unknown-unknown/debug/waltr_client.wasm`;

await run("cargo", [
  "build",
  "-p",
  "waltr-client",
  "--lib",
  "--features",
  "wasm",
  "--target",
  "wasm32-unknown-unknown",
]);
await run("wasm-bindgen", [
  "--target",
  "web",
  "--out-dir",
  "generated/wasm",
  "--out-name",
  "waltr_client",
  wasmPath,
]);

function run(command, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      cwd: webRoot,
      env: process.env,
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
