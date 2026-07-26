import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import {
  access,
  link,
  lstat,
  mkdir,
  readFile,
  readlink,
  readdir,
  realpath,
  rename,
  rm,
  symlink,
  unlink,
  writeFile,
} from "node:fs/promises";
import { constants } from "node:fs";
import { basename, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const webRoot = fileURLToPath(new URL("..", import.meta.url));
const repoRoot = fileURLToPath(new URL("../../..", import.meta.url));
const generatedRoot = join(webRoot, "generated");
const storeRoot = join(generatedRoot, ".wasm-store");
const transactionsRoot = join(storeRoot, "transactions");
const recoveryRoot = join(storeRoot, "recovery");
const versionsRoot = join(storeRoot, "versions");
const pointerPath = join(generatedRoot, "wasm-current");
const lockPath = join(storeRoot, "publish.lock");
const heartbeatPath = join(storeRoot, "publish.heartbeat");
const legacyOutputPath = join(generatedRoot, "wasm");
const ownerLeaseMs = Number(process.env.WALTR_WASM_TEST_LEASE_MS ?? 120_000);
const heartbeatIntervalMs = Number(process.env.WALTR_WASM_TEST_HEARTBEAT_INTERVAL_MS ?? Math.max(50, Math.floor(ownerLeaseMs / 3)));
const retentionGraceMs = 5 * 60_000;
const minimumPreviousVersions = 2;
const versionManifestName = ".waltr-wasm-version.json";
const requiredOutputFiles = [
  "waltr_client.js",
  "waltr_client.d.ts",
  "waltr_client_bg.wasm",
  "waltr_client_bg.wasm.d.ts",
];

export async function main(args = process.argv.slice(2)) {
  if (args.length === 0) return buildWasm();
  if (args.length === 1 && args[0] === "--cleanup") return cleanupVersions();
  throw new Error("build-wasm accepts only --cleanup; output paths are repository-owned");
}

async function buildWasm() {
  await initializeTrustedRoot();
  await recoverArtifacts();
  const transaction = await beginTransaction();
  try {
    await generateOutput(transaction.output);
    await validateGeneratedOutput(transaction.output);
    const digest = await contentDigest(transaction.output);
    const lock = await acquirePublishLock(transaction.owner, transaction.ownerPath, true);
    try {
      await holdLockForTest();
      await lock.assertOwned();
      await validatePointerIfPresent(true);
      const version = join(versionsRoot, `version-${digest}`);
      if (await exists(version)) {
        await validateContentVersion(version, true);
      } else {
        await writeVersionManifest(transaction.output, digest, now());
        await rename(transaction.output, version);
      }
      await writeVersionManifest(version, digest, now(), true);
      await crashForTest("version-finalized");
      await switchPointer(version, transaction.owner.id);
      await crashForTest("pointer-published");
      await removeLegacyOutput();
    } finally {
      await lock.release();
    }
  } finally {
    await removeOwnedTransaction(transaction.dir, transaction.owner);
  }
}

async function cleanupVersions() {
  await initializeTrustedRoot();
  await recoverArtifacts();
  const transaction = await beginTransaction();
  try {
    const lock = await acquirePublishLock(transaction.owner, transaction.ownerPath, false);
    try {
      await holdLockForTest();
      await lock.assertOwned();
      await validatePointerIfPresent();
      const current = await realpath(pointerPath);
      const versions = [];
      for (const entry of await readdir(versionsRoot, { withFileTypes: true })) {
        if (!entry.isDirectory() || entry.isSymbolicLink() || !entry.name.startsWith("version-")) {
          throw new Error(`untrusted version artifact: ${entry.name}`);
        }
        const version = join(versionsRoot, entry.name);
        const manifest = await validateContentVersion(version);
        versions.push({ version, manifest });
      }
      const previous = versions
        .filter(({ version }) => version !== current)
        .sort((left, right) => right.manifest.publishedAt - left.manifest.publishedAt);
      const preserve = new Set(previous.slice(0, minimumPreviousVersions).map(({ version }) => version));
      for (const { version, manifest } of previous) {
        if (now() - manifest.publishedAt <= retentionGraceMs) preserve.add(version);
      }
      for (const { version } of previous) {
        if (!preserve.has(version)) await removeValidatedVersion(version);
      }
    } finally {
      await lock.release();
    }
  } finally {
    await removeOwnedTransaction(transaction.dir, transaction.owner);
  }
}

async function beginTransaction() {
  const owner = await createOwner();
  const dir = join(transactionsRoot, `txn-${owner.id}`);
  const ownerWriting = join(transactionsRoot, `.owner-writing-${owner.id}`);
  const ownerCandidate = join(transactionsRoot, `.owner-${owner.id}`);
  await writeJson(ownerWriting, owner);
  await crashForTest("owner-writing-created");
  await rename(ownerWriting, ownerCandidate);
  await crashForTest("owner-candidate-published");
  await mkdir(dir);
  await crashForTest("transaction-dir-created");
  const ownerPath = join(dir, "owner.json");
  await link(ownerCandidate, ownerPath);
  await unlink(ownerCandidate);
  await crashForTest("owner-linked");
  const output = join(dir, "output");
  await mkdir(output);
  await crashForTest("output-created");
  return { owner, dir, ownerPath, output };
}

async function initializeTrustedRoot() {
  await assertRealDirectory(repoRoot);
  await assertRealDirectory(webRoot);
  await mkdir(generatedRoot, { recursive: true });
  await assertRealDirectory(generatedRoot);
  await mkdir(storeRoot, { recursive: true });
  await assertRealDirectory(storeRoot);
  await mkdir(transactionsRoot, { recursive: true });
  await mkdir(recoveryRoot, { recursive: true });
  await mkdir(versionsRoot, { recursive: true });
  await assertRealDirectory(transactionsRoot);
  await assertRealDirectory(recoveryRoot);
  await assertRealDirectory(versionsRoot);
  await validatePointerIfPresent(true);
}

async function generateOutput(output) {
  if (process.env.WALTR_WASM_BUILD_TEST_MODE) {
    if (process.env.WALTR_WASM_BUILD_TEST_MODE === "fail-generation") {
      throw new Error("injected generation failure");
    }
    const marker = process.env.WALTR_WASM_BUILD_TEST_MARKER ?? randomUUID();
    await mkdir(join(output, "snippets/current"), { recursive: true });
    for (const file of requiredOutputFiles) await writeFile(join(output, file), `${marker}:${file}`);
    await writeFile(join(output, "snippets/current/inline0.js"), `${marker}:inline0.js`);
    return;
  }
  const targetDir = process.env.CARGO_TARGET_DIR ?? join(repoRoot, "target");
  const wasmPath = join(targetDir, "wasm32-unknown-unknown/debug/waltr_client.wasm");
  await run("cargo", ["build", "-p", "waltr-client", "--lib", "--features", "wasm", "--target", "wasm32-unknown-unknown"]);
  await run("wasm-bindgen", ["--target", "web", "--out-dir", output, "--out-name", "waltr_client", wasmPath]);
}

async function acquirePublishLock(owner, ownerPath, wait) {
  const deadline = Date.now() + 30_000;
  while (true) {
    try {
      await link(ownerPath, lockPath);
      const stats = await lstat(lockPath);
      const lease = { owner, stats, stopped: false };
      lease.stopPromise = new Promise((resolveStop) => { lease.stop = resolveStop; });
      await refreshHeartbeat(lease);
      lease.task = runHeartbeat(lease);
      return {
        assertOwned: async () => {
          if (lease.lost || !await inspectOwnedLock(lease)) throw new Error("WASM publish lock ownership was lost");
        },
        release: async () => {
          lease.stopped = true;
          lease.stop();
          await lease.task;
          await releaseOwnedLock(lease);
        },
      };
    } catch (error) {
      if (error.code !== "EEXIST") throw error;
      const state = await inspectLock();
      if (state.stale) {
        await unlinkVerifiedLock(state);
        continue;
      }
      if (!wait) throw new Error("refusing cleanup while a WASM publisher may be active");
      if (Date.now() >= deadline) throw new Error("timed out waiting for the WASM publisher");
      await delay(50);
    }
  }
}

async function inspectLock() {
  let stats;
  try {
    stats = await lstat(lockPath);
  } catch (error) {
    if (error.code === "ENOENT") return { missing: true };
    throw error;
  }
  if (!stats.isFile() || stats.isSymbolicLink()) throw new Error("WASM publish lock is not a trusted regular file");
  let owner;
  try {
    owner = validateOwner(JSON.parse(await readFile(lockPath, "utf8")));
  } catch {
    return { stats, stale: now() - stats.mtimeMs > ownerLeaseMs };
  }
  if (owner.identity.kind === "linux") return { stats, owner, stale: !(await ownerIsAlive(owner)) };
  let heartbeat;
  let heartbeatStats;
  try {
    heartbeatStats = await lstat(heartbeatPath);
    if (!heartbeatStats.isFile() || heartbeatStats.isSymbolicLink()) throw new Error("untrusted heartbeat");
    heartbeat = validateHeartbeat(JSON.parse(await readFile(heartbeatPath, "utf8")), owner);
  } catch (error) {
    if (error.code === "ENOENT") return { stats, owner, stale: now() > owner.leaseUntil };
    return { stats, owner, heartbeatStats, stale: !heartbeatStats || now() - heartbeatStats.mtimeMs > ownerLeaseMs };
  }
  return { stats, owner, heartbeat, heartbeatStats, stale: now() - heartbeat.heartbeatAt > ownerLeaseMs };
}

async function unlinkVerifiedLock(state) {
  if (state.missing) return;
  let after;
  try {
    after = await lstat(lockPath);
  } catch (error) {
    if (error.code === "ENOENT") return;
    throw error;
  }
  if (state.stats.dev !== after.dev || state.stats.ino !== after.ino) return;
  await unlink(lockPath).catch(ignoreMissing);
}

async function releaseOwnedLock(lease) {
  const state = await inspectLock();
  if (!sameOwner(state.owner, lease.owner) || !sameInode(state.stats, lease.stats)) return;
  await unlinkVerifiedLock(state);
  await unlinkOwnedHeartbeat(lease.owner);
}

async function runHeartbeat(lease) {
  while (!lease.stopped) {
    await waitForHeartbeat(lease);
    if (lease.stopped) return;
    if (!await refreshHeartbeat(lease)) {
      lease.lost = true;
      return;
    }
  }
}

function waitForHeartbeat(lease) {
  return new Promise((resolveWait) => {
    const timer = setTimeout(resolveWait, heartbeatIntervalMs);
    lease.stopPromise.then(() => {
      clearTimeout(timer);
      resolveWait();
    });
  });
}

async function refreshHeartbeat(lease) {
  const before = await inspectOwnedLock(lease);
  if (!before) return false;
  const temporary = join(storeRoot, `.publish-heartbeat-${lease.owner.id}-${randomUUID()}`);
  await writeJson(temporary, {
    schema: 1,
    id: lease.owner.id,
    nonce: lease.owner.nonce,
    heartbeatAt: now(),
  });
  try {
    if (!await inspectOwnedLock(lease)) return false;
    await rename(temporary, heartbeatPath);
    return Boolean(await inspectOwnedLock(lease));
  } finally {
    await unlink(temporary).catch(ignoreMissing);
  }
}

async function inspectOwnedLock(lease) {
  try {
    const stats = await lstat(lockPath);
    if (!sameInode(stats, lease.stats)) return undefined;
    const owner = validateOwner(JSON.parse(await readFile(lockPath, "utf8")));
    return sameOwner(owner, lease.owner) ? stats : undefined;
  } catch (error) {
    if (error.code === "ENOENT") return undefined;
    throw error;
  }
}

async function unlinkOwnedHeartbeat(owner) {
  let stats;
  let heartbeat;
  try {
    stats = await lstat(heartbeatPath);
    heartbeat = validateHeartbeat(JSON.parse(await readFile(heartbeatPath, "utf8")), owner);
  } catch (error) {
    if (error.code === "ENOENT") return;
    return;
  }
  const after = await lstat(heartbeatPath).catch((error) => error.code === "ENOENT" ? undefined : Promise.reject(error));
  if (after && sameInode(stats, after) && heartbeat) await unlink(heartbeatPath).catch(ignoreMissing);
}

async function recoverArtifacts() {
  const lock = await inspectLock();
  if (lock.stale) {
    await unlinkVerifiedLock(lock);
    if (lock.owner) await unlinkOwnedHeartbeat(lock.owner);
  }
  await recoverAbandonedClaims();
  const entries = await readdir(transactionsRoot, { withFileTypes: true });
  const owners = new Map();
  for (const entry of entries) {
    const match = entry.name.match(/^\.owner(?:-writing)?-([0-9a-f-]{36})$/);
    if (!match) continue;
    if (!entry.isFile() || entry.isSymbolicLink()) throw new Error(`untrusted owner artifact: ${entry.name}`);
    let owner;
    try {
      owner = await readOwnerArtifact(join(transactionsRoot, entry.name));
    } catch (error) {
      if (error.code === "ENOENT") continue;
      throw new Error(`ambiguous owner artifact: ${entry.name}`);
    }
    if (owner.id !== match[1]) throw new Error(`owner artifact identity mismatch: ${entry.name}`);
    owners.set(owner.id, { owner, paths: [...(owners.get(owner.id)?.paths ?? []), join(transactionsRoot, entry.name)] });
  }
  for (const entry of entries) {
    const match = entry.name.match(/^txn-([0-9a-f-]{36})$/);
    if (!match) continue;
    const dir = join(transactionsRoot, entry.name);
    if (!entry.isDirectory() || entry.isSymbolicLink()) throw new Error(`untrusted transaction artifact: ${entry.name}`);
    let owner;
    try {
      owner = validateOwner(JSON.parse(await readFile(join(dir, "owner.json"), "utf8")));
    } catch (error) {
      if (error.code !== "ENOENT") throw new Error(`ambiguous transaction ownership: ${entry.name}`);
      owner = owners.get(match[1])?.owner;
      if (!owner && await recoveryClaimExists(match[1])) continue;
      if (!owner && !await exists(dir)) continue;
      if (!owner) throw new Error(`ownerless transaction is ambiguous: ${entry.name}`);
    }
    if (owner.id !== match[1]) throw new Error(`transaction identity mismatch: ${entry.name}`);
    owners.set(owner.id, { owner, dir, paths: owners.get(owner.id)?.paths ?? [] });
  }
  for (const [id, artifact] of owners) {
    if (await ownerIsAlive(artifact.owner)) continue;
    await recoverOwnerArtifacts(id, artifact);
  }
  for (const entry of await readdir(generatedRoot, { withFileTypes: true })) {
    const match = entry.name.match(/^\.wasm-pointer-([0-9a-f-]{36})$/);
    if (!match) continue;
    if (!owners.has(match[1]) && !await recoveryClaimExists(match[1])) throw new Error(`temporary pointer ownership is ambiguous: ${entry.name}`);
  }
}

async function recoverOwnerArtifacts(id, artifact) {
  const claim = join(recoveryRoot, `claim-${id}`);
  try {
    await mkdir(claim);
  } catch (error) {
    if (error.code === "EEXIST") return;
    throw error;
  }
  const recoveryOwner = await createOwner();
  const artifacts = [
    ...artifact.paths.map((source, index) => ({ source, name: `owner-${index}`, kind: "owner" })),
    ...(artifact.dir ? [{ source: artifact.dir, name: "transaction", kind: "transaction" }] : []),
    { source: join(generatedRoot, `.wasm-pointer-${id}`), name: "pointer", kind: "pointer" },
  ];
  const manifest = {
    schema: 1,
    recoveryOwner,
    subjectOwner: artifact.owner,
    artifacts: artifacts.map(({ name, kind }) => ({ name, kind })),
  };
  await writeJson(join(claim, "recovery.json"), manifest);
  const claimDelay = Number(process.env.WALTR_WASM_BUILD_TEST_RECOVERY_DELAY_MS ?? 0);
  if (claimDelay > 0) await delay(claimDelay);
  for (const item of artifacts) {
    try {
      await rename(item.source, join(claim, item.name));
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
      await assertMissing(item.source);
    }
  }
  await removeRecoveryClaim(claim, manifest);
}

async function recoverAbandonedClaims() {
  for (const entry of await readdir(recoveryRoot, { withFileTypes: true })) {
    const match = entry.name.match(/^claim-([0-9a-f-]{36})(?:-[0-9a-f-]{36})?$/);
    if (!match || !entry.isDirectory() || entry.isSymbolicLink()) throw new Error(`untrusted recovery namespace: ${entry.name}`);
    const source = join(recoveryRoot, entry.name);
    let manifest;
    try {
      manifest = await readRecoveryManifest(source);
    } catch (error) {
      const stats = await lstat(source).catch((failure) => failure.code === "ENOENT" ? undefined : Promise.reject(failure));
      if (!stats || now() - stats.mtimeMs <= ownerLeaseMs) continue;
      throw new Error(`ambiguous recovery namespace: ${entry.name}`);
    }
    if (await ownerIsAlive(manifest.recoveryOwner)) continue;
    const claimed = join(recoveryRoot, `claim-${match[1]}-${randomUUID()}`);
    try {
      await rename(source, claimed);
    } catch (error) {
      if (error.code === "ENOENT") {
        await assertMissing(source);
        continue;
      }
      throw error;
    }
    const after = await readRecoveryManifest(claimed);
    if (!sameOwner(after.recoveryOwner, manifest.recoveryOwner) || !sameOwner(after.subjectOwner, manifest.subjectOwner)) {
      throw new Error(`recovery namespace identity changed: ${entry.name}`);
    }
    await removeRecoveryClaim(claimed, after);
  }
}

async function readRecoveryManifest(claim) {
  const manifest = JSON.parse(await readFile(join(claim, "recovery.json"), "utf8"));
  validateOwner(manifest?.recoveryOwner);
  validateOwner(manifest?.subjectOwner);
  if (manifest.schema !== 1 || !Array.isArray(manifest.artifacts)
    || manifest.artifacts.some((artifact) => !["owner", "transaction", "pointer"].includes(artifact?.kind)
      || !artifact.name?.match(/^(?:owner-[0-9]+|transaction|pointer)$/))) {
    throw new Error("invalid recovery manifest");
  }
  return manifest;
}

async function removeRecoveryClaim(claim, manifest) {
  const allowed = new Map(manifest.artifacts.map((artifact) => [artifact.name, artifact.kind]));
  allowed.set("recovery.json", "manifest");
  for (const entry of await readdir(claim, { withFileTypes: true })) {
    const kind = allowed.get(entry.name);
    if (!kind) throw new Error(`untrusted recovery artifact: ${entry.name}`);
    const path = join(claim, entry.name);
    if (kind === "manifest") continue;
    if (kind === "owner") {
      if (!entry.isFile() || entry.isSymbolicLink()) throw new Error(`untrusted recovered owner: ${entry.name}`);
      const owner = await readOwnerArtifact(path);
      if (!sameOwner(owner, manifest.subjectOwner)) throw new Error(`recovered owner identity mismatch: ${entry.name}`);
      await unlink(path);
    } else if (kind === "transaction") {
      if (!entry.isDirectory() || entry.isSymbolicLink()) throw new Error("untrusted recovered transaction");
      const ownerPath = join(path, "owner.json");
      if (await exists(ownerPath)) {
        const owner = await readOwnerArtifact(ownerPath);
        if (!sameOwner(owner, manifest.subjectOwner)) throw new Error("recovered transaction identity mismatch");
      }
      await assertTreeHasNoSymlinks(path);
      await rm(path, { recursive: true });
    } else {
      if (!entry.isSymbolicLink()) throw new Error("untrusted recovered pointer");
      const target = resolve(generatedRoot, await readlink(path));
      assertBeneath(versionsRoot, target);
      await validateAnyVersion(target);
      await unlink(path);
    }
  }
  const current = await readRecoveryManifest(claim);
  if (!sameOwner(current.recoveryOwner, manifest.recoveryOwner) || !sameOwner(current.subjectOwner, manifest.subjectOwner)) {
    throw new Error("recovery manifest identity changed");
  }
  await unlink(join(claim, "recovery.json"));
  await rm(claim, { recursive: true });
}

async function recoveryClaimExists(id) {
  return (await readdir(recoveryRoot)).some((name) => name.startsWith(`claim-${id}`));
}

async function readOwnerArtifact(path) {
  let lastError;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    try {
      return validateOwner(JSON.parse(await readFile(path, "utf8")));
    } catch (error) {
      lastError = error;
      if (error.code === "ENOENT") throw error;
      await delay(10);
    }
  }
  throw lastError;
}

async function switchPointer(version, ownerId) {
  await validateAnyVersion(version);
  const temporary = join(generatedRoot, `.wasm-pointer-${ownerId}`);
  if (process.env.WALTR_WASM_BUILD_TEST_FAIL === "pointer-create") throw new Error("injected pointer creation failure");
  await symlink(relative(generatedRoot, version), temporary, "dir");
  await crashForTest("pointer-created");
  try {
    if (process.env.WALTR_WASM_BUILD_TEST_FAIL === "pointer-rename") throw new Error("injected pointer rename failure");
    await rename(temporary, pointerPath);
  } finally {
    await removeOwnedTemporaryPointer(ownerId);
  }
}

async function removeOwnedTemporaryPointer(ownerId) {
  const temporary = join(generatedRoot, `.wasm-pointer-${ownerId}`);
  let stats;
  try {
    stats = await lstat(temporary);
  } catch (error) {
    if (error.code === "ENOENT") return;
    throw error;
  }
  if (!stats.isSymbolicLink()) throw new Error("refusing to remove an untrusted temporary pointer");
  const target = await realpath(temporary);
  assertBeneath(versionsRoot, target);
  await validateAnyVersion(target);
  await unlink(temporary);
}

async function validatePointerIfPresent(allowMissingManifest = false) {
  let stats;
  try {
    stats = await lstat(pointerPath);
  } catch (error) {
    if (error.code === "ENOENT") return;
    throw error;
  }
  if (!stats.isSymbolicLink()) throw new Error("generated/wasm-current must be an owned symbolic link");
  const target = await realpath(pointerPath);
  assertBeneath(versionsRoot, target);
  if (basename(target).match(/^version-[0-9a-f]{64}$/)) {
    await validateContentVersion(target, allowMissingManifest);
  } else {
    await validateAnyVersion(target);
  }
}

async function validateAnyVersion(version) {
  if (basename(version).match(/^version-[0-9a-f]{64}$/)) return validateContentVersion(version);
  await validateGeneratedOutput(version);
  const legacyOwner = join(version, ".waltr-wasm-owner.json");
  if (!await exists(legacyOwner)) throw new Error(`untrusted immutable version: ${basename(version)}`);
}

async function validateContentVersion(version, allowMissingManifest = false) {
  await validateGeneratedOutput(version);
  const digest = await contentDigest(version);
  if (basename(version) !== `version-${digest}`) throw new Error("content-addressed version digest mismatch");
  try {
    return validateVersionManifest(JSON.parse(await readFile(join(version, versionManifestName), "utf8")), digest);
  } catch (error) {
    if (allowMissingManifest && error.code === "ENOENT") return undefined;
    throw new Error(`invalid content version manifest: ${basename(version)}`);
  }
}

async function validateGeneratedOutput(output) {
  assertBeneath(storeRoot, output);
  await assertTreeHasNoSymlinks(output);
  for (const file of requiredOutputFiles) if (!(await lstat(join(output, file))).isFile()) throw new Error(`generated output is missing ${file}`);
}

async function contentDigest(root) {
  const hash = createHash("sha256");
  async function visit(dir) {
    for (const entry of (await readdir(dir, { withFileTypes: true })).sort((a, b) => a.name.localeCompare(b.name))) {
      if (entry.name === ".waltr-wasm-owner.json" || entry.name === versionManifestName) continue;
      const path = join(dir, entry.name);
      const name = relative(root, path);
      if (entry.isDirectory()) {
        hash.update(`d\0${name}\0`);
        await visit(path);
      } else if (entry.isFile()) {
        const content = await readFile(path);
        hash.update(`f\0${name}\0${content.length}\0`);
        hash.update(content);
      } else throw new Error(`untrusted generated entry: ${name}`);
    }
  }
  await visit(root);
  return hash.digest("hex");
}

async function writeVersionManifest(version, digest, publishedAt, replace = false) {
  const manifest = { schema: 1, digest, publishedAt };
  const path = join(version, versionManifestName);
  if (!replace) return writeJson(path, manifest);
  const temporary = join(version, `.version-manifest-${randomUUID()}`);
  await writeJson(temporary, manifest);
  try {
    await rename(temporary, path);
  } finally {
    await unlink(temporary).catch(ignoreMissing);
  }
}

function validateVersionManifest(manifest, digest) {
  if (manifest?.schema !== 1 || manifest.digest !== digest || !Number.isFinite(manifest.publishedAt)) {
    throw new Error("invalid WASM version manifest");
  }
  return manifest;
}

async function removeValidatedVersion(version) {
  await validateContentVersion(version);
  await assertTreeHasNoSymlinks(version);
  await rm(version, { recursive: true });
}

async function removeLegacyOutput() {
  if (!await exists(legacyOutputPath)) return;
  const stats = await lstat(legacyOutputPath);
  if (!stats.isDirectory() || stats.isSymbolicLink()) throw new Error("refusing to remove untrusted legacy WASM output");
  const allowed = new Set([...requiredOutputFiles, "snippets"]);
  for (const entry of await readdir(legacyOutputPath, { withFileTypes: true })) {
    if (!allowed.has(entry.name) || entry.isSymbolicLink()) throw new Error(`untrusted legacy WASM artifact: ${entry.name}`);
  }
  await assertTreeHasNoSymlinks(legacyOutputPath);
  await rm(legacyOutputPath, { recursive: true });
}

async function removeOwnedTransaction(dir, owner, allowOwnerless = false) {
  assertBeneath(transactionsRoot, dir);
  let stored;
  try {
    stored = validateOwner(JSON.parse(await readFile(join(dir, "owner.json"), "utf8")));
  } catch (error) {
    if (error.code === "ENOENT" && allowOwnerless && basename(dir) === `txn-${owner.id}`) {
      await assertTreeHasNoSymlinks(dir);
      await rm(dir, { recursive: true });
      return;
    }
    if (error.code === "ENOENT") return;
    throw error;
  }
  if (stored.id !== owner.id || stored.nonce !== owner.nonce || basename(dir) !== `txn-${owner.id}`) throw new Error("transaction ownership mismatch");
  await assertTreeHasNoSymlinks(dir);
  await rm(dir, { recursive: true });
}

async function createOwner() {
  const now = Date.now();
  return { schema: 2, id: randomUUID(), nonce: randomUUID(), pid: process.pid, createdAt: now, leaseUntil: now + ownerLeaseMs, identity: await currentIdentity(process.pid) };
}

async function currentIdentity(pid) {
  if (process.env.WALTR_WASM_TEST_IDENTITY === "no-proc") return { kind: "portable" };
  if (process.platform === "linux") {
    try {
      const bootId = (await readFile("/proc/sys/kernel/random/boot_id", "utf8")).trim();
      const stat = await readFile(`/proc/${pid}/stat`, "utf8");
      const processStart = stat.slice(stat.lastIndexOf(") ") + 2).split(" ")[19];
      if (!bootId || !processStart) throw new Error("malformed Linux process identity");
      return { kind: "linux", bootId, processStart };
    } catch {
      return { kind: "portable" };
    }
  }
  return { kind: "portable" };
}

async function ownerIsAlive(owner) {
  const injected = process.env.WALTR_WASM_TEST_PID_STATE;
  if (injected === "dead") return false;
  if (injected === "live") return true;
  if (injected === "reused") return owner.identity.kind !== "linux";
  if (!pidIsAlive(owner.pid)) return false;
  if (owner.identity.kind !== "linux") return true;
  const current = await currentIdentity(owner.pid);
  if (current.kind !== "linux") return true;
  return current.bootId === owner.identity.bootId && current.processStart === owner.identity.processStart;
}

function pidIsAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error.code === "EPERM";
  }
}

function validateOwner(owner) {
  if (owner?.schema !== 2 || !owner.id?.match(/^[0-9a-f-]{36}$/) || !owner.nonce?.match(/^[0-9a-f-]{36}$/)
    || !Number.isSafeInteger(owner.pid) || !Number.isFinite(owner.createdAt) || !Number.isFinite(owner.leaseUntil)
    || !["linux", "portable"].includes(owner.identity?.kind)) throw new Error("invalid WASM transaction owner");
  if (owner.identity.kind === "linux" && (typeof owner.identity.bootId !== "string" || typeof owner.identity.processStart !== "string")) throw new Error("invalid Linux process identity");
  return owner;
}

function validateHeartbeat(heartbeat, owner) {
  if (heartbeat?.schema !== 1 || heartbeat.id !== owner.id || heartbeat.nonce !== owner.nonce
    || !Number.isFinite(heartbeat.heartbeatAt)) throw new Error("invalid WASM lock heartbeat");
  return heartbeat;
}

function sameOwner(left, right) {
  return left?.id === right?.id && left?.nonce === right?.nonce;
}

function sameInode(left, right) {
  return left?.dev === right?.dev && left?.ino === right?.ino;
}

async function assertTreeHasNoSymlinks(root) {
  assertBeneath(generatedRoot, root);
  for (const entry of await readdir(root, { withFileTypes: true })) {
    if (entry.isSymbolicLink()) throw new Error(`symbolic link in generated artifact: ${entry.name}`);
    if (entry.isDirectory()) await assertTreeHasNoSymlinks(join(root, entry.name));
  }
}

async function assertRealDirectory(path) {
  const stats = await lstat(path);
  if (!stats.isDirectory() || stats.isSymbolicLink() || await realpath(path) !== resolve(path)) throw new Error(`untrusted generated path ancestor: ${path}`);
}

async function assertMissing(path) {
  try {
    await lstat(path);
  } catch (error) {
    if (error.code === "ENOENT") return;
    throw error;
  }
  throw new Error(`artifact replacement appeared while claiming recovery: ${path}`);
}

function assertBeneath(root, path) {
  const rel = relative(root, path);
  if (!rel || rel === ".." || rel.startsWith(`..${sep}`)) throw new Error(`path escapes trusted generated root: ${path}`);
}

async function crashForTest(boundary) {
  if (process.env.WALTR_WASM_BUILD_TEST_CRASH === boundary) {
    process.kill(process.pid, "SIGKILL");
    await new Promise(() => {});
  }
}

async function holdLockForTest() {
  const duration = Number(process.env.WALTR_WASM_BUILD_TEST_HOLD_LOCK_MS ?? 0);
  if (duration > 0) await delay(duration);
}

async function writeJson(path, value) { await writeFile(path, `${JSON.stringify(value)}\n`, { flag: "wx" }); }
async function exists(path) { try { await access(path, constants.F_OK); return true; } catch { return false; } }
function ignoreMissing(error) { if (error.code !== "ENOENT") throw error; }
function now() { return Number(process.env.WALTR_WASM_BUILD_TEST_NOW_MS ?? Date.now()); }
const delay = (ms) => new Promise((resolveDelay) => setTimeout(resolveDelay, ms));

function run(command, args) {
  return new Promise((resolveRun, reject) => {
    const child = spawn(command, args, { cwd: webRoot, env: process.env, stdio: "inherit" });
    child.once("exit", (code, signal) => code === 0 ? resolveRun() : reject(new Error(`${command} exited with ${signal ?? code}`)));
  });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
