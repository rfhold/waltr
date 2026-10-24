import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import * as fs from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("../", import.meta.url));
const bundleIdentifier = "dev.waltr.app";
const buildArgs = ["tauri", "build", "--debug", "--target", "aarch64-apple-darwin", "--bundles", "app", "--no-sign", "--", "--locked"];

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: "utf8", ...options });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} failed (${result.signal ?? result.status})`);
  return result.stdout ?? "";
}

async function statOrMissing(io, target) {
  try {
    return await io.lstat(target);
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}

async function validateBundle(bundle, io, runner) {
  for (const directory of [bundle, path.join(bundle, "Contents"), path.join(bundle, "Contents/MacOS")]) {
    const stat = await statOrMissing(io, directory);
    if (!stat?.isDirectory() || stat.isSymbolicLink()) throw new Error(`Invalid app directory: ${directory}`);
  }
  const plist = path.join(bundle, "Contents/Info.plist");
  const plistStat = await statOrMissing(io, plist);
  if (!plistStat?.isFile() || plistStat.isSymbolicLink()) throw new Error(`Invalid app Info.plist: ${bundle}`);
  const readKey = (key) => runner("/usr/libexec/PlistBuddy", ["-c", `Print :${key}`, plist]).trim();
  if (readKey("CFBundleIdentifier") !== bundleIdentifier) throw new Error(`Refusing app with unexpected bundle identifier: ${bundle}`);
  const executable = readKey("CFBundleExecutable");
  if (!/^[A-Za-z0-9_.-]+$/.test(executable) || [".", ".."].includes(executable)) {
    throw new Error(`Invalid app executable name: ${bundle}`);
  }
  const stat = await statOrMissing(io, path.join(bundle, "Contents/MacOS", executable));
  if (!stat?.isFile() || stat.isSymbolicLink() || !(stat.mode & 0o111)) {
    throw new Error(`Missing or invalid app executable: ${bundle}`);
  }
  return { stat: await io.lstat(bundle), executable: path.join("Contents/MacOS", executable) };
}

async function executableDigest(bundle, executable, io) {
  return createHash("sha256").update(await io.readFile(path.join(bundle, executable))).digest("hex");
}

export async function main(args = process.argv.slice(2), {
  platform = process.platform,
  arch = process.arch,
  home = homedir(),
  root = repoRoot,
  env = process.env,
  io = fs,
  runner = run,
  log = console.log,
} = {}) {
  if (args.includes("--help") && args.length === 1) {
    log("Usage: pnpm dev:install [--dry-run]\nBuild, sign and verify the Apple Silicon macOS debug app before installation at ~/Applications/Waltr.app.\nDefault: ad-hoc signature with identifier dev.waltr.app and crates/waltr-app/Entitlements.plist.\nSet WALTR_DEV_SIGNING_IDENTITY to an available stable codesign identity to opt in; no certificate discovery or creation.\nAd-hoc rebuilds do not guarantee that macOS retains Accessibility or other TCC grants.\nQuit Waltr first. This command never launches or stops the app.\n--dry-run prints paths and the build/sign/verify plan without filesystem changes or subprocesses.");
    return;
  }
  if (args.some((arg) => arg !== "--dry-run") || args.length > 1) throw new Error("Usage: pnpm dev:install [--dry-run]");
  if (platform !== "darwin" || arch !== "arm64") throw new Error("dev:install supports only native Apple Silicon macOS (darwin/arm64).");
  const signingIdentity = env.WALTR_DEV_SIGNING_IDENTITY ?? "-";
  if (!signingIdentity.trim()) throw new Error("WALTR_DEV_SIGNING_IDENTITY must not be blank; unset it for ad-hoc signing.");
  const signingPlan = `Sign staged bundle: ${signingIdentity === "-" ? "ad-hoc (-)" : "supplied WALTR_DEV_SIGNING_IDENTITY"}; identifier ${bundleIdentifier}`;
  const entitlements = path.resolve(root, "crates/waltr-app/Entitlements.plist");
  const target = path.resolve(root, "target/macos-default");
  const source = path.join(target, "aarch64-apple-darwin/debug/bundle/macos/Waltr.app");
  const parent = path.resolve(home, "Applications");
  const destination = path.join(parent, "Waltr.app");
  const cwd = path.resolve(root, "crates/waltr-app");
  if (args.includes("--dry-run")) {
    log(`Build directory: ${cwd}\nCARGO_TARGET_DIR=${target}\ncargo ${buildArgs.join(" ")}\nSource: ${source}\nInstall: ${destination}\n${signingPlan}\nEntitlements: ${entitlements}\nCompare copied executable with build output; verify staged bundle: /usr/bin/codesign --verify --deep --strict before destination replacement.\nVerify installed executable SHA-256 against the signed staged copy. Embedded revision and running-process identity are not checked.\nAd-hoc rebuilds do not guarantee that macOS retains Accessibility or other TCC grants.\nQuit Waltr before a real install. No build, sign, verify or install performed.`);
    return;
  }

  const parentStat = await statOrMissing(io, parent);
  if (parentStat && (!parentStat.isDirectory() || parentStat.isSymbolicLink())) {
    throw new Error(`Refusing unsafe Applications directory: ${parent}`);
  }
  let previous = await statOrMissing(io, destination);
  if (previous) previous = (await validateBundle(destination, io, runner)).stat;
  log("Quit Waltr before installation. Building the normal unsigned debug app...");
  log(`${signingPlan}\nAd-hoc rebuilds do not guarantee that macOS retains Accessibility or other TCC grants.`);
  runner("cargo", buildArgs, { cwd, env: { ...env, CARGO_TARGET_DIR: target }, stdio: "inherit" });
  const sourceBundle = await validateBundle(source, io, runner);
  const sourceDigest = await executableDigest(source, sourceBundle.executable, io);
  await io.mkdir(parent, { recursive: true });
  const installParent = await io.lstat(parent);
  if (!installParent.isDirectory() || installParent.isSymbolicLink()) {
    throw new Error(`Refusing unsafe Applications directory: ${parent}`);
  }
  const transaction = await io.mkdtemp(path.join(parent, ".waltr-install-"));
  const staged = path.join(transaction, "Waltr.app");
  const backup = path.join(transaction, "previous.app");
  let backedUp = false;
  let preserveTransaction = false;
  let promoted = false;
  let installError;
  let installedDigest;
  try {
    runner("/usr/bin/ditto", [source, staged]);
    const stagedBundle = await validateBundle(staged, io, runner);
    if (stagedBundle.executable !== sourceBundle.executable || await executableDigest(staged, stagedBundle.executable, io) !== sourceDigest) {
      throw new Error("Staged executable differs from the built executable; refusing installation.");
    }
    runner("/usr/bin/codesign", ["--force", "--sign", signingIdentity, "--identifier", bundleIdentifier, "--entitlements", entitlements, staged]);
    runner("/usr/bin/codesign", ["--verify", "--deep", "--strict", staged]);
    installedDigest = await executableDigest(staged, stagedBundle.executable, io);
    const current = await statOrMissing(io, destination);
    if (previous ? !current || current.dev !== previous.dev || current.ino !== previous.ino : current) {
      throw new Error(`Destination changed during build; refusing replacement: ${destination}`);
    }
    if (previous) {
      await validateBundle(destination, io, runner);
      await io.rename(destination, backup);
      backedUp = true;
    }
    try {
      await io.rename(staged, destination);
      promoted = true;
      const installedBundle = await validateBundle(destination, io, runner);
      if (installedBundle.stat.dev !== stagedBundle.stat.dev || installedBundle.stat.ino !== stagedBundle.stat.ino || installedBundle.executable !== stagedBundle.executable || await executableDigest(destination, installedBundle.executable, io) !== installedDigest) {
        throw new Error("Installed executable differs from the verified staged executable.");
      }
    } catch (error) {
      try {
        if (promoted) {
          const current = await statOrMissing(io, destination);
          if (current && (current.dev !== stagedBundle.stat.dev || current.ino !== stagedBundle.stat.ino)) {
            throw new Error("Destination changed after promotion; refusing rollback removal.");
          }
          await io.rm(destination, { recursive: true, force: true });
          promoted = false;
        }
        if (backedUp) {
          await io.rename(backup, destination);
          backedUp = false;
        }
      } catch {
        preserveTransaction = true;
        const recovery = backedUp ? `Previous app preserved at ${backup}` : `Inspect ${destination} and ${transaction}`;
        throw new Error(`Install and rollback failed. ${recovery}`, { cause: error });
      }
      throw error;
    }
  } catch (error) {
    installError = error;
  } finally {
    if (!preserveTransaction) {
      try {
        await io.rm(transaction, { recursive: true, force: true });
      } catch (cleanupError) {
        const context = `Cleanup failed: ${cleanupError.message}\nInspect transaction path: ${transaction}\nPrevious-app backup path (if retained): ${backup}`;
        if (installError) {
          throw new AggregateError([installError, cleanupError], `${installError.message}\n${context}`, { cause: installError });
        }
        const outcome = promoted ? `Installed: ${destination}, but cleanup failed.` : "Installation incomplete.";
        throw new Error(`${outcome}\n${context}`, { cause: cleanupError });
      }
    }
  }
  if (installError) throw installError;
  log(`Installed: ${destination}\nInstalled executable SHA-256: ${installedDigest}\nExecutable matches the signed, verified staged copy. Embedded revision and running-process identity were not checked.\nQuit any old Waltr process before opening manually: open ${JSON.stringify(destination)}`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(`dev:install: ${error.message}`);
    process.exitCode = 1;
  });
}
