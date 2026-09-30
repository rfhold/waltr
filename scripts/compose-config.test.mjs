import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("..", import.meta.url));
const composePath = `${repoRoot}/compose.yml`;
const rawCompose = readFileSync(composePath, "utf8");
const dockerignore = readFileSync(`${repoRoot}/.dockerignore`, "utf8");
const dockerfile = readFileSync(`${repoRoot}/Dockerfile`, "utf8");
const packageJson = JSON.parse(readFileSync(`${repoRoot}/package.json`, "utf8"));
const caddyfile = readFileSync(`${repoRoot}/tests/web/k6/support/Caddyfile.auth`, "utf8");
const caScriptPath = `${repoRoot}/tests/web/k6/support/generate-test-ca.sh`;
const caScript = readFileSync(caScriptPath, "utf8");
const k6Entrypoint = readFileSync(`${repoRoot}/tests/web/k6/support/k6-entrypoint.sh`, "utf8");
const trustScript = readFileSync(`${repoRoot}/tests/web/k6/access/trust.browser.js`, "utf8");
const hostedOauthScript = readFileSync(`${repoRoot}/tests/web/k6/sessions/mcp-oauth.browser.js`, "utf8");
const hostedOauthMcpFixture = readFileSync(`${repoRoot}/tests/web/k6/support/resource-trees/mcp-oauth-runtime/.waltr/resources/mcp-servers/protected.yaml`, "utf8");
const hostedOauthCodingFixture = readFileSync(`${repoRoot}/tests/web/k6/support/resource-trees/mcp-oauth-runtime/.waltr/resources/features/coding.yaml`, "utf8");
const hostedOauthDefaultsFixture = readFileSync(`${repoRoot}/tests/web/k6/support/resource-trees/mcp-oauth-runtime/.waltr/resources/defaults/default.yaml`, "utf8");
const config = composeConfig("*");

const expectedServicesByProfile = {
  dev: ["waltr", "waltr-worker"],
  test: ["dex", "k6", "waltr-test", "waltr-test-worker", "mcp"],
  "test-auth": ["auth-proxy", "dex-auth", "k6-auth", "waltr-auth", "waltr-auth-worker", "mcp-auth", "test-ca"],
};

test("compose has the exact static profile topology", () => {
  const profiles = new Set();
  const servicesByProfile = {};

  for (const [serviceName, service] of Object.entries(config.services)) {
    assert.equal(service.profiles?.length, 1, `${serviceName} must have exactly one profile`);
    const [profile] = service.profiles;
    profiles.add(profile);
    servicesByProfile[profile] ??= [];
    servicesByProfile[profile].push(serviceName);
  }

  assert.deepEqual([...profiles].sort(), ["dev", "test", "test-auth"]);
  for (const [profile, expectedServices] of Object.entries(expectedServicesByProfile)) {
    assert.deepEqual(servicesByProfile[profile].sort(), expectedServices);
    assert.deepEqual(Object.keys(composeConfig(profile).services).sort(), expectedServices);
  }
});

test("compose does not interpolate ambient configuration", () => {
  assert.doesNotMatch(rawCompose, /\$\{/);
  assert.doesNotMatch(rawCompose, /^\s*env_file:/m);
  for (const [serviceName, service] of Object.entries(config.services)) {
    assert.equal(service.env_file, undefined, `${serviceName} must not use env_file`);
  }
});

test("only the dev service publishes its fixed loopback port", () => {
  const published = Object.entries(config.services).flatMap(([serviceName, service]) => (
    (service.ports ?? []).map((port) => ({ serviceName, ...port }))
  ));
  assert.deepEqual(published, [{
    serviceName: "waltr",
    mode: "ingress",
    host_ip: "127.0.0.1",
    target: 14333,
    published: "14333",
    protocol: "tcp",
  }]);
  assert.equal(config.services.waltr.volumes[0].source, "waltr-dev-data");
  assert.equal(config.services.waltr.volumes[0].target, "/data");
});

test("all Waltr services have exact static environments", () => {
  const expectedCommonEnvironment = {
    WALTR_SERVER_ADDR: "0.0.0.0:14333",
    WALTR_DATABASE_URL: "sqlite:///data/waltr.sqlite3",
    WALTR_MCP_DATABASE_URL: "sqlite:///data/mcp.sqlite3",
    WALTR_MODELS_DEV_PATH: "/fixtures/models-dev-api.fixture.json",
    WALTR_SANDBOX_SOCKET: "/run/waltr-sandbox/gateway.sock",
    WALTR_SESSION_FILESYSTEM_ROOT: "/data/sessions",
    WALTR_SESSION_STORE: "filesystem",
    WALTR_WORKSPACE_ROOT: "/workspace",
  };
  const expected = {
    waltr: {
      ...expectedCommonEnvironment,
      WALTR_AUTH_MODE: "disabled",
      WALTR_PUBLIC_URL: "http://127.0.0.1:14333",
    },
    "waltr-test": {
      ...expectedCommonEnvironment,
      WALTR_AUTH_MODE: "disabled",
      WALTR_PUBLIC_URL: "http://127.0.0.1:14333",
    },
    "waltr-auth": {
      ...expectedCommonEnvironment,
      WALTR_AUTH_MODE: "oidc",
      WALTR_MCP_REQUIRED_SCOPES: "mcp:use",
      WALTR_MCP_OAUTH_ISSUER: "https://waltr.test/oauth",
      WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE: "/run/waltr-mcp-oauth/keyring.json",
      WALTR_MCP_OAUTH_ACCESS_TOKEN_TTL: "600",
      WALTR_MCP_OAUTH_REFRESH_TOKEN_TTL: "86400",
      WALTR_MCP_OAUTH_REFRESH_FAMILY_TTL: "2592000",
      WALTR_MCP_OAUTH_CODE_TTL: "300",
      WALTR_MCP_OAUTH_ALLOW_DCR: "true",
      WALTR_MCP_OAUTH_ALLOW_LOOPBACK_REDIRECTS: "true",
      WALTR_MCP_OAUTH_ALLOW_CIMD: "true",
      WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS: "true",
      WALTR_OIDC_AUDIENCE: "waltr-access-test",
      WALTR_OIDC_CLIENT_ID: "waltr-access-test",
      WALTR_OIDC_CLIENT_SECRET: "waltr-access-test-secret",
      WALTR_OIDC_ISSUER: "https://dex.test",
      WALTR_OIDC_REDIRECT_URI: "https://waltr.test/oidc/callback",
      WALTR_OIDC_SCOPES: "openid email profile",
      WALTR_PUBLIC_URL: "https://waltr.test",
    },
  };

  for (const [serviceName, environment] of Object.entries(expected)) {
    const service = config.services[serviceName];
    assert.deepEqual(service.environment, environment, serviceName);
    const fixture = service.volumes.find((volume) => volume.target === "/fixtures/models-dev-api.fixture.json");
    assert.equal(fixture?.read_only, true);
  }
  assert.equal(config.services.waltr.tmpfs, undefined);
  assert.deepEqual(config.services["waltr-test"].tmpfs, ["/data:mode=0700,uid=65532,gid=65532"]);
  assert.deepEqual(config.services["waltr-auth"].tmpfs, [
    "/data:mode=0700,uid=65532,gid=65532",
    "/run/waltr-mcp-oauth:mode=0750,uid=65532,gid=65532",
  ]);
  const authCommand = config.services["waltr-auth"].command.join(" ");
  assert.equal(config.services["waltr-auth"].user, "0:0");
  assert.equal(config.services["waltr-test"].user, undefined);
  assert.match(authCommand, /head -c 32 \/dev\/urandom/);
  assert.match(authCommand, /chmod 0440 \/run\/waltr-mcp-oauth\/keyring\.json/);
  assert.match(authCommand, /chown 65532:65532 \/run\/waltr-mcp-oauth\/keyring\.json && exec setpriv --reuid=65532 --regid=65532 --clear-groups waltr-server$/);
  assert.equal(config.services["mcp-auth"].user, "0:0");
  assert.equal(config.services.mcp.user, undefined);
  assert.match(config.services["mcp-auth"].command.join(" "), /update-ca-certificates >\/dev\/null && exec setpriv --reuid=65532 --regid=65532 --clear-groups waltr-agent-test-mcp-server --http 0\.0\.0\.0:14666$/);
  assert.doesNotMatch(rawCompose, /WALTR_MCP_(?:OIDC|SELF|PROTECTED_OAUTH)/);
  for (const serviceName of ["waltr", "waltr-test", "mcp", "k6", "mcp-auth", "k6-auth"]) {
    assert.equal(config.services[serviceName].environment?.WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS, undefined);
  }
});

test("each profile gives only its worker a workspace and shares only its private socket", () => {
  const pairs = [
    ["waltr", "waltr-worker", "waltr-dev-sandbox-socket"],
    ["waltr-test", "waltr-test-worker", "waltr-test-sandbox-socket"],
    ["waltr-auth", "waltr-auth-worker", "waltr-auth-sandbox-socket"],
  ];
  for (const [serverName, workerName, socketVolume] of pairs) {
    const server = config.services[serverName];
    const worker = config.services[workerName];
    assert.deepEqual(worker.environment, {
      WALTR_SANDBOX_SOCKET: "/run/waltr-sandbox/gateway.sock",
      WALTR_SANDBOX_WORKSPACE_ROOT: "/workspace",
    });
    assert.equal(worker.build.target, "worker-runtime");
    assert.equal(worker.user, undefined);
    assert.ok(worker.command == null);
    assert.ok(worker.entrypoint == null);
    assert.equal(worker.depends_on, undefined);
    assert.deepEqual(servicesMounting(socketVolume), [serverName, workerName].sort());
    for (const service of [server, worker]) {
      const socketMount = service.volumes.find((volume) => volume.target === "/run/waltr-sandbox");
      assert.equal(socketMount?.type, "volume");
      assert.equal(socketMount.source, socketVolume);
      assert.notEqual(socketMount.read_only, true);
    }
    assert.equal(server.volumes.some((volume) => volume.target === "/workspace"), false);
    assert.equal(server.tmpfs?.some((mount) => mount.startsWith("/workspace")) ?? false, false);
    assert.equal(worker.volumes.some((volume) => volume.target === "/data"), false);
    assert.equal(worker.tmpfs?.some((mount) => mount.startsWith("/data")) ?? false, false);
  }
  assert.deepEqual(config.services["waltr-worker"].tmpfs, undefined);
  assert.equal(config.services["waltr-worker"].volumes.find((volume) => volume.target === "/workspace")?.source, "waltr-dev-workspace");
  assert.deepEqual(servicesMounting("waltr-dev-workspace"), ["waltr-worker"]);
  for (const workerName of ["waltr-test-worker", "waltr-auth-worker"]) {
    const worker = config.services[workerName];
    assert.equal(worker.volumes.some((volume) => volume.target === "/workspace"), false);
    assert.deepEqual(worker.tmpfs, ["/workspace:mode=0700,uid=65532,gid=65532"]);
  }
  assert.equal(config.services.waltr.volumes.find((volume) => volume.target === "/data")?.source, "waltr-dev-data");
  assert.deepEqual(servicesMounting("waltr-dev-data"), ["waltr"]);
});

test("hosted MCP OAuth resource tree uses the OIDC/TLS Waltr contract", () => {
  assert.match(hostedOauthMcpFixture, /^kind: McpServer$/m);
  assert.match(hostedOauthMcpFixture, /^  url: https:\/\/waltr\.test\/mcp$/m);
  assert.match(hostedOauthCodingFixture, /^kind: FeatureSet$/m);
  assert.match(hostedOauthCodingFixture, /^  name: coding$/m);
  assert.match(hostedOauthCodingFixture, /^      include: \[protected\]$/m);
  assert.match(hostedOauthDefaultsFixture, /^kind: Defaults$/m);
  assert.match(hostedOauthDefaultsFixture, /^    provider: k6-openai$/m);
  assert.match(hostedOauthDefaultsFixture, /^    model: gpt-5-nano$/m);
  assert.match(hostedOauthScript, /\.well-known\/oauth-client\/waltr/);
  assert.match(hostedOauthScript, /outbound-oauth\/callback/);
  assert.match(hostedOauthScript, /mcp_protected_exec/);
  assert.doesNotMatch(hostedOauthScript, /completeDexLoginViaHttp/);
});

test("test MCP authorization is enabled only for the HTTPS fixture", () => {
  assert.equal(config.services.mcp.environment, undefined);
  assert.equal(
    config.services["mcp-auth"].environment.WALTR_TEST_MCP_RESOURCE,
    "http://127.0.0.1:14666/mcp",
  );
  assert.equal(config.services["mcp-auth"].environment.WALTR_TEST_MCP_OAUTH_ISSUER, "https://dex.test");
});

test("test services use profile-local internal networks", () => {
  assert.equal(config.networks.test.internal, true);
  assert.equal(config.networks["test-auth"].internal, true);
  for (const serviceName of expectedServicesByProfile.test) {
    assert.deepEqual(Object.keys(config.services[serviceName].networks), ["test"], serviceName);
  }
  for (const serviceName of expectedServicesByProfile["test-auth"]) {
    if (serviceName === "test-ca") {
      assert.equal(config.services[serviceName].networks, undefined);
      assert.equal(config.services[serviceName].network_mode, "none");
    } else {
      assert.deepEqual(Object.keys(config.services[serviceName].networks), ["test-auth"], serviceName);
    }
  }
});

test("test containers are unprivileged and source mounts are read-only", () => {
  for (const [serviceName, service] of Object.entries(config.services)) {
    assert.notEqual(service.privileged, true, serviceName);
    assert.equal(service.cap_add, undefined, serviceName);
    assert.equal(service.devices, undefined, serviceName);
    for (const volume of service.volumes ?? []) {
      if (volume.type === "bind") {
        assert.equal(volume.read_only, true, `${serviceName}:${volume.source}`);
      }
    }
  }
});

test("every required health dependency edge is explicit", () => {
  const longRunning = ["waltr", "waltr-test", "dex", "mcp", "auth-proxy", "dex-auth", "waltr-auth", "mcp-auth"];
  for (const serviceName of longRunning) {
    assert.ok(config.services[serviceName].healthcheck?.test, `${serviceName} needs a health check`);
  }
  for (const serviceName of ["waltr", "waltr-test", "waltr-auth"]) {
    assert.match(config.services[serviceName].healthcheck.test.join(" "), /curl .*--fail .*\/ready$/);
  }
  assert.match(config.services.dex.healthcheck.test.join(" "), /well-known\/openid-configuration/);
  assert.match(config.services.mcp.healthcheck.test.join(" "), /--max-time 2 .*\/mcp/);
  assert.match(config.services["auth-proxy"].healthcheck.test.join(" "), /127\.0\.0\.1:2015\/health/);

  const expectedEdges = {
    waltr: { "waltr-worker": "service_started" },
    "waltr-test": { dex: "service_healthy", mcp: "service_healthy", "waltr-test-worker": "service_started" },
    mcp: { dex: "service_healthy" },
    k6: { dex: "service_healthy", "waltr-test": "service_healthy", mcp: "service_healthy" },
    "auth-proxy": { "test-ca": "service_completed_successfully" },
    "waltr-auth": {
      "auth-proxy": "service_healthy",
      "dex-auth": "service_healthy",
      "test-ca": "service_completed_successfully",
      "waltr-auth-worker": "service_started",
    },
    "mcp-auth": {
      "auth-proxy": "service_healthy",
      "dex-auth": "service_healthy",
      "test-ca": "service_completed_successfully",
    },
    "k6-auth": {
      "auth-proxy": "service_healthy",
      "dex-auth": "service_healthy",
      "waltr-auth": "service_healthy",
      "mcp-auth": "service_healthy",
      "test-ca": "service_completed_successfully",
    },
  };
  for (const [serviceName, dependencies] of Object.entries(expectedEdges)) {
    assert.deepEqual(dependencyConditions(config.services[serviceName]), dependencies, serviceName);
  }
  for (const serviceName of ["waltr-worker", "waltr-test-worker", "waltr-auth-worker", "dex", "test-ca", "dex-auth"]) {
    assert.equal(config.services[serviceName].depends_on, undefined, serviceName);
  }
});

test("TLS trust wiring is strict and private material is isolated", () => {
  assert.doesNotMatch(
    `${rawCompose}\n${caddyfile}\n${k6Entrypoint}`,
    /K6_INSECURE_SKIP_TLS_VERIFY|ignore-certificate-errors|NODE_TLS_REJECT_UNAUTHORIZED|tls\s+internal/,
  );
  assert.doesNotMatch(caddyfile, /tls\s+internal/);
  assert.match(caddyfile, /tls \/test-ca-caddy\/tls\.crt \/test-ca-caddy\/tls\.key/g);
  assert.match(k6Entrypoint, /certutil -N --empty-password/);
  assert.match(k6Entrypoint, /certutil -A .* -t "C,,"/);
  assert.match(k6Entrypoint, /certutil -L .* -n "Waltr integration test CA"/);
  assert.match(k6Entrypoint, /exec \/usr\/bin\/k6 "\$@"/);
  assert.match(trustScript, /await page\.goto\(endpoint/);
  assert.doesNotMatch(trustScript, /gotoWithCertVerifierRetry|ERR_CERT_VERIFIER_CHANGED/);
  assert.match(dockerfile, /apk add --no-cache ca-certificates nss-tools/);
  assert.equal(config.services["k6-auth"].environment.SSL_CERT_FILE, "/test-ca-public/ca.crt");
  assert.equal(config.services["k6-auth"].environment.K6_INSECURE_SKIP_TLS_VERIFY, undefined);
  assert.equal(config.services["k6-auth"].environment.K6_BROWSER_ARGS, undefined);
  assert.match(config.services["waltr-auth"].command.join(" "), /test-ca-public\/ca\.crt .*update-ca-certificates/);
  assert.match(config.services["mcp-auth"].command.join(" "), /test-ca-public\/ca\.crt .*update-ca-certificates/);

  assert.deepEqual(volumeTargets(config.services["test-ca"]), ["/test-ca-caddy", "/test-ca-public"]);
  assert.deepEqual(volumeTargets(config.services["auth-proxy"]), ["/etc/caddy/Caddyfile", "/test-ca-caddy"]);
  for (const serviceName of ["waltr-auth", "mcp-auth", "k6-auth"]) {
    assert.ok(volumeTargets(config.services[serviceName]).includes("/test-ca-public"));
    assert.ok(!volumeTargets(config.services[serviceName]).includes("/test-ca-caddy"));
    const caVolume = config.services[serviceName].volumes.find((volume) => volume.target === "/test-ca-public");
    assert.equal(caVolume.read_only, true);
  }
  assert.deepEqual(servicesMounting("test-ca-public"), ["k6-auth", "waltr-auth", "mcp-auth", "test-ca"]);
  assert.deepEqual(servicesMounting("test-ca-caddy"), ["auth-proxy", "test-ca"]);
});

test("TLS generator produces a verifiable two-SAN fixture with exact permissions", (context) => {
  assert.notEqual(statSync(caScriptPath).mode & 0o111, 0, "CA generator must be executable");
  assert.notEqual(statSync(`${repoRoot}/tests/web/k6/support/k6-entrypoint.sh`).mode & 0o111, 0, "k6 entrypoint must be executable");

  const root = mkdtempSync(join(realpathSync(tmpdir()), "waltr-test-ca-contract-"));
  const publicDir = join(root, "public");
  const caddyDir = join(root, "caddy");
  const workDir = join(root, "work");
  context.after(() => rmSync(root, { force: true, recursive: true }));
  mkdirSync(publicDir);
  mkdirSync(caddyDir);
  mkdirSync(workDir);
  writeFileSync(join(publicDir, ".private-key"), "must be scrubbed");
  writeFileSync(join(caddyDir, ".old-key"), "must be scrubbed");
  mkdirSync(join(publicDir, ".hidden-directory"));
  writeFileSync(join(publicDir, ".hidden-directory", "secret"), "must be scrubbed");

  const generated = spawnSync(caScriptPath, [], {
    encoding: "utf8",
    env: {
      ...process.env,
      TEST_CA_PUBLIC_DIR: publicDir,
      TEST_CA_CADDY_DIR: caddyDir,
      TMPDIR: workDir,
    },
    timeout: 30_000,
  });
  assert.ifError(generated.error);
  assert.equal(generated.signal, null, generated.stderr);
  assert.equal(generated.status, 0, generated.stderr);

  assert.deepEqual(readdirSync(publicDir), ["ca.crt"]);
  assert.deepEqual(readdirSync(caddyDir).sort(), ["tls.crt", "tls.key"]);
  assert.deepEqual(readdirSync(workDir), [], "temporary signing material must be deleted");

  const caPath = join(publicDir, "ca.crt");
  const certPath = join(caddyDir, "tls.crt");
  const keyPath = join(caddyDir, "tls.key");
  assert.equal(statSync(caPath).mode & 0o777, 0o444);
  assert.equal(statSync(certPath).mode & 0o777, 0o444);
  assert.equal(statSync(keyPath).mode & 0o777, 0o400);

  const sanOutput = openssl(["x509", "-in", certPath, "-noout", "-ext", "subjectAltName"]);
  const sans = [...sanOutput.matchAll(/DNS:([^,\s]+)/g)].map((match) => match[1]).sort();
  assert.deepEqual(sans, ["dex.test", "waltr.test"]);
  assert.match(openssl(["x509", "-in", caPath, "-noout", "-ext", "basicConstraints"]), /CA:TRUE/);
  assert.match(openssl(["x509", "-in", certPath, "-noout", "-ext", "basicConstraints"]), /CA:FALSE/);
  assert.match(openssl(["verify", "-CAfile", caPath, "-verify_hostname", "waltr.test", certPath]), /: OK/);
  assert.match(openssl(["verify", "-CAfile", caPath, "-verify_hostname", "dex.test", certPath]), /: OK/);
  assert.equal(
    openssl(["x509", "-in", certPath, "-pubkey", "-noout"]).trim(),
    openssl(["pkey", "-in", keyPath, "-pubout"]).trim(),
  );
});

test("TLS generator rejects symlinked output roots without external writes", (context) => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), "waltr-test-ca-symlink-contract-"));
  context.after(() => rmSync(root, { force: true, recursive: true }));

  for (const attackedRoot of ["public", "caddy"]) {
    const caseRoot = join(root, attackedRoot);
    const publicDir = join(caseRoot, "public");
    const caddyDir = join(caseRoot, "caddy");
    const externalDir = join(caseRoot, "external");
    const workDir = join(caseRoot, "work");
    mkdirSync(caseRoot);
    mkdirSync(externalDir);
    mkdirSync(workDir);
    writeFileSync(join(externalDir, "sentinel"), "must survive");
    if (attackedRoot === "public") {
      symlinkSync(externalDir, publicDir, "dir");
      mkdirSync(caddyDir);
    } else {
      mkdirSync(publicDir);
      symlinkSync(externalDir, caddyDir, "dir");
    }

    const generated = spawnSync(caScriptPath, [], {
      encoding: "utf8",
      env: {
        ...process.env,
        TEST_CA_PUBLIC_DIR: publicDir,
        TEST_CA_CADDY_DIR: caddyDir,
        TMPDIR: workDir,
      },
      timeout: 30_000,
    });
    assert.ifError(generated.error);
    assert.equal(generated.signal, null, generated.stderr);
    assert.notEqual(generated.status, 0, `${attackedRoot} symlink must be rejected`);
    assert.match(generated.stderr, new RegExp(`TEST_CA_${attackedRoot.toUpperCase()}_DIR must be an existing non-symlink directory`));
    assert.deepEqual(readdirSync(externalDir), ["sentinel"]);
    assert.equal(readFileSync(join(externalDir, "sentinel"), "utf8"), "must survive");
    assert.deepEqual(readdirSync(workDir), []);
    assert.deepEqual(readdirSync(attackedRoot === "public" ? caddyDir : publicDir), []);
  }
});

test("project-built services do not use shared image tags", () => {
  for (const serviceName of ["waltr", "waltr-worker", "waltr-test", "waltr-test-worker", "mcp", "k6", "test-ca", "waltr-auth", "waltr-auth-worker", "mcp-auth", "k6-auth"]) {
    assert.ok(config.services[serviceName].build, `${serviceName} must be project-built`);
    assert.equal(config.services[serviceName].image, undefined, `${serviceName} must use its project-scoped image`);
  }
});

test("production and test services use isolated runtime targets", () => {
  assert.equal(config.services.waltr.build.target, "runtime");
  for (const serviceName of ["waltr-test", "mcp", "waltr-auth", "mcp-auth"]) {
    assert.equal(config.services[serviceName].build.target, "test-runtime", serviceName);
  }

  const runtime = dockerStage("runtime");
  const testRuntime = dockerStage("test-runtime");
  const workerRuntime = dockerStage("worker-runtime");
  assert.match(runtime, /COPY --from=server-build \/usr\/local\/bin\/waltr-server \/usr\/local\/bin\/waltr-server/);
  assert.doesNotMatch(runtime, /waltr-agent-test-mcp-server/);
  assert.match(runtime, /USER 65532:65532/);
  assert.match(runtime, /ENTRYPOINT \["\/usr\/local\/bin\/waltr-server"\]/);
  assert.doesNotMatch(runtime, /^CMD /m);
  assert.match(testRuntime, /waltr-agent-test-mcp-server/);
  assert.match(testRuntime, /apt-get install -y --no-install-recommends [^\n]*util-linux/);
  assert.match(testRuntime, /^USER 65532:65532$/m);
  assert.match(workerRuntime, /^USER 65532:65532$/m);
  assert.match(dockerStage("runtime-base"), /install -d -o 65532 -g 65532 \/data \/run\/waltr-sandbox/);
  assert.match(workerRuntime, /install -d -o 65532 -g 65532 \/workspace \/run\/waltr-sandbox/);
  assert.match(dockerfile, /ENV HOME=\/data/);
  assert.match(dockerfile, /WORKDIR \/data/);
});

test("runtime and build context contain required hardening", () => {
  const buildBase = dockerStage("build-base");
  const serverBuild = dockerStage("server-build");
  const testBuild = dockerStage("test-build");
  assert.match(dockerStage("runtime-base"), /apt-get install -y --no-install-recommends ca-certificates curl/);
  assert.match(dockerStage("test-runtime"), /apt-get install -y --no-install-recommends bash coreutils findutils procps/);
  assert.match(dockerfile, /ENV PNPM_HOME=\/pnpm/);
  assert.equal((dockerfile.match(/pnpm install --frozen-lockfile/g) ?? []).length, 1);
  assert.equal((dockerfile.match(/pnpm --filter @rfhold\/waltr-web build/g) ?? []).length, 0);
  assert.doesNotMatch(dockerfile, /^(?:COPY|ADD)\s+(?:\.|\.\/)\s+(?:\.|\.\/|\/workspace)\/?$/m);
  const installIndex = dockerfile.indexOf("pnpm install --frozen-lockfile");
  for (const instruction of [
    "COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./",
    "COPY crates/waltr-web/package.json crates/waltr-web/package.json",
  ]) {
    const copyIndex = dockerfile.indexOf(instruction);
    assert.notEqual(copyIndex, -1, `${instruction} must exist`);
    assert.ok(copyIndex < installIndex, `${instruction} must precede pnpm install`);
  }
  for (const instruction of [
    "COPY Cargo.toml Cargo.lock rust-toolchain.toml ./",
    "COPY crates crates",
  ]) {
    const copyIndex = dockerfile.indexOf(instruction);
    assert.notEqual(copyIndex, -1, `${instruction} must exist`);
    assert.ok(copyIndex > installIndex, `${instruction} must follow pnpm install`);
  }
  assert.match(dockerfile, /cargo build -p waltr-server --features web/);
  assert.doesNotMatch(buildBase, /^ARG REVISION$/m);
  assert.match(serverBuild, /^ARG REVISION$/m);
  assert.doesNotMatch(testBuild, /^ARG REVISION$/m);
  assert.match(dockerfile, /^ARG REVISION=unknown$/m);
  assert.match(dockerfile, /WALTR_BUILD_REVISION="\$\{REVISION\}" cargo build/);
  assert.match(dockerfile, /org\.opencontainers\.image\.source="https:\/\/git\.holdenitdown\.net\/rfhold\/waltr"/);
  assert.match(dockerfile, /org\.opencontainers\.image\.revision="\$\{REVISION\}"/);
  for (const cache of ["cargo-registry", "cargo-git", "pnpm-store"]) {
    assert.match(buildBase, new RegExp(`id=waltr-\\$\\{TARGETARCH\\}-${cache}`));
  }
  for (const cache of ["cargo-registry", "cargo-git", "cargo-target", "pnpm-store"]) {
    assert.match(serverBuild, new RegExp(`id=waltr-\\$\\{TARGETARCH\\}-${cache}`));
  }
  for (const cache of ["cargo-registry", "cargo-git", "cargo-target"]) {
    assert.match(testBuild, new RegExp(`id=waltr-\\$\\{TARGETARCH\\}-${cache}`));
  }
  assert.doesNotMatch(dockerfile, /FROM --platform=/);
  const entries = new Set(dockerignore.split("\n").map(normalizeIgnoreEntry).filter(Boolean));
  for (const entry of [
    ".env*",
    ".envrc",
    ".waltr",
    ".npmrc",
    "*.sqlite3*",
    "*.key",
    "tests/web/k6/snapshots",
    ".dev",
    ".agents",
    ".tekton",
    ".worktrees",
    "docs",
    "infra",
    "scripts",
    "proto",
  ]) {
    assert.ok(entries.has(entry), `.dockerignore must contain ${entry}`);
  }
});

test("package aliases use explicit runner flags without environment prefixes", () => {
  assert.deepEqual({
    integration: packageJson.scripts["test:web:integration"],
    noAuth: packageJson.scripts["test:web:integration:no-auth"],
    auth: packageJson.scripts["test:web:integration:auth"],
    trust: packageJson.scripts["test:web:trust"],
    dev: packageJson.scripts["test:web:integration:dev"],
    live: packageJson.scripts["test:web:live"],
    screenshots: packageJson.scripts["test:web:screenshots"],
    updateScreenshots: packageJson.scripts["test:web:screenshots:update"],
  }, {
    integration: "node scripts/test-web-integration.mjs --mode compose --access both",
    noAuth: "node scripts/test-web-integration.mjs --mode compose --access disabled",
    auth: "node scripts/test-web-integration.mjs --mode compose --access oidc",
    trust: "node scripts/test-web-integration.mjs --mode compose --access oidc --script access/trust.browser.js",
    dev: "node scripts/test-web-integration.mjs --mode dev --access disabled",
    live: "node scripts/test-web-integration.mjs --mode compose --access disabled --live",
    screenshots: "node scripts/test-web-integration.mjs --mode compose --access disabled --screenshots",
    updateScreenshots: "node scripts/test-web-integration.mjs --mode compose --access disabled --screenshots --update-screenshots",
  });
});

function composeConfig(profile) {
  const environment = Object.fromEntries(Object.entries(process.env).filter(([name]) => (
    !name.startsWith("WALTR_") && !name.startsWith("K6_") && !name.startsWith("COMPOSE_")
  )));
  environment.COMPOSE_DISABLE_ENV_FILE = "1";
  const result = spawnSync(
    "docker",
    ["compose", "--file", composePath, "--profile", profile, "config", "--format", "json"],
    { cwd: repoRoot, encoding: "utf8", env: environment, timeout: 30_000 },
  );
  assert.ifError(result.error);
  assert.equal(result.signal, null, result.stderr);
  assert.equal(result.status, 0, result.stderr);
  return JSON.parse(result.stdout);
}

function volumeTargets(service) {
  return (service.volumes ?? []).map((volume) => volume.target).sort();
}

function servicesMounting(source) {
  return Object.entries(config.services)
    .filter(([, service]) => (service.volumes ?? []).some((volume) => volume.source === source))
    .map(([serviceName]) => serviceName)
    .sort();
}

function dependencyConditions(service) {
  return Object.fromEntries(Object.entries(service.depends_on ?? {}).map(([name, dependency]) => (
    [name, dependency.condition]
  )));
}

function openssl(args) {
  const result = spawnSync("openssl", args, { encoding: "utf8", timeout: 10_000 });
  assert.ifError(result.error);
  assert.equal(result.signal, null, result.stderr);
  assert.equal(result.status, 0, result.stderr);
  return result.stdout;
}

function normalizeIgnoreEntry(entry) {
  return entry.trim().replace(/^(?:\/|\*\*\/)/, "").replace(/\/$/, "");
}

function dockerStage(name) {
  const match = dockerfile.match(new RegExp(`^FROM [^\\n]+ AS ${name}\\n([\\s\\S]*?)(?=^FROM |(?![\\s\\S]))`, "m"));
  assert.ok(match, `Dockerfile stage ${name} must exist`);
  return match[1];
}
