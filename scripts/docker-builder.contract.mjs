import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const dockerfile = readFileSync(new URL("../Dockerfile", import.meta.url), "utf8");
const instructions = dockerfile.replace(/\\\r?\n\s*/g, " ").split(/\r?\n/)
  .map((line) => line.trim().replace(/\s+/g, " ")).filter((line) => line && !line.startsWith("#"));
const stages = new Map();
let current;
for (const instruction of instructions) {
  const from = /^FROM (\S+) AS (\S+)$/.exec(instruction);
  if (from) {
    current = { base: from[1], instructions: [] };
    stages.set(from[2], current);
  } else if (current) {
    current.instructions.push(instruction);
  }
}
const source = (name) => stages.get(name).instructions.join("\n");
const image = "cr.holdenitdown.net/rfhold/rust-web-ci@sha256:472959b480ef0f974ba75be2ee5aef4b6e20fd3f5fd1491b03559826ca1f30ce";

test("server and test builds inherit the verified immutable shared builder without provisioning", () => {
  assert.equal(stages.get("build-base").base, image);
  for (const name of ["server-build", "test-build"]) {
    assert.equal(stages.get(name).base, "build-base");
  }
  assert.equal(stages.has("node"), false);
  assert.doesNotMatch(dockerfile, /ARG (?:NODE_VERSION|PNPM_VERSION|WASM_BINDGEN_VERSION)\b/);
  assert.doesNotMatch(source("build-base"), /apt-get|\bnpm install|rustup|cargo install|COPY --from=node|ENV (?:PATH|PNPM_HOME|CARGO_HOME)/);
  assert.match(source("build-base"), /^WORKDIR \/workspace$/m);
});

test("workspace manifests precede frozen pnpm installation and source copies", () => {
  const base = stages.get("build-base").instructions;
  const install = base.findIndex((line) => line.startsWith("RUN "));
  assert.ok(install >= 0);
  assert.match(base[install], /pnpm install --frozen-lockfile --store-dir \/pnpm\/store --config\.dangerouslyAllowAllBuilds=true$/);
  for (const copy of [
    "COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./",
    ...["app", "server", "web"].map((name) => `COPY crates/waltr-${name}/package.json crates/waltr-${name}/package.json`),
  ]) {
    assert.ok(base.indexOf(copy) >= 0 && base.indexOf(copy) < install, copy);
  }
  for (const copy of ["COPY Cargo.toml Cargo.lock rust-toolchain.toml ./", "COPY crates crates"]) {
    assert.ok(base.indexOf(copy) > install, copy);
  }
});

test("Cargo and pnpm cache mounts retain separate amd64 and arm64 keys and paths", () => {
  for (const name of ["build-base", "worker-build"]) {
    assert.match(source(name), /^ARG TARGETARCH$/m);
  }
  const caches = {
    "build-base": { "pnpm-store": "/pnpm/store" },
    "worker-build": { "cargo-registry": "/usr/local/cargo/registry", "cargo-git": "/usr/local/cargo/git", "cargo-target": "/workspace/target" },
    "server-build": { "cargo-registry": "/usr/local/cargo/registry", "cargo-git": "/usr/local/cargo/git", "cargo-target": "/workspace/target", "pnpm-store": "/pnpm/store" },
    "test-build": { "cargo-registry": "/usr/local/cargo/registry", "cargo-git": "/usr/local/cargo/git", "cargo-target": "/workspace/target" },
  };
  for (const [name, mounts] of Object.entries(caches)) {
    for (const arch of ["amd64", "arm64"]) {
      const expanded = source(name).replaceAll("${TARGETARCH}", arch);
      for (const [key, target] of Object.entries(mounts)) {
        assert.ok(expanded.includes(`--mount=type=cache,id=waltr-${arch}-${key},target=${target}`), `${name}: ${arch}/${key}`);
      }
    }
  }
});

test("worker build stays on its independent Rust base and locked release command", () => {
  assert.match(dockerfile, /^ARG RUST_VERSION=1\.96\.0$/m);
  assert.equal(stages.get("worker-build").base, "rust:${RUST_VERSION}-slim-bookworm");
  const worker = source("worker-build");
  assert.match(worker, /apt-get install -y --no-install-recommends build-essential ca-certificates pkg-config &&/);
  assert.match(worker, /cargo build -p waltr-agent-sandbox --features shell --bin waltr-agent-sandbox --locked --release && cp \/workspace\/target\/release\/waltr-agent-sandbox \/usr\/local\/bin\/waltr-agent-sandbox$/m);
  assert.doesNotMatch(worker, /pnpm|node|wasm-bindgen/);
});

test("server revision, release outputs and runtime boundaries remain intact", () => {
  assert.match(dockerfile, /^ARG REVISION=unknown$/m);
  assert.match(source("server-build"), /^ARG REVISION$/m);
  assert.match(source("server-build"), /touch crates\/waltr-server\/build\.rs && WALTR_BUILD_REVISION="\$\{REVISION\}" cargo build -p waltr-server --features web --locked --release && cp \/workspace\/target\/release\/waltr-server \/usr\/local\/bin\/waltr-server$/m);
  assert.match(source("test-build"), /cargo build -p waltr-agent --bin waltr-agent-test-mcp-server --locked --release/);
  assert.equal(stages.get("runtime-base").base, "debian:bookworm-slim");
  assert.match(source("runtime-base"), /apt-get install -y --no-install-recommends ca-certificates curl &&/);
  assert.match(source("runtime-base"), /install -d -o 65532 -g 65532 \/data \/run\/waltr-sandbox/);
  assert.equal(stages.get("runtime").base, "runtime-base");
  assert.equal(stages.get("worker-runtime").base, "debian:bookworm-slim");
  assert.match(source("worker-runtime"), /apt-get install -y --no-install-recommends ca-certificates bash coreutils findutils &&/);
  for (const [name, build, binary, home] of [
    ["runtime", "server-build", "waltr-server", "/data"],
    ["worker-runtime", "worker-build", "waltr-agent-sandbox", "/workspace"],
  ]) {
    const runtime = source(name);
    assert.match(runtime, /^ARG REVISION$/m);
    assert.match(runtime, /org\.opencontainers\.image\.revision="\$\{REVISION\}"/);
    assert.ok(runtime.includes(`COPY --from=${build} /usr/local/bin/${binary} /usr/local/bin/${binary}`));
    assert.match(runtime, /^USER 65532:65532$/m);
    assert.ok(runtime.includes(`ENTRYPOINT ["/usr/local/bin/${binary}"]`));
    const environment = name === "runtime" ? source("runtime-base") : runtime;
    assert.ok(environment.includes(`ENV HOME=${home}`));
    assert.ok(environment.includes(`WORKDIR ${home}`));
  }
  assert.match(source("runtime"), /^EXPOSE 14333 14334$/m);
  assert.match(source("worker-runtime"), /^VOLUME \["\/workspace"\]$/m);
});
