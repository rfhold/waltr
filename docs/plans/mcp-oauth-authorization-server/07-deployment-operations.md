# 07 Deployment And Operations

Status: In Progress

The Pulumi topology, Authentik cutover, wrapping-key deployment, readiness checks, network policy, backup resources, pipeline definitions, and recovery runbook are implemented and covered by deterministic shape/policy checks. No live Pulumi or Tekton deployment was run. Exit remains pending for live multi-replica rotation and revocation, database failover, backup restoration, and rollback exercises; no production deployment or successful restoration is claimed.

## Objective

Deploy the Waltr-hosted MCP authorization server safely through Pulumi and Pipelines as Code, with Authentik limited to user authentication, durable wrapping-key custody, coordinated signing-key rotation, complete backup coverage, and observable multi-replica behavior.

## Authentik Topology

The hosted server reuses the existing Waltr browser session. Pulumi therefore retains:

- Confidential browser application for Waltr web login.
- Public native API application for TUI login.

Remove the public MCP Authentik application and its direct-token policy:

- MCP Authentik client ID and redirect URI.
- MCP audience mapping.
- MCP scope property mapping.
- MCP refresh/offline-access configuration.
- `WALTR_MCP_OIDC_ISSUER` and `WALTR_MCP_OIDC_AUDIENCE`.
- `WALTR_MCP_SELF_OAUTH_CLIENT_ID` and `WALTR_MCP_SELF_OAUTH_REDIRECT_URI`.
- `mcpManualCallback` stack configuration.

`WALTR_MCP_REQUIRED_SCOPES` remains, but becomes local Waltr authorization-server policy rather than an Authentik token claim requirement.

## Public Routing

The existing Waltr HTTPRoute must expose without path rewriting:

- `/mcp`
- `/.well-known/oauth-protected-resource/mcp`
- `/.well-known/oauth-authorization-server/oauth`
- `/oauth/*`
- `/outbound-oauth/callback`
- `/.well-known/oauth-client/waltr`
- Existing `/oidc/*`, API metadata, web, and gRPC routes

Proxy configuration must preserve the externally visible scheme and authority used by `WALTR_PUBLIC_URL`. Authorization, client, redirect, issuer, resource, and metadata URLs must all be generated from that canonical value, never inferred from untrusted forwarding headers.

## Wrapping-Key Secret

Pulumi provisions one versioned wrapping-key ring per stack as a Kubernetes Secret and mounts it read-only into the Waltr pod. It must not place wrapping keys in:

- Pulumi plaintext configuration.
- ConfigMaps.
- Deployment annotations.
- Container arguments.
- Image build arguments or layers.
- Stack outputs.

Use a documented file format with:

- Schema version.
- Active wrapping-key ID.
- Map of key IDs to 256-bit random keys.

The file parser rejects unknown schema versions, duplicate IDs, invalid lengths, empty keyrings, and an active ID absent from the map. The mounted file is readable by runtime UID/GID `65532` and the root filesystem remains read-only.

Pulumi secret values remain secret through interpolation. Add mock/policy tests that reject accidental plaintext projection.

## Signing-Key Rotation

The `mcp` crate generates and rotates ES256 signing keys; Pulumi supplies only wrapping-key material and rotation policy.

Default lifecycle:

1. Ensure a pending signing key exists before the active key reaches rotation age.
2. Publish its public JWK while pending if the implementation uses a prepublication window.
3. Atomically activate one key through SQL.
4. Sign new access tokens only with the active key.
5. Retain old public keys through maximum access-token lifetime, clock skew, and rollback allowance.
6. Remove retired private material before eventually deleting the public JWK.

Rotation is application-managed and safe with multiple replicas. A failed candidate generation does not retire the current signer.

## Wrapping-Key Rotation

Wrapping-key rotation is a separate operation:

1. Add a new key to the mounted keyring while retaining all old keys.
2. Mark the new key active and roll out pods.
3. Let the MCP maintenance task re-encrypt every signing private key under the new wrapping-key ID.
4. Verify no database row references the old wrapping key.
5. Remove the old key in a later deployment.

Never combine adding and removing a wrapping key in one rollout. Losing every wrapping key referenced by SQL makes signing state unrecoverable.

Add a canonical runbook under `docs/runbooks/` covering normal rotation, interrupted rotation, rollback, and disaster recovery.

## Database And Backups

The existing MCP persistence target now contains both transport sessions and OAuth security state. Backup and restore must include:

- Clients and redirect URIs.
- Cached CIMD records.
- Consent grants.
- Refresh families and token digests.
- Access-token revocation state.
- Encrypted signing keys and public JWKs.

The wrapping-key Secret must be backed up separately through the cluster secret-management process. A database backup without its referenced wrapping keys cannot restore token issuance.

CloudNativePG backups must be tested with the MCP schema and the corresponding wrapping-key version. SQLite development documentation must identify `mcp.sqlite3` and the keyring as one recovery unit.

## Network Policy

CIMD and outbound OAuth discovery require controlled egress. Deployment policy should permit HTTPS and DNS while the application-level destination policy blocks private and metadata ranges.

Do not grant broad in-cluster credentials or Kubernetes API access to the application as a substitute for registration. The Waltr pod requires no permission to mutate Authentik after deployment.

## Readiness And Availability

Readiness requires:

- Completed MCP migrations.
- Usable OAuth store.
- Decryptable active signing key.
- Valid local issuer/resource configuration.
- Initialized Authentik browser access authentication.
- Healthy same-process revocation notification and a bounded authoritative durable-store recheck interval. PostgreSQL `LISTEN/NOTIFY` is not implemented; cross-replica termination relies on polling at most every five seconds plus scheduling and database latency.

After startup, an Authentik outage blocks new login but must not prevent validation, refresh, or revocation of Waltr-issued credentials. Database failure fails closed for protected MCP access.

## Observability

Add bounded-cardinality metrics for:

- Authorization starts, approvals, denials, expiry, and failures.
- DCR/CIMD/preregistered client resolution outcomes.
- Code redemption and replay.
- Token issue and refresh outcomes.
- Refresh-family replay revocation.
- Token, grant, client, and family revocation.
- Active authorization leases and termination reason.
- Signing-key generation, activation, retirement, and decrypt failure.
- Wrapping-key re-encryption progress.
- Cleanup batch results and revocation listener health.

Do not label metrics by subject, client ID, token ID, issuer URL, redirect URI, or arbitrary hostname. Traces may record sanitized operation and outcome but never credentials or complete authorization queries.

## Pulumi Changes

Update `infra/pulumi/` to:

- Remove the Authentik MCP application and direct issuer wiring.
- Generate and persist the wrapping-key Secret.
- Mount the keyring read-only.
- Configure local issuer, scopes, lifetimes, and registration policy.
- Add a checksum derived without exposing secret material so intended keyring updates restart pods.
- Preserve nonroot, read-only-root, dropped-capability, PVC, S3, database, Service, and HTTPRoute configuration.
- Export only nonsecret local issuer, resource, metadata, and JWKS URLs.

Add Pulumi tests asserting resource shape and secret propagation. Avoid snapshotting secret values.

## Pipeline Changes

The committed `.tekton/waltr-preview.yaml` and `.tekton/waltr-release.yaml` definitions verify before deployment:

- Rust formatting, the SQLite OAuth store target, Waltr server integration tests, and shared-client integration tests.
- Web component tests.
- Pulumi TypeScript build and mock/policy tests.
- A private-material scan over Pulumi, Dockerfile, and pipeline inputs.
- Multi-architecture runtime image build, manifest publication, digest verification, and revision-label verification.

Pulumi preview and update run only after immutable image publication and verification. Compose contracts, browser integration, PostgreSQL conformance, broader aggregate gates, and live deployment exercises are separate from these Tekton tasks; do not claim the pipeline executed them.

## Rollout

Because no direct-Authentik MCP consumers or durable credentials require migration, use a clean cutover:

1. Deploy migrations, wrapping-key Secret, and hosted server to preview.
2. Verify DCR, CIMD, authorization, refresh, revocation, stream termination, restart, and signing rotation.
3. Verify Waltr outbound authorization against preview without `self` settings.
4. Deploy production with the local issuer from the first supported MCP release.
5. Remove the obsolete Authentik MCP application in the same Pulumi program after preview proves replacement safety.

Do not add a direct-Authentik fallback validator or advertise multiple authorization servers for migration.

## Operational Tests

Status: Not run against a live multi-replica deployment. The following remain exit work rather than passed checks:

- Two replicas rotate one signing key without split-brain activation.
- Token revocation reaches a stream connected to another replica.
- Pod restart during authorization, code exchange, and refresh preserves atomic outcomes.
- Active database failover fails requests closed and recovers without replay.
- Authentik outage permits existing token validation and refresh but blocks new authentication.
- Wrapping-key addition and re-encryption survive rolling restart.
- Old wrapping-key removal is rejected while references remain.
- Backup restoration recovers signing, grants, refresh, and revocation state.
- Rollback retains public keys required by unexpired tokens.

## Exit Criteria

- Authentik has no downstream MCP audience, scope, client, or token-issuance role.
- Wrapping keys are secret, durable, rotatable, and absent from images and outputs.
- Signing rotation and revocation work across replicas.
- Production readiness fails closed on unusable OAuth persistence or key state.
- Preview and production are fully Pulumi-managed with no manual Kubernetes resources.
- Operational rotation and recovery procedures are documented and exercised.

## Dependencies

Depends on [Waltr Server Integration](05-waltr-server-integration.md) and [Outbound MCP Client](06-outbound-client.md).
