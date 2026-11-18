import { createHash } from "node:crypto";
import { constants } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const directoryHelper = fileURLToPath(new URL("linux-directory-helper.py", import.meta.url));
const xattr = `
import errno, os, sys
op, filename, *value = sys.argv[1:]
name = "user.waltr-dev-install.v1"
if op == "set":
    os.setxattr(filename, name, value[0].encode("ascii"), flags=os.XATTR_CREATE, follow_symlinks=False)
else:
    try:
        sys.stdout.buffer.write(os.getxattr(filename, name, follow_symlinks=False))
    except OSError as error:
        if error.errno != errno.ENODATA:
            raise
`;
const buildWeb = ["--filter", "@rfhold/waltr-web", "build:tauri"];
const stageWeb = ["scripts/stage-web.mjs"];
const buildApp = ["build", "--locked", "-p", "waltr-app", "--bin", "waltr-app", "--features", "tauri/custom-protocol", "--target"];

async function statOrMissing(io, filename) {
  try {
    return await io.lstat(filename, { bigint: true });
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}

function sameInode(a, b) {
  return a?.dev === b?.dev && a?.ino === b?.ino;
}

function unchanged(a, b) {
  return sameInode(a, b) && a?.ctimeNs === b?.ctimeNs && a?.mtimeNs === b?.mtimeNs && a?.size === b?.size;
}

async function inspectDirectories(io, home, dirs, uid) {
  const stats = new Map();
  const ancestors = [];
  for (let dir = path.dirname(home); ; dir = path.dirname(dir)) {
    ancestors.unshift(dir);
    if (dir === "/") break;
  }
  for (const dir of [...ancestors, ...dirs]) {
    const stat = await statOrMissing(io, dir);
    const managed = dirs.includes(dir);
    // Allow root-owned sticky ancestors (e.g. /tmp), but not a writable home or install directory.
    const stickyAncestor = stat && !managed && stat.uid === 0n && (stat.mode & 0o1000n) !== 0n;
    const unsafe = stat
      ? !stat.isDirectory() || (stat.uid !== uid && (managed || stat.uid !== 0n)) ||
        ((stat.mode & 0o022n) !== 0n && !stickyAncestor)
      : dir === home || !managed;
    if (unsafe) {
      throw new Error(`Refusing unsafe Linux install directory: ${dir}`);
    }
    stats.set(dir, stat);
  }
  return stats;
}

function checkDirectories(previous, current, allowNew = false) {
  for (const [dir, before] of previous) {
    if (!(allowNew && !before) && !sameInode(before, current.get(dir))) {
      throw new Error(`Linux install directory changed during install: ${dir}`);
    }
  }
}

async function executableDigest(io, filename, machine, expected) {
  const invalid = () => new Error(`Missing or invalid native Linux ELF executable: ${filename}`);
  const before = await statOrMissing(io, filename);
  if (!before?.isFile() || !(before.mode & 0o100n)) throw invalid();
  const file = await io.open(filename, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const stat = await file.stat({ bigint: true });
    if (!unchanged(before, stat) || (expected && !unchanged(expected, stat))) throw invalid();

    const header = Buffer.alloc(64);
    if ((await file.read(header, 0, header.length, 0)).bytesRead !== header.length ||
        header.readUInt32BE(0) !== 0x7f454c46 || // ELF magic
        !header.subarray(4, 7).equals(Buffer.from([2, 1, 1])) || // 64-bit, little endian, version 1
        ![2, 3].includes(header.readUInt16LE(16)) || // ET_EXEC or ET_DYN (PIE)
        header.readUInt16LE(18) !== machine || header.readUInt32LE(20) !== 1 ||
        header.readUInt16LE(52) !== 64 || header.readUInt16LE(54) !== 56) throw invalid();

    const entry = header.readBigUInt64LE(24);
    const offset = header.readBigUInt64LE(32);
    const length = header.readUInt16LE(56) * 56;
    if (!length || length > 65536 || offset < 64n || offset + BigInt(length) > stat.size ||
        offset > BigInt(Number.MAX_SAFE_INTEGER)) throw invalid();
    const segments = Buffer.alloc(length);
    if ((await file.read(segments, 0, length, Number(offset))).bytesRead !== length) throw invalid();
    let executable = false;
    for (let i = 0; i < length; i += 56) {
      if (segments.readUInt32LE(i) !== 1 || !(segments.readUInt32LE(i + 4) & 1)) continue; // executable PT_LOAD
      const start = segments.readBigUInt64LE(i + 16);
      const size = segments.readBigUInt64LE(i + 32);
      if (entry !== 0n && entry >= start && entry - start < size &&
          size <= segments.readBigUInt64LE(i + 40) && segments.readBigUInt64LE(i + 8) + size <= stat.size) executable = true;
    }
    if (!executable) throw invalid();

    const hash = createHash("sha256");
    for await (const chunk of file.createReadStream({ start: 0, autoClose: false })) hash.update(chunk);
    if (!unchanged(stat, await file.stat({ bigint: true })) || !unchanged(stat, await statOrMissing(io, filename))) throw invalid();
    return { hash: hash.digest("hex"), stat };
  } finally {
    await file.close();
  }
}

export async function installLinux({ dryRun, arch, home, root, env, io, runner, log }) {
  if (!path.isAbsolute(home) || path.resolve(home) === "/") throw new Error("Linux installation requires an absolute non-root HOME.");
  home = path.resolve(home);
  root = path.resolve(root);
  const cwd = path.join(root, "crates/waltr-app");
  const target = path.join(root, "target/linux-default");
  const local = path.join(home, ".local");
  const parent = path.join(local, "bin");
  const destination = path.join(parent, "waltr-desktop");
  const rustc = env.RUSTC || "rustc";
  if (dryRun) {
    log(`Build web (${root}): WALTR_WASM_BUILD_LOCKED=1 pnpm ${buildWeb.join(" ")}\nStage web (${cwd}): node ${stageWeb.join(" ")}\nResolve native Rust target (${root}): ${rustc} -vV\nBuild desktop (${root}): CARGO_TARGET_DIR=${target} cargo ${buildApp.join(" ")} <host-triple>\nSource: ${path.join(target, "<host-triple>/debug/waltr-app")}\nInstall: ${destination}\nCheck for a native ${arch} ELF, verify staged SHA-256 and record it in the install marker before atomic promotion. Existing files must be recognized Waltr installs.\nNo build, verification, installation or launch performed.`);
    return;
  }

  const uid = BigInt(process.geteuid());
  const machine = arch === "x64" ? 62 : 183;
  const directories = () => inspectDirectories(io, home, [home, local, parent], uid);
  const python = (args) => runner("python3", args, { cwd: root, env, stdio: ["ignore", "pipe", "inherit"] });
  const marker = (op, filename, ...value) => python(["-c", xattr, op, filename, ...value]);
  const initial = await directories();
  const previous = await statOrMissing(io, destination);
  if (previous) {
    if (!previous.isFile() || previous.uid !== uid || previous.nlink !== 1n ||
        (previous.mode & 0o7022n) !== 0n || (previous.mode & 0o100n) === 0n) {
      throw new Error(`Refusing unsafe Linux install destination: ${destination}`);
    }
    const recorded = marker("get", destination);
    if (!/^[a-f0-9]{64}$/.test(recorded) ||
        recorded !== (await executableDigest(io, destination, machine, previous)).hash) {
      throw new Error(`Refusing to replace an unrecognized or modified Waltr executable: ${destination}`);
    }
  }

  const host = /^host: (\S+)$/m.exec(runner(rustc, ["-vV"], { cwd: root, env }))?.[1];
  const cpu = arch === "x64" ? "x86_64" : "aarch64";
  if (!host || !new RegExp(`^${cpu}-[a-zA-Z0-9_]+-linux-[a-zA-Z0-9_]+$`).test(host)) {
    throw new Error(`Rust toolchain is not native Linux ${arch}: ${host ?? "unknown host"}`);
  }
  log("Quit Waltr before installation. Building the native Linux debug executable...");
  runner("pnpm", buildWeb, { cwd: root, env: { ...env, WALTR_WASM_BUILD_LOCKED: "1" }, stdio: "inherit" });
  runner("node", stageWeb, { cwd, env, stdio: "inherit" });
  runner("cargo", [...buildApp, host], { cwd: root, env: { ...env, CARGO_TARGET_DIR: target }, stdio: "inherit" });

  const source = path.join(target, host, "debug/waltr-app");
  const { hash: digest } = await executableDigest(io, source, machine);
  checkDirectories(initial, await directories());
  for (const dir of [local, parent]) {
    if (!initial.get(dir)) await io.mkdir(dir, { mode: 0o755 });
  }
  const installDirs = await directories();
  checkDirectories(initial, installDirs, true);
  const transaction = await io.mkdtemp(path.join(parent, ".waltr-install-"));
  const staged = path.join(transaction, "waltr-desktop");
  const transactionStat = await io.lstat(transaction, { bigint: true });
  const checkTransaction = async () => {
    const stat = await statOrMissing(io, transaction);
    if (!sameInode(transactionStat, stat) || !stat.isDirectory() || stat.uid !== uid || (stat.mode & 0o077n) !== 0n) {
      throw new Error(`Linux install staging directory changed: ${transaction}`);
    }
  };
  let promoted = false;
  try {
    await checkTransaction();
    await io.copyFile(source, staged, constants.COPYFILE_EXCL);
    await io.chmod(staged, 0o755);
    const verified = await executableDigest(io, staged, machine);
    if (verified.hash !== digest) {
      throw new Error("Staged executable differs from the built executable; refusing installation.");
    }
    marker("set", staged, digest);
    if (marker("get", staged) !== digest) throw new Error("Could not verify Linux install marker.");

    checkDirectories(installDirs, await directories());
    await checkTransaction();
    const ready = await statOrMissing(io, staged);
    if (!sameInode(verified.stat, ready) || ready.mtimeNs !== verified.stat.mtimeNs ||
        ready.size !== verified.stat.size || ready.mode !== verified.stat.mode ||
        ready.uid !== uid || ready.nlink !== 1n) {
      throw new Error(`Verified Linux executable changed before installation: ${staged}`);
    }
    const parentStat = installDirs.get(parent);
    const previousIdentity = previous ? [previous.dev, previous.ino, previous.ctimeNs, previous.mtimeNs, previous.size] : [];
    python([directoryHelper, "promote-install", parent, path.relative(parent, staged), path.basename(destination),
      ...[parentStat.dev, parentStat.ino, ...previousIdentity].map(String)]);
    promoted = true;
  } finally {
    try {
      checkDirectories(installDirs, await directories());
      await checkTransaction();
      // Never recursively remove anything from the install directory.
      if (!promoted) {
        try {
          await io.unlink(staged);
        } catch (error) {
          if (error.code !== "ENOENT") throw error;
        }
      }
      await io.rmdir(transaction);
    } catch (error) {
      log(`Could not clean Linux install staging directory ${transaction}: ${error.message}`);
    }
  }
  log(`Installed: ${destination}\nVerified executable SHA-256: ${digest}\nWaltr was not launched.`);
}
