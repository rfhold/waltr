# MCP OAuth Key Rotation And Recovery

Use this runbook for the Pulumi-managed Waltr MCP wrapping-key ring. Signing keys live encrypted in the MCP database; wrapping keys live separately in the Kubernetes Secret generated from Pulumi state. Back up and restore both.

This procedure is documented but has not yet been exercised as a live multi-replica rotation, failover, rollback, or backup restoration. Do not treat it as evidence of a successful recovery or production deployment.

## Preconditions

- Confirm the current stack and Kubernetes context.
- Take and verify a CloudNativePG backup.
- Export the `waltr-mcp-oauth-wrapping-keys` Secret through the approved encrypted cluster-secret backup process. Do not write decoded key material to the repository, shell history, CI logs, or Pulumi outputs.
- Record the active wrapping-key version and all retained versions by identifier only.

## Normal Rotation

1. Add a new DNS-label version to `waltr:mcpOAuthWrappingKeyVersions` while retaining every existing version.
2. Set `waltr:mcpOAuthActiveWrappingKeyVersion` to the new version in the same rollout.
3. Run `pulumi preview` and verify that Pulumi creates one random key, updates the Secret, and rolls the Deployment without deleting old random-key resources.
4. Run `pulumi up`. Wait for rollout completion and readiness.
5. Verify token issuance, refresh, JWKS publication, and signing-key maintenance.
6. Verify through the application maintenance status or database-safe operational query that no encrypted signing row references the old wrapping-key ID.
7. In a later change, remove only unreferenced old versions from `waltr:mcpOAuthWrappingKeyVersions`, preview, deploy, and take a new paired database/Secret backup.

Never add and remove wrapping-key versions in one rollout. Never remove a version while database rows reference it.

## Interrupted Rotation

If rollout or re-encryption fails, retain the complete old and new ring. Roll back the workload image or active version only; do not remove either key. Restore service, confirm the active signing key is decryptable, then resume re-encryption. Candidate-generation failure must not retire the current signer.

## Rollback

Roll back with a keyring containing every version present before and during the failed release. The database may contain rows encrypted under either version. Keep retired public signing keys for the maximum access-token lifetime, clock skew, and rollback allowance.

## Disaster Recovery

1. Restore the CloudNativePG backup containing the MCP OAuth schema and transport sessions.
2. Restore the separately backed-up wrapping-key Secret version that contains every key ID referenced by encrypted signing rows.
3. Apply the Pulumi workload without replacing restored key material. If Pulumi state was also lost, import or securely re-establish the exact keyring before starting Waltr; newly generated values cannot decrypt the restored database.
4. Start one replica and verify migrations, OAuth-store access, active-key decryption, issuer/resource configuration, and JWKS publication before allowing traffic.
5. Verify existing token validation, refresh, revocation, and new issuance. Restore the configured replica count. The repository currently configures one replica; exercise cross-replica revocation separately before increasing it, and verify closure within the five-second authoritative polling bound plus scheduling and query latency.

For SQLite development, recover `mcp.sqlite3` and its keyring file together. `waltr.sqlite3` contains browser access-auth state and should be backed up as part of the same deployment recovery point, but it cannot replace the MCP database or keyring.
