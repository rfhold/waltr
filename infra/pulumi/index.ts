import * as authentik from "@pulumi/authentik";
import * as k8s from "@pulumi/kubernetes";
import * as pulumi from "@pulumi/pulumi";
import * as random from "@pulumi/random";
import * as tls from "@pulumi/tls";
import { createHash } from "node:crypto";
import { OAuthApplication } from "./authentik";

const config = new pulumi.Config();
const namespaceName = config.require("namespace");
const hostname = config.require("hostname");
const displayName = config.require("displayName");
const slug = config.require("slug");
const image = config.require("image");
const authentikBaseUrl = config.require("authentikBaseUrl").replace(/\/$/, "");
const storageClass = config.require("storageClass");
const bucketStorageClass = config.require("bucketStorageClass");
const dataStorageSize = config.require("dataStorageSize");
const databaseStorageSize = config.require("databaseStorageSize");
const s3Endpoint = config.require("s3Endpoint");
const s3Prefix = config.require("s3Prefix");
const apiAudience = config.require("apiAudience");
const apiScope = config.require("apiScope");
const mcpScope = config.require("mcpScope");
const mcpOAuthAccessTokenTtl = config.require("mcpOAuthAccessTokenTtl");
const mcpOAuthRefreshTokenTtl = config.require("mcpOAuthRefreshTokenTtl");
const mcpOAuthRefreshFamilyTtl = config.require("mcpOAuthRefreshFamilyTtl");
const mcpOAuthCodeTtl = config.require("mcpOAuthCodeTtl");
const mcpOAuthAllowDcr = config.requireBoolean("mcpOAuthAllowDcr");
const mcpOAuthAllowLoopbackRedirects = config.requireBoolean(
  "mcpOAuthAllowLoopbackRedirects",
);
const mcpOAuthAllowCimd = config.requireBoolean("mcpOAuthAllowCimd");
const mcpOAuthCimdTrustedPrivateOrigins =
  config.getObject<string[]>("mcpOAuthCimdTrustedPrivateOrigins") ?? [];
const mcpOutboundOAuthTrustedPrivateOrigins =
  config.getObject<string[]>("mcpOutboundOAuthTrustedPrivateOrigins") ?? [];
const wrappingKeyVersions = config.requireObject<string[]>(
  "mcpOAuthWrappingKeyVersions",
);
const activeWrappingKeyVersion = config.require(
  "mcpOAuthActiveWrappingKeyVersion",
);
const protectData = config.requireBoolean("protectData");

const authentikUrl = new URL(authentikBaseUrl);
if (
  authentikUrl.protocol !== "https:" ||
  authentikUrl.username !== "" ||
  authentikUrl.password !== "" ||
  authentikUrl.pathname !== "/" ||
  authentikUrl.search !== "" ||
  authentikUrl.hash !== ""
) {
  throw new Error(
    "authentikBaseUrl must be an HTTPS origin without credentials, path, query, or fragment",
  );
}

if (
  wrappingKeyVersions.length === 0 ||
  new Set(wrappingKeyVersions).size !== wrappingKeyVersions.length ||
  !wrappingKeyVersions.includes(activeWrappingKeyVersion) ||
  wrappingKeyVersions.some(
    (version) => !/^[a-z0-9][a-z0-9-]{0,62}$/.test(version),
  )
) {
  throw new Error(
    "mcpOAuthWrappingKeyVersions must be unique DNS labels and contain mcpOAuthActiveWrappingKeyVersion",
  );
}

const labels = {
  "app.kubernetes.io/name": "waltr",
  "app.kubernetes.io/instance": slug,
  "app.kubernetes.io/part-of": "waltr",
  "app.kubernetes.io/managed-by": "pulumi",
};
const workloadLabels = { ...labels, "app.kubernetes.io/component": "server" };
const deploymentEnvironment = pulumi.getStack();
const publicUrl = `https://${hostname}`;
const mcpIssuer = `${publicUrl}/oauth`;
const mcpResource = `${publicUrl}/mcp`;
const wrappingKeyMountPath = "/var/run/secrets/waltr/mcp-oauth";
const wrappingKeyFile = `${wrappingKeyMountPath}/keyring.json`;
const postgresTrustMountPath = "/var/run/secrets/waltr/postgres";
const postgresCaFile = `${postgresTrustMountPath}/ca.crt`;
const browserCallback = `https://${hostname}/oidc/callback`;
const nativeRedirectPattern = "^http://127\\.0\\.0\\.1:[0-9]+/callback$";

const namespace = new k8s.core.v1.Namespace("waltr-namespace", {
  metadata: { name: namespaceName, labels },
});

const bucketClaim = new k8s.apiextensions.CustomResource(
  "waltr-sessions-bucket",
  {
    apiVersion: "objectbucket.io/v1alpha1",
    kind: "ObjectBucketClaim",
    metadata: {
      name: "waltr-sessions",
      namespace: namespace.metadata.name,
      labels,
    },
    spec: {
      storageClassName: bucketStorageClass,
      generateBucketName: `${slug}-sessions`,
    },
  },
  { dependsOn: [namespace], protect: protectData },
);
const bucketConfig = pulumi
  .all([namespace.metadata.name, bucketClaim.id])
  .apply(([resolvedNamespace]) =>
    k8s.core.v1.ConfigMap.get(
      "waltr-sessions-generated-config",
      `${resolvedNamespace}/waltr-sessions`,
    ),
  );
const bucketSecret = pulumi
  .all([namespace.metadata.name, bucketClaim.id])
  .apply(([resolvedNamespace]) =>
    k8s.core.v1.Secret.get(
      "waltr-sessions-generated-secret",
      `${resolvedNamespace}/waltr-sessions`,
    ),
  );

const databaseCluster = new k8s.apiextensions.CustomResource(
  "waltr-postgres",
  {
    apiVersion: "postgresql.cnpg.io/v1",
    kind: "Cluster",
    metadata: {
      name: "waltr-postgres",
      namespace: namespace.metadata.name,
      labels,
    },
    spec: {
      instances: 1,
      enableSuperuserAccess: false,
      backup: {
        retentionPolicy: "30d",
        barmanObjectStore: {
          destinationPath: bucketConfig.data.apply(
            (values) => `s3://${values.BUCKET_NAME}/database`,
          ),
          endpointURL: s3Endpoint,
          s3Credentials: {
            accessKeyId: { name: "waltr-sessions", key: "AWS_ACCESS_KEY_ID" },
            secretAccessKey: {
              name: "waltr-sessions",
              key: "AWS_SECRET_ACCESS_KEY",
            },
          },
          wal: { compression: "gzip" },
          data: { compression: "gzip" },
        },
      },
      storage: { size: databaseStorageSize, storageClass },
      resources: {
        requests: { cpu: "250m", memory: "512Mi" },
        limits: { cpu: "2", memory: "2Gi" },
      },
    },
  },
  { dependsOn: [bucketClaim], protect: protectData },
);

new k8s.apiextensions.CustomResource(
  "waltr-postgres-backup",
  {
    apiVersion: "postgresql.cnpg.io/v1",
    kind: "ScheduledBackup",
    metadata: {
      name: "waltr-postgres",
      namespace: namespace.metadata.name,
      labels,
    },
    spec: {
      schedule: "0 0 2 * * *",
      backupOwnerReference: "self",
      cluster: { name: "waltr-postgres" },
      immediate: true,
      method: "barmanObjectStore",
    },
  },
  { dependsOn: [databaseCluster] },
);

const database = new k8s.apiextensions.CustomResource(
  "waltr-database",
  {
    apiVersion: "postgresql.cnpg.io/v1",
    kind: "Database",
    metadata: { name: "waltr", namespace: namespace.metadata.name, labels },
    spec: { name: "waltr", owner: "app", cluster: { name: "waltr-postgres" } },
  },
  { dependsOn: [databaseCluster] },
);

const data = new k8s.core.v1.PersistentVolumeClaim(
  "waltr-data",
  {
    metadata: { name: "waltr-data", namespace: namespace.metadata.name, labels },
    spec: {
      accessModes: ["ReadWriteOnce"],
      storageClassName: storageClass,
      resources: { requests: { storage: dataStorageSize } },
    },
  },
  { dependsOn: [namespace], protect: protectData },
);

const signingPrivateKey = new tls.PrivateKey("waltr-oidc-signing-key", {
  algorithm: "RSA",
  rsaBits: 4096,
});
const signingCertificate = new tls.SelfSignedCert(
  "waltr-oidc-signing-certificate",
  {
    privateKeyPem: signingPrivateKey.privateKeyPem,
    subject: { commonName: `${slug} OAuth signing` },
    validityPeriodHours: 87600,
    allowedUses: ["digital_signature", "key_encipherment"],
  },
);
const signingKey = new authentik.CertificateKeyPair(
  "waltr-oidc-signing-keypair",
  {
    name: `${displayName} OAuth signing key`,
    certificateData: signingCertificate.certPem,
    keyData: signingPrivateKey.privateKeyPem,
  },
);

const openid = authentik.getPropertyMappingProviderScopeOutput({
  managed: "goauthentik.io/providers/oauth2/scope-openid",
});
const profile = authentik.getPropertyMappingProviderScopeOutput({
  managed: "goauthentik.io/providers/oauth2/scope-profile",
});
const email = authentik.getPropertyMappingProviderScopeOutput({
  managed: "goauthentik.io/providers/oauth2/scope-email",
});
const offlineAccess = new authentik.PropertyMappingProviderScope(
  "waltr-offline-access",
  {
    name: `${displayName} offline access`,
    scopeName: "offline_access",
    expression: "return {}",
  },
);
const apiAudienceMapping = new authentik.PropertyMappingProviderScope(
  "waltr-api-audience",
  {
    name: `${displayName} API audience`,
    scopeName: apiScope,
    expression: `return {"aud": ${JSON.stringify(apiAudience)}}`,
  },
);
const identityMappings = [openid.id, profile.id, email.id];
const nativeMappings = [
  ...identityMappings,
  offlineAccess.propertyMappingProviderScopeId,
  apiAudienceMapping.propertyMappingProviderScopeId,
];

const browserApp = new OAuthApplication("waltr-browser", {
  displayName: `${displayName} Browser`,
  slug: `${slug}-browser`,
  issuerBaseUrl: authentikBaseUrl,
  clientType: "confidential",
  redirectUris: [{ matching_mode: "strict", url: browserCallback }],
  signingKeyId: signingKey.id,
  propertyMappings: identityMappings,
  launchUrl: `https://${hostname}`,
});
const nativeApp = new OAuthApplication("waltr-native", {
  displayName: `${displayName} Native API`,
  slug: `${slug}-native`,
  issuerBaseUrl: authentikBaseUrl,
  clientType: "public",
  redirectUris: [{ matching_mode: "regex", url: nativeRedirectPattern }],
  signingKeyId: signingKey.id,
  propertyMappings: nativeMappings,
});
const cnpgAppSecret = pulumi
  .all([namespace.metadata.name, databaseCluster.id])
  .apply(([resolvedNamespace]) =>
    k8s.core.v1.Secret.get(
      "waltr-postgres-generated-app",
      `${resolvedNamespace}/waltr-postgres-app`,
    ),
  );
const decodeSecret = (key: string) =>
  cnpgAppSecret.data.apply((values) =>
    Buffer.from(values[key], "base64").toString("utf8"),
  );
const decodeBucketSecret = (key: string) =>
  bucketSecret.data.apply((values) =>
    Buffer.from(values[key], "base64").toString("utf8"),
  );
const databaseUrl = pulumi
  .all([decodeSecret("username"), decodeSecret("password")])
  .apply(
    ([username, password]) =>
      `postgresql://${encodeURIComponent(username)}:${encodeURIComponent(password)}@waltr-postgres-rw.${namespaceName}.svc:5432/waltr?sslmode=verify-full&sslrootcert=${encodeURIComponent(postgresCaFile)}`,
  );

const wrappingKeys = wrappingKeyVersions.map((version) => ({
  version,
  key: new random.RandomBytes(`waltr-mcp-oauth-wrapping-key-${version}`, {
    length: 32,
  }).base64,
}));
const wrappingKeyring = pulumi
  .all(wrappingKeys.map(({ key }) => key))
  .apply((keys) =>
    JSON.stringify({
      schema_version: 1,
      active: activeWrappingKeyVersion,
      keys: wrappingKeyVersions.map((version, index) => ({
        id: version,
        key: Buffer.from(keys[index], "base64").toString("base64url"),
      })),
    }),
  );
const wrappingKeyChecksum = wrappingKeyring.apply((keyring) =>
  createHash("sha256").update(keyring).digest("hex"),
);
const wrappingKeySecret = new k8s.core.v1.Secret(
  "waltr-mcp-oauth-wrapping-keys",
  {
    metadata: {
      name: "waltr-mcp-oauth-wrapping-keys",
      namespace: namespace.metadata.name,
      labels,
      annotations: { "waltr.holdenitdown.net/keyring-format": "1" },
    },
    type: "Opaque",
    stringData: { "keyring.json": wrappingKeyring },
  },
);

const appSecret = new k8s.core.v1.Secret(
  "waltr-app",
  {
    metadata: { name: "waltr-app", namespace: namespace.metadata.name, labels },
    stringData: {
      WALTR_DATABASE_URL: databaseUrl,
      WALTR_MCP_DATABASE_URL: databaseUrl,
      WALTR_AUTH_MODE: "oidc",
      WALTR_PUBLIC_URL: publicUrl,
      WALTR_OIDC_ISSUER: browserApp.issuer,
      WALTR_OIDC_CLIENT_ID: browserApp.clientId,
      WALTR_OIDC_CLIENT_SECRET: browserApp.clientSecret!,
      WALTR_OIDC_REDIRECT_URI: browserCallback,
      WALTR_OIDC_SCOPES: "openid profile email",
      WALTR_API_OIDC_ISSUER: nativeApp.issuer,
      WALTR_API_OIDC_AUDIENCE: apiAudience,
      WALTR_API_OIDC_CLIENT_ID: nativeApp.clientId,
      WALTR_API_REQUIRED_SCOPES: apiScope,
      WALTR_MCP_REQUIRED_SCOPES: mcpScope,
      WALTR_MCP_OAUTH_ISSUER: mcpIssuer,
      WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE: wrappingKeyFile,
      WALTR_MCP_OAUTH_ACCESS_TOKEN_TTL: mcpOAuthAccessTokenTtl,
      WALTR_MCP_OAUTH_REFRESH_TOKEN_TTL: mcpOAuthRefreshTokenTtl,
      WALTR_MCP_OAUTH_REFRESH_FAMILY_TTL: mcpOAuthRefreshFamilyTtl,
      WALTR_MCP_OAUTH_CODE_TTL: mcpOAuthCodeTtl,
      WALTR_MCP_OAUTH_ALLOW_DCR: String(mcpOAuthAllowDcr),
      WALTR_MCP_OAUTH_ALLOW_LOOPBACK_REDIRECTS: String(
        mcpOAuthAllowLoopbackRedirects,
      ),
      WALTR_MCP_OAUTH_ALLOW_CIMD: String(mcpOAuthAllowCimd),
      ...(mcpOAuthCimdTrustedPrivateOrigins.length > 0
        ? {
            WALTR_MCP_OAUTH_CIMD_TRUSTED_PRIVATE_ORIGINS:
              mcpOAuthCimdTrustedPrivateOrigins.join(","),
          }
        : {}),
      ...(mcpOutboundOAuthTrustedPrivateOrigins.length > 0
        ? {
            WALTR_MCP_OUTBOUND_OAUTH_TRUSTED_PRIVATE_ORIGINS:
              mcpOutboundOAuthTrustedPrivateOrigins.join(","),
          }
        : {}),
      WALTR_OTEL_EXPORT: "true",
      OTEL_EXPORTER_OTLP_ENDPOINT: "https://telemetry.holdenitdown.net:4318",
      OTEL_EXPORTER_OTLP_PROTOCOL: "http/protobuf",
      OTEL_RESOURCE_ATTRIBUTES: `deployment.environment.name=${deploymentEnvironment}`,
      WALTR_PYROSCOPE_URL: "https://telemetry.holdenitdown.net:4040",
      WALTR_SESSION_STORE: "s3",
      WALTR_SESSION_S3_ENDPOINT: s3Endpoint,
      WALTR_SESSION_S3_REGION: "us-east-1",
      WALTR_SESSION_S3_BUCKET: bucketConfig.data.apply(
        (values) => values.BUCKET_NAME,
      ),
      WALTR_SESSION_S3_PREFIX: s3Prefix,
      WALTR_SESSION_S3_ACCESS_KEY_ID: decodeBucketSecret("AWS_ACCESS_KEY_ID"),
      WALTR_SESSION_S3_SECRET_ACCESS_KEY: decodeBucketSecret(
        "AWS_SECRET_ACCESS_KEY",
      ),
      WALTR_SESSION_S3_FORCE_PATH_STYLE: "true",
      WALTR_SESSION_S3_WRITE_MODE: "single-writer",
    },
  },
  { dependsOn: [database, data, browserApp, nativeApp] },
);

const deployment = new k8s.apps.v1.Deployment(
  "waltr",
  {
    metadata: {
      name: "waltr",
      namespace: namespace.metadata.name,
      labels: workloadLabels,
      annotations: {
        "secret.reloader.stakater.com/reload":
          "waltr-app,waltr-mcp-oauth-wrapping-keys",
      },
    },
    spec: {
      replicas: 1,
      strategy: { type: "Recreate" },
      selector: { matchLabels: workloadLabels },
      template: {
        metadata: {
          labels: workloadLabels,
          annotations: {
            "waltr.holdenitdown.net/wrapping-key-checksum": wrappingKeyChecksum,
          },
        },
        spec: {
          automountServiceAccountToken: false,
          securityContext: {
            runAsNonRoot: true,
            runAsUser: 65532,
            runAsGroup: 65532,
            fsGroup: 65532,
            fsGroupChangePolicy: "OnRootMismatch",
            seccompProfile: { type: "RuntimeDefault" },
          },
          terminationGracePeriodSeconds: 30,
          containers: [
            {
              name: "waltr",
              image,
              imagePullPolicy: "IfNotPresent",
              ports: [
                { name: "grpc-web", containerPort: 14333, protocol: "TCP" },
              ],
              envFrom: [{ secretRef: { name: appSecret.metadata.name } }],
              securityContext: {
                runAsNonRoot: true,
                allowPrivilegeEscalation: false,
                readOnlyRootFilesystem: true,
                capabilities: { drop: ["ALL"] },
              },
              resources: {
                requests: { cpu: "250m", memory: "512Mi" },
                limits: { cpu: "2", memory: "2Gi" },
              },
              startupProbe: {
                httpGet: { path: "/health", port: "grpc-web" },
                periodSeconds: 2,
                failureThreshold: 60,
              },
              readinessProbe: {
                httpGet: { path: "/ready", port: "grpc-web" },
                periodSeconds: 5,
                failureThreshold: 3,
              },
              livenessProbe: {
                httpGet: { path: "/health", port: "grpc-web" },
                periodSeconds: 10,
                failureThreshold: 3,
              },
              volumeMounts: [
                { name: "data", mountPath: "/data" },
                { name: "tmp", mountPath: "/tmp" },
                {
                  name: "mcp-oauth-wrapping-keys",
                  mountPath: wrappingKeyMountPath,
                  readOnly: true,
                },
                {
                  name: "postgres-ca",
                  mountPath: postgresTrustMountPath,
                  readOnly: true,
                },
              ],
            },
          ],
          volumes: [
            {
              name: "data",
              persistentVolumeClaim: { claimName: data.metadata.name },
            },
            { name: "tmp", emptyDir: { sizeLimit: "256Mi" } },
            {
              name: "mcp-oauth-wrapping-keys",
              secret: {
                secretName: wrappingKeySecret.metadata.name,
                defaultMode: 0o440,
                items: [
                  { key: "keyring.json", path: "keyring.json", mode: 0o440 },
                ],
              },
            },
            {
              name: "postgres-ca",
              secret: {
                secretName: "waltr-postgres-ca",
                defaultMode: 0o444,
                items: [{ key: "ca.crt", path: "ca.crt", mode: 0o444 }],
              },
            },
          ],
        },
      },
    },
  },
  { dependsOn: [appSecret, wrappingKeySecret] },
);

new k8s.networking.v1.NetworkPolicy("waltr-egress", {
  metadata: { name: "waltr-egress", namespace: namespace.metadata.name, labels },
  spec: {
    podSelector: { matchLabels: workloadLabels },
    policyTypes: ["Egress"],
    egress: [
      {
        to: [
          {
            namespaceSelector: {
              matchLabels: { "kubernetes.io/metadata.name": "kube-system" },
            },
          },
        ],
        ports: [
          { port: 53, protocol: "UDP" },
          { port: 53, protocol: "TCP" },
        ],
      },
      {
        ports: [
          { port: 443, protocol: "TCP" },
          { port: 4040, protocol: "TCP" },
          { port: 4318, protocol: "TCP" },
        ],
      },
      {
        to: [
          {
            podSelector: {
              matchLabels: { "cnpg.io/cluster": "waltr-postgres" },
            },
          },
        ],
        ports: [{ port: 5432, protocol: "TCP" }],
      },
    ],
  },
});

const service = new k8s.core.v1.Service("waltr", {
  metadata: { name: "waltr", namespace: namespace.metadata.name, labels },
  spec: {
    selector: workloadLabels,
    ports: [
      {
        name: "grpc-web",
        port: 14333,
        targetPort: "grpc-web",
        appProtocol: "kubernetes.io/h2c",
      },
    ],
  },
});

const route = new k8s.apiextensions.CustomResource("waltr-route", {
  apiVersion: "gateway.networking.k8s.io/v1",
  kind: "HTTPRoute",
  metadata: { name: "waltr", namespace: namespace.metadata.name, labels },
  spec: {
    parentRefs: [
      {
        group: "gateway.networking.k8s.io",
        kind: "Gateway",
        name: "default-gateway",
        namespace: "ingress",
      },
    ],
    hostnames: [hostname],
    rules: [
      {
        matches: [{ path: { type: "PathPrefix", value: "/" } }],
        backendRefs: [{ name: service.metadata.name, port: 14333 }],
        timeouts: { request: "0s" },
      },
    ],
  },
});

export const mcpOAuthIssuer = mcpIssuer;
export const mcpOAuthResource = mcpResource;
export const mcpOAuthMetadataUrl = `${publicUrl}/.well-known/oauth-authorization-server/oauth`;
export const mcpOAuthJwksUrl = `${mcpIssuer}/jwks.json`;
