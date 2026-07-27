# 02 OAuth Persistence And Signing Keys

Status: Complete

## Objective

Add a generic, atomic OAuth authorization store and durable encrypted ES256 signing-key lifecycle to `crates/mcp`, with equivalent memory, SQLite, and PostgreSQL behavior.

## Boundaries

OAuth authorization state is distinct from Streamable HTTP transport-session state. Do not extend `StreamableHttpSessionStore` with OAuth operations. Both stores may share an MCP-owned backend, migration history, and pool factory.

The store accepts opaque `McpPrincipalId` values from an integrating resource-owner authenticator. It must not persist Authentik tokens, Waltr cookies, or Waltr-specific identity records.

## Store Contract

Add an object-safe `OAuthAuthorizationStore` using the crate's boxed-future convention.

The contract must expose high-level atomic transitions rather than handler-side read/modify/write sequences:

| Transition | Required behavior |
| --- | --- |
| Register client | Enforce uniqueness, redirect limits, client quotas, and public-client policy. |
| Cache CIMD | Store a validated metadata snapshot, digest, retrieval time, and expiry. |
| Create authorization transaction | Bind client, redirect, resource, scopes, state reference, PKCE challenge, and expiry. |
| Authenticate transaction | Attach one stable principal exactly once without consuming consent state. |
| Decide transaction | Record approval or denial and prevent duplicate decisions. |
| Issue authorization code | Create a digest-only, short-lived code bound to the approved transaction. |
| Redeem code and issue tokens | Atomically consume one code and create the grant, access-token record, refresh family, and first refresh generation from prepared signed-token metadata. A failed commit returns no token. |
| Rotate refresh token | Consume one generation and issue the next access/refresh pair atomically. |
| Detect refresh replay | Revoke the family and related access tokens in the same transaction. |
| Revoke | Revoke a token, family, grant, or client idempotently. |
| Validate access token | Check `jti`, client, grant, principal, resource, scopes, expiry, and revocation. |
| Manage signing keys | Generate candidates, activate one signer, retire old keys, publish verification keys, and clean expired keys. |
| Cleanup | Delete expired transient and retained security state in bounded batches. |

## SQL Layout

Add forward-only migrations:

```text
crates/mcp/migrations/sqlite/0002_oauth_authorization_server.sql
crates/mcp/migrations/postgresql/0002_oauth_authorization_server.sql
```

Logical tables:

| Table | Required contents |
| --- | --- |
| `oauth_clients` | Client ID, registration method, display metadata, creation, expiry, disablement, and limits. |
| `oauth_client_redirect_uris` | Exact redirect URIs with a client foreign key. |
| `oauth_client_metadata_cache` | Versioned CIMD snapshot ID, URL, normalized document, document digest, fetch time, expiry, and retention deadline. |
| `oauth_authorization_transactions` | Client or CIMD snapshot, redirect, resource, scopes, PKCE challenge, browser-correlation digest, state reference, principal, decision, and expiry. |
| `oauth_consents` | Principal, client, resource, exact scopes, creation, update, and revocation. |
| `oauth_authorization_codes` | Code digest, transaction/grant input, creation, expiry, and consumption. |
| `oauth_grants` | Principal, client, resource, granted scopes, and revocation. |
| `oauth_refresh_families` | Grant binding, creation, absolute expiry, and family revocation reason. |
| `oauth_refresh_tokens` | Token digest, family, generation, issuance, expiry, consumption, and replacement generation. |
| `oauth_access_tokens` | `jti`, signing key, refresh-family ID, grant, client, principal, resource, scopes, expiry, and revocation. |
| `oauth_signing_keys` | Key ID, algorithm, public JWK, encrypted private material, wrapping-key ID, state, and lifecycle timestamps. |

Use foreign keys and indexes for all lookup and cleanup paths. Codes and opaque tokens are never stored plaintext. Scope sets use one canonical representation so equality and subset checks do not depend on input ordering.

## Shared MCP Persistence

Both SQL stores must run the same complete MCP migrator. Do not create a second `_sqlx_migrations` history in the same SQLite file or PostgreSQL schema.

Introduce an internal or public backend factory if needed:

```text
SqliteMcpPersistence
PostgresMcpPersistence
```

The factory may expose both session and OAuth store handles while preserving the current `SqliteStreamableHttpSessionStore` and `PostgresStreamableHttpSessionStore` constructors as compatibility wrappers.

## Concurrency

SQLite uses write transactions that serialize security-sensitive transitions. PostgreSQL uses row locks or advisory locks for:

- Client admission and quota enforcement.
- Authorization-code redemption.
- Refresh rotation and replay handling.
- Signing-key activation.
- Bounded cleanup coordination.

No network request or cryptographic key generation occurs while holding a database transaction open. Generate candidate values first, then commit their digests or encrypted representations atomically. Authorization-code exchange prepares and signs a candidate access token before calling one conditional store transition that consumes the code and persists every corresponding grant/token record. A racing or failed transition discards the candidate token and never returns it.

## Signing-Key Custody

Use ES256. Generate private keys inside the MCP authorization subsystem and encode them as PKCS#8 before encryption.

The integrating deployment supplies a versioned wrapping-key ring through a secret file. The generic crate receives parsed secret key material through a small encryption interface; it must not read Waltr environment variables directly.

Each encrypted private key stores:

- Wrapping-key ID.
- Unique authenticated-encryption nonce.
- Ciphertext.
- Algorithm and key ID as authenticated associated data.
- Public JWK separately for publication.

Use an authenticated encryption construction from a maintained Rust crypto crate. Secret buffers should use zeroization where supported. Custom `Debug` implementations must omit private material, nonces, ciphertext, and wrapping keys.

## Key Lifecycle

Model explicit states:

```text
pending -> active -> retired -> deleted
```

Rules:

- On an empty store, generate a bootstrap candidate outside the transaction, acquire the issuer rotation lock, and atomically insert and activate it only if the issuer still has no key. Concurrent losers discard their candidates and load the winner.
- Exactly one active signing key exists per issuer.
- Rotation activation is coordinated through SQL across replicas.
- New tokens use only the active key.
- JWKS includes active and retired public keys needed by unexpired tokens.
- Retired keys remain verifiable through maximum token expiry plus clock skew.
- Encrypted private material may be retained through the rollback window, then removed before public JWK deletion.
- A new wrapping key re-encrypts stored private keys without changing JWT `kid` values.
- Failure to decrypt the active key fails readiness.
- Unknown `kid` never falls back to another key.

## Store Implementations

Implement in this order:

1. In-memory store with deterministic clock and entropy for state-machine tests.
2. SQLite store and migrations.
3. PostgreSQL store and migrations.
4. Shared conformance suite executed against every backend.

The in-memory implementation is a real behavioral reference, not a reduced test double.

## Tests

Add store-contract targets modeled after the existing Streamable HTTP store suite:

```text
crates/mcp/tests/oauth_store.rs
crates/mcp/tests/oauth_store_support/
crates/mcp/tests/oauth_sqlite_store.rs
crates/mcp/tests/oauth_postgres_store.rs
```

Cover:

- Code redemption exactly once under concurrency.
- Refresh rotation exactly once under concurrency.
- Replay revoking the complete family.
- Client, grant, family, and token revocation.
- Exact principal, client, resource, redirect, and scope isolation.
- Restart and reopen persistence.
- Admission limits and bounded cleanup.
- Migration idempotence and checksum enforcement.
- One active signing key across concurrent replicas.
- First-start signing-key bootstrap across concurrent replicas.
- Active/retired JWKS overlap.
- Wrapping-key re-encryption.
- Corrupt ciphertext and unknown wrapping-key failure.
- No plaintext code, refresh token, or private key in SQL fixtures, errors, or debug output.

## Exit Criteria

- Memory, SQLite, and PostgreSQL pass one store contract.
- Every security-sensitive state transition is atomic.
- Existing MCP session-store migrations and tests remain valid.
- A restarted process can decrypt the active signer and validate persisted token state.
- Key rotation across two PostgreSQL-backed instances produces one active signer.
- No Waltr type or Authentik concept enters `crates/mcp`.

## Dependencies

Depends on [OAuth Protocol And Discovery](01-protocol-discovery.md). Enables [Authorization, Tokens, And Revocation](03-authorization-tokens.md).
