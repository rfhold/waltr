import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";
import * as pulumi from "@pulumi/pulumi";

interface ResourceRecord {
  type: string;
  name: string;
  inputs: Record<string, unknown>;
}

const resources: ResourceRecord[] = [];
const previousConfig = process.env.PULUMI_CONFIG;
let program: typeof import("./index");

before(async () => {
  process.env.PULUMI_CONFIG = JSON.stringify({
    "waltr:namespace": "waltr-test",
    "waltr:hostname": "waltr.example.test",
    "waltr:displayName": "Waltr Test",
    "waltr:slug": "waltr-test",
    "waltr:image": "registry.example.test/waltr:test",
    "waltr:authentikBaseUrl": "https://auth.example.test",
    "waltr:protectData": "false",
    "waltr:storageClass": "test",
    "waltr:bucketStorageClass": "test-bucket",
    "waltr:dataStorageSize": "1Gi",
    "waltr:databaseStorageSize": "1Gi",
    "waltr:s3Endpoint": "https://s3.example.test",
    "waltr:s3Prefix": "test/sessions",
    "waltr:apiAudience": "waltr-api",
    "waltr:apiScope": "waltr:use",
    "waltr:mcpScope": "mcp:use",
    "waltr:mcpOAuthAccessTokenTtl": "259200",
    "waltr:mcpOAuthRefreshTokenTtl": "86400",
    "waltr:mcpOAuthRefreshFamilyTtl": "2592000",
    "waltr:mcpOAuthCodeTtl": "300",
    "waltr:mcpOAuthAllowDcr": "true",
    "waltr:mcpOAuthAllowLoopbackRedirects": "true",
    "waltr:mcpOAuthAllowCimd": "true",
    "waltr:mcpOAuthCimdTrustedPrivateOrigins":
      '["https://client-metadata.internal.example"]',
    "waltr:mcpOutboundOAuthTrustedPrivateOrigins":
      '["https://mcp.internal.example","https://oauth.internal.example:8443"]',
    "waltr:mcpOAuthWrappingKeyVersions": '["v1"]',
    "waltr:mcpOAuthActiveWrappingKeyVersion": "v1",
  });
  pulumi.runtime.setMocks(
    {
      newResource: (args) => {
        resources.push({
          type: args.type,
          name: args.name,
          inputs: args.inputs,
        });
        const outputs: Record<string, unknown> = { ...args.inputs };
        if (args.type === "random:index/randomBytes:RandomBytes") {
          outputs.base64 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        }
        if (args.type === "tls:index/privateKey:PrivateKey") {
          outputs.privateKeyPem = "test-private-key";
        }
        if (args.type === "tls:index/selfSignedCert:SelfSignedCert") {
          outputs.certPem = "test-certificate";
        }
        if (
          args.type === "authentik:index/certificateKeyPair:CertificateKeyPair"
        ) {
          outputs.id = "signing-key";
        }
        if (args.name === "waltr-sessions-generated-config") {
          outputs.data = { BUCKET_NAME: "test-bucket" };
        }
        if (args.name === "waltr-sessions-generated-secret") {
          outputs.data = {
            AWS_ACCESS_KEY_ID: Buffer.from("access").toString("base64"),
            AWS_SECRET_ACCESS_KEY: Buffer.from("secret").toString("base64"),
          };
        }
        if (args.name === "waltr-postgres-generated-app") {
          outputs.data = {
            username: Buffer.from("app").toString("base64"),
            password: Buffer.from("password").toString("base64"),
          };
        }
        return { id: `${args.name}-id`, state: outputs };
      },
      call: (args) => ({
        ...args.inputs,
        id: "lookup-id",
        propertyMappingProviderScopeId: "mapping-id",
        data: {
          BUCKET_NAME: "test-bucket",
          username: Buffer.from("app").toString("base64"),
          password: Buffer.from("password").toString("base64"),
          AWS_ACCESS_KEY_ID: Buffer.from("access").toString("base64"),
          AWS_SECRET_ACCESS_KEY: Buffer.from("secret").toString("base64"),
        },
      }),
    },
    "waltr",
    "test",
    false,
  );
  program = await import("./index");
  await pulumi.runtime.disconnect();
});

after(() => {
  if (previousConfig === undefined) delete process.env.PULUMI_CONFIG;
  else process.env.PULUMI_CONFIG = previousConfig;
});

describe("hosted MCP OAuth deployment", () => {
  test("creates separate Authentik browser and native access clients", () => {
    const browser = resource(
      "authentik:index/providerOauth2:ProviderOauth2",
      "waltr-browser-provider",
    );
    const native = resource(
      "authentik:index/providerOauth2:ProviderOauth2",
      "waltr-native-provider",
    );

    assert.equal(browser.inputs.clientType, "confidential");
    assert.notEqual(browser.inputs.clientSecret, undefined);
    assert.deepEqual(browser.inputs.allowedRedirectUris, [
      {
        matching_mode: "strict",
        url: "https://waltr.example.test/oidc/callback",
      },
    ]);
    assert.equal(native.inputs.clientType, "public");
    assert.equal(native.inputs.clientSecret, undefined);
    assert.deepEqual(native.inputs.allowedRedirectUris, [
      {
        matching_mode: "regex",
        url: "^http://127\\.0\\.0\\.1:[0-9]+/callback$",
      },
    ]);

    const appSecret = environment();
    assert.equal(
      appSecret.WALTR_OIDC_ISSUER,
      "https://auth.example.test/application/o/waltr-test-browser/",
    );
    assert.equal(
      appSecret.WALTR_API_OIDC_ISSUER,
      "https://auth.example.test/application/o/waltr-test-native/",
    );
    assert.equal(appSecret.WALTR_API_OIDC_AUDIENCE, "waltr-api");
    assert.equal(appSecret.WALTR_API_OIDC_CLIENT_ID, "waltr-test-native");
    assert.equal(appSecret.WALTR_API_REQUIRED_SCOPES, "waltr:use");
  });

  test("removes the Authentik MCP provider and direct verifier environment", () => {
    assert.equal(
      resources.some((resource) => resource.name.includes("waltr-mcp-provider")),
      false,
    );
    assert.equal(
      resources.some((resource) => resource.name.includes("waltr-mcp-scope")),
      false,
    );
    const values = environment();
    assert.equal(values.WALTR_MCP_OIDC_ISSUER, undefined);
    assert.equal(values.WALTR_MCP_OIDC_AUDIENCE, undefined);
    assert.equal(values.WALTR_MCP_SELF_OAUTH_CLIENT_ID, undefined);
    assert.equal(values.WALTR_MCP_SELF_OAUTH_REDIRECT_URI, undefined);
    assert.equal(
      values.WALTR_MCP_OAUTH_ISSUER,
      "https://waltr.example.test/oauth",
    );
    assert.equal(values.WALTR_MCP_REQUIRED_SCOPES, "mcp:use");
    assert.equal(values.WALTR_MCP_OAUTH_ACCESS_TOKEN_TTL, "259200");
    assert.equal(values.WALTR_MCP_OAUTH_ALLOW_LOOPBACK_REDIRECTS, "true");
    assert.equal(values.WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS, undefined);
  });

  test("projects private OAuth origin lists through the application Secret", () => {
    const values = environment();
    assert.equal(
      values.WALTR_MCP_OAUTH_CIMD_TRUSTED_PRIVATE_ORIGINS,
      "https://client-metadata.internal.example",
    );
    assert.equal(
      values.WALTR_MCP_OUTBOUND_OAUTH_TRUSTED_PRIVATE_ORIGINS,
      "https://mcp.internal.example,https://oauth.internal.example:8443",
    );
  });

  test("projects versioned wrapping keys only through a read-only Secret", () => {
    const keyring = resource(
      "kubernetes:core/v1:Secret",
      "waltr-mcp-oauth-wrapping-keys",
    );
    assert.notEqual(keyring.inputs.stringData, undefined);
    assert.ok(
      Object.keys(keyring.inputs.stringData as object).some((key) =>
        key.startsWith("4dabf181"),
      ),
      "keyring stringData must remain a Pulumi secret",
    );
    const keyringData = unwrapSecrets(keyring.inputs.stringData) as Record<
      string,
      string
    >;
    const parsed = JSON.parse(keyringData["keyring.json"]);
    assert.equal(parsed.schema_version, 1);
    assert.equal(parsed.active, "v1");
    assert.deepEqual(
      parsed.keys.map(({ id }: { id: string }) => id),
      ["v1"],
    );

    const deployment = resource("kubernetes:apps/v1:Deployment", "waltr");
    const spec = deployment.inputs.spec as any;
    assert.equal(spec.template.spec.securityContext.runAsNonRoot, true);
    assert.equal(spec.template.spec.securityContext.runAsUser, 65532);
    assert.equal(spec.template.spec.securityContext.runAsGroup, 65532);
    assert.equal(spec.template.spec.securityContext.fsGroup, 65532);
    assert.equal(spec.template.spec.automountServiceAccountToken, false);
    assert.equal(
      spec.template.spec.containers[0].securityContext.runAsNonRoot,
      true,
    );
    assert.equal(
      spec.template.spec.containers[0].securityContext.readOnlyRootFilesystem,
      true,
    );
    assert.deepEqual(
      spec.template.spec.containers[0].securityContext.capabilities.drop,
      ["ALL"],
    );
    assert.ok(
      spec.template.spec.volumes.some(
        (volume: any) =>
          volume.name === "mcp-oauth-wrapping-keys" &&
          volume.secret.defaultMode === 0o440,
      ),
    );
    assert.deepEqual(
      spec.template.spec.containers[0].volumeMounts.find(
        (mount: any) => mount.name === "mcp-oauth-wrapping-keys",
      ),
      {
        name: "mcp-oauth-wrapping-keys",
        mountPath: "/var/run/secrets/waltr/mcp-oauth",
        readOnly: true,
      },
    );
    assert.notEqual(
      spec.template.metadata.annotations[
        "waltr.holdenitdown.net/wrapping-key-checksum"
      ],
      undefined,
    );
    assert.equal(
      spec.template.spec.containers[0].readinessProbe.httpGet.path,
      "/ready",
    );
    assert.equal(
      spec.template.spec.containers[0].livenessProbe.httpGet.path,
      "/health",
    );
  });

  test("uses secret-projected credentials and verified PostgreSQL TLS for both schemas", () => {
    const appSecret = resource("kubernetes:core/v1:Secret", "waltr-app");
    assert.ok(
      Object.keys(appSecret.inputs.stringData as object).some((key) =>
        key.startsWith("4dabf181"),
      ),
      "application stringData must remain a Pulumi secret",
    );
    const values = environment();
    assert.equal(values.WALTR_DATABASE_URL, values.WALTR_MCP_DATABASE_URL);
    assert.match(values.WALTR_DATABASE_URL as string, /sslmode=verify-full/);
    assert.match(
      values.WALTR_DATABASE_URL as string,
      /sslrootcert=%2Fvar%2Frun%2Fsecrets%2Fwaltr%2Fpostgres%2Fca\.crt/,
    );
    assert.doesNotMatch(values.WALTR_DATABASE_URL as string, /sslmode=disable/);

    const deployment = resource("kubernetes:apps/v1:Deployment", "waltr");
    const podSpec = (deployment.inputs.spec as any).template.spec;
    assert.deepEqual(podSpec.containers[0].envFrom, [
      { secretRef: { name: "waltr-app" } },
    ]);
    assert.ok(
      podSpec.volumes.some(
        (volume: any) =>
          volume.name === "postgres-ca" &&
          volume.secret.secretName === "waltr-postgres-ca",
      ),
    );
    assert.ok(
      podSpec.containers[0].volumeMounts.some(
        (mount: any) => mount.name === "postgres-ca" && mount.readOnly === true,
      ),
    );
  });

  test("exports telemetry with the Pulumi stack as the deployment environment", () => {
    const values = environment();
    assert.equal(values.WALTR_OTEL_EXPORT, "true");
    assert.equal(
      values.OTEL_EXPORTER_OTLP_ENDPOINT,
      "https://telemetry.holdenitdown.net:4318",
    );
    assert.equal(values.OTEL_EXPORTER_OTLP_PROTOCOL, "http/protobuf");
    assert.equal(
      values.OTEL_RESOURCE_ATTRIBUTES,
      "deployment.environment.name=test",
    );
    assert.equal(
      values.WALTR_PYROSCOPE_URL,
      "https://telemetry.holdenitdown.net:4040",
    );
  });

  test("does not project wrapping material into non-Secret resources", () => {
    for (const candidate of resources.filter(
      (resource) =>
        resource.type !== "kubernetes:core/v1:Secret" &&
        resource.type !== "random:index/randomBytes:RandomBytes",
    )) {
      assert.doesNotMatch(
        JSON.stringify(candidate.inputs),
        /AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=/,
      );
      assert.doesNotMatch(
        JSON.stringify(candidate.inputs),
        /\"active\":\"v1\"/,
      );
    }
  });

  test("adds persistent storage, backup retention, and Secret-backed object credentials", () => {
    assert.equal(
      resourceByName("waltr-sessions-bucket").inputs.kind,
      "ObjectBucketClaim",
    );
    const cluster = resourceByName("waltr-postgres").inputs.spec as any;
    assert.equal(cluster.backup.retentionPolicy, "30d");
    assert.deepEqual(cluster.backup.barmanObjectStore.s3Credentials, {
      accessKeyId: { name: "waltr-sessions", key: "AWS_ACCESS_KEY_ID" },
      secretAccessKey: { name: "waltr-sessions", key: "AWS_SECRET_ACCESS_KEY" },
    });
    const backup = resourceByName("waltr-postgres-backup").inputs;
    assert.equal(backup.kind, "ScheduledBackup");
    assert.deepEqual(backup.spec, {
      schedule: "0 0 2 * * *",
      backupOwnerReference: "self",
      cluster: { name: "waltr-postgres" },
      immediate: true,
      method: "barmanObjectStore",
    });
    assert.notEqual(
      resource("kubernetes:core/v1:PersistentVolumeClaim", "waltr-data"),
      undefined,
    );
  });

  test("runs Ceph session storage in single-writer mode", () => {
    const deployment = resource("kubernetes:apps/v1:Deployment", "waltr");
    const spec = deployment.inputs.spec as any;

    assert.equal(environment().WALTR_SESSION_S3_WRITE_MODE, "single-writer");
    assert.equal(spec.replicas, 1);
    assert.deepEqual(spec.strategy, { type: "Recreate" });
  });

  test("creates a service, streaming HTTPRoute, resources, readiness, and controlled egress", () => {
    const deployment = resource("kubernetes:apps/v1:Deployment", "waltr");
    const container = (deployment.inputs.spec as any).template.spec
      .containers[0];
    assert.deepEqual(container.resources, {
      requests: { cpu: "250m", memory: "512Mi" },
      limits: { cpu: "2", memory: "2Gi" },
    });
    assert.equal(container.readinessProbe.httpGet.path, "/ready");
    assert.equal(
      resource("kubernetes:core/v1:Service", "waltr").inputs.spec != null,
      true,
    );
    const route = resourceByName("waltr-route").inputs;
    assert.equal(route.kind, "HTTPRoute");
    assert.equal((route.spec as any).rules[0].timeouts.request, "0s");

    const policy = resource(
      "kubernetes:networking.k8s.io/v1:NetworkPolicy",
      "waltr-egress",
    );
    const egress = (policy.inputs.spec as any).egress;
    assert.deepEqual((policy.inputs.spec as any).policyTypes, ["Egress"]);
    assert.deepEqual(egress[0].to, [
      {
        namespaceSelector: {
          matchLabels: { "kubernetes.io/metadata.name": "kube-system" },
        },
      },
    ]);
    assert.deepEqual(
      egress.find((rule: any) => rule.to === undefined).ports,
      [
        { port: 443, protocol: "TCP" },
        { port: 4040, protocol: "TCP" },
        { port: 4318, protocol: "TCP" },
      ],
    );
    assert.ok(
      egress.some((rule: any) =>
        rule.ports?.some((port: any) => port.port === 5432),
      ),
    );
  });

  test("keeps all WALTR configuration out of non-Secret resources and outputs", () => {
    for (const candidate of resources.filter(
      (resource) => resource.type !== "kubernetes:core/v1:Secret",
    )) {
      assert.doesNotMatch(
        JSON.stringify(candidate.inputs),
        /"WALTR_[A-Z0-9_]+"/,
      );
    }
    const source = readFileSync(join(__dirname, "index.ts"), "utf8");
    assert.doesNotMatch(source, /-----BEGIN (?:RSA |EC )?PRIVATE KEY-----/);
    assert.doesNotMatch(
      source,
      /WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS\s*:/,
    );
    assert.deepEqual(Object.keys(program).sort(), [
      "mcpOAuthIssuer",
      "mcpOAuthJwksUrl",
      "mcpOAuthMetadataUrl",
      "mcpOAuthResource",
    ]);
  });

  test("keeps stack configuration secret-free and requires pipeline-supplied images", () => {
    const preview = readFileSync(
      join(__dirname, "Pulumi.preview.yaml"),
      "utf8",
    );
    const production = readFileSync(
      join(__dirname, "Pulumi.prod.yaml"),
      "utf8",
    );
    for (const stack of [preview, production]) {
      assert.doesNotMatch(stack, /^\s*waltr:image:/m);
      assert.doesNotMatch(
        stack,
        /(?:password|secret|privateKey|accessKeyId)\s*:/i,
      );
      assert.doesNotMatch(stack, /WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS/);
    }
    assert.match(preview, /^\s*waltr:protectData: false$/m);
    assert.match(production, /^\s*waltr:protectData: true$/m);
    assert.match(
      preview,
      /^\s*waltr:mcpOAuthAccessTokenTtl: "259200"$/m,
    );
    assert.match(
      production,
      /^\s*waltr:mcpOAuthAccessTokenTtl: "259200"$/m,
    );
  });
});

function resource(type: string, name: string): ResourceRecord {
  const match = resources.find(
    (candidate) => candidate.type === type && candidate.name === name,
  );
  if (!match) throw new Error(`missing mocked resource ${type}::${name}`);
  return match;
}

function resourceByName(name: string): ResourceRecord {
  const match = resources.find((candidate) => candidate.name === name);
  if (!match) throw new Error(`missing mocked resource ${name}`);
  return match;
}

function environment(): Record<string, unknown> {
  const appSecret = resource("kubernetes:core/v1:Secret", "waltr-app");
  return unwrapSecrets(appSecret.inputs.stringData) as Record<string, unknown>;
}

function unwrapSecrets(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(unwrapSecrets);
  if (!value || typeof value !== "object") return value;
  const record = value as Record<string, unknown>;
  if (
    Object.keys(record).some((key) => key.startsWith("4dabf181")) &&
    "value" in record
  ) {
    return unwrapSecrets(record.value);
  }
  return Object.fromEntries(
    Object.entries(record).map(([key, entry]) => [key, unwrapSecrets(entry)]),
  );
}
