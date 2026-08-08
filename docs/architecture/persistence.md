# Server Persistence

## Purpose

Waltr server persistence initializes separately owned Waltr and MCP database targets for server-owned durable state, outbound provider and MCP OAuth credentials, and the hosted MCP authorization server. Startup completes both standard SQLx migrators before the router, health endpoint, or gRPC readiness service becomes available.

This database is separate from agent session storage selected by `WALTR_SESSION_STORE`.

## Configuration

`WALTR_DATABASE_URL` selects the Waltr target. Optional `WALTR_MCP_DATABASE_URL` overrides the MCP target. Each accepts:

- `sqlite:` URLs use SQLite.
- `postgres://` and `postgresql://` URLs use PostgreSQL.
- An explicit PostgreSQL `sslmode` is preserved. PostgreSQL defaults to `sslmode=require` when the URL omits it.

When `WALTR_DATABASE_URL` is absent, Waltr uses `waltr/waltr.sqlite3` under `XDG_DATA_HOME`, or `$HOME/.local/share/waltr/waltr.sqlite3` when `XDG_DATA_HOME` is absent. The default MCP target is the sibling `mcp.sqlite3`. Startup fails if neither location can be resolved.

Without `WALTR_MCP_DATABASE_URL`, or when it is empty, a file-backed SQLite Waltr target derives `mcp.sqlite3` in the same directory while preserving safe parsed SQLite options. A PostgreSQL target derives the same logical database, opened through a separate pool. An explicit MCP URL can select another supported target. Configuration rejects physical aliases for the same file-backed SQLite target and equal named shared in-memory targets; separate private `sqlite::memory:` targets remain valid.

Tests inject independent in-memory databases or temporary file-backed SQLite databases instead of reading the process environment. The static Compose `dev` profile keeps `/data/waltr.sqlite3` and `/data/mcp.sqlite3` on the local `waltr-dev-data` volume. The `test` and `test-auth` Waltr services mount `/data` as tmpfs so integration runs cannot retain or reuse persistence.

SQLite settings must use a direct `sqlite:` path. Nested `file:` URI filenames are rejected for both Waltr and MCP targets because they can obscure target identity and collision checks.

## Ownership

Waltr and MCP use separate pools and retain separate schema and migration ownership.

| Backend | Waltr objects | MCP objects |
| --- | --- | --- |
| SQLite | `waltr.sqlite3`: `_sqlx_migrations`, OIDC state, and durable outbound OAuth attempts/credentials | `mcp.sqlite3`: `_sqlx_migrations`, hosted OAuth tables, and inert historical Streamable HTTP session/event tables |
| PostgreSQL | `waltr` schema and `waltr._sqlx_migrations`, including outbound OAuth | `mcp` schema and `mcp._sqlx_migrations`, including hosted OAuth |

Each owning crate embeds its forward-only migration files with `sqlx::migrate!()`. SQLx owns the standard `_sqlx_migrations` history format, version/checksum validation, and migration coordination; Waltr adds no migration-history layer. PostgreSQL migration connections set the owning `waltr` or `mcp` search path so the standard history and owned objects are created in that schema. Runtime queries remain owner-qualified where required.

Waltr migrations are immutable and normalized identically across SQLite and PostgreSQL:

| Version | Owner | Purpose |
| --- | --- | --- |
| `0001_schema_ownership.sql` | Waltr | Establish Waltr schema ownership. |
| `0002_oidc_access_auth.sql` | Waltr | Persist OIDC login attempts and browser sessions. |
| `0003_mcp_oauth_login_continuations.sql` | Waltr | Persist browser-bound hosted-MCP authorization continuations. |
| `0004_outbound_oauth.sql` | Waltr | Persist outbound OAuth attempts and the original MCP credential/refresh contract. |
| `0005_unified_oauth_credentials.sql` | Waltr | Hard-cut over provider and MCP credentials to one typed payload and refresh contract. |

The V2 access-authentication domain contains:

| Data | Durable contract |
| --- | --- |
| Login attempts | State and browser-correlation digests, nonce, PKCE verifier, local return path, creation/expiry, and one-time consumption. |
| Browser sessions | Opaque token digest, issuer, subject, creation/expiry/last-seen, and revocation. |

Admission is database-coordinated across server instances. The store permits at most 1,024 active login attempts, 1,024 active browser sessions globally, and 32 active browser sessions per issuer/subject. SQLite transactions and PostgreSQL advisory transaction locks make capacity checks and insertion atomic. Cleanup is bounded and opportunistic; expired or revoked records never authenticate even before cleanup.

Waltr migration V3 owns the hosted server's browser-correlation continuation records. V4 introduced outbound OAuth state. `outbound_oauth_attempts` persists state-bound callback attempts, registration/completion mode, expiry, sanitized lifecycle outcome, and an expiring identity-bound completion claim. Expired attempt-completion claims are reclaimable after a process crash, and stale claim identities cannot finish or commit.

V5 is a hard cutover for `outbound_oauth_credentials`: applying it removes existing credential rows, so every previously authorized provider and MCP target must authorize again. The recreated table stores a versioned Waltr-owned credential payload containing a shared OAuth token set and typed provider or MCP context. Access and refresh token values use `oauth2` secret-bearing primitives, while Waltr owns absolute expiry, context, and serialization. Credential revision, claim identity/deadline, reauthorization state, and update time remain envelope columns. Both provider and MCP refreshes use the same database lease and compare-and-swap contract. Callback completion and credential installation commit together for durable MCP callback attempts; provider authorization attempts remain process-local. An indeterminate refresh or expired refresh lease marks the credential for reauthorization rather than risking reuse.

MCP migrations are likewise immutable. V1 created Streamable HTTP session/event tables for a removed protocol runtime; those tables remain inert so existing SQLx history and checksums stay valid. V2 adds the hosted authorization server's transactions, consents, digest-only codes and refresh tokens, grants, refresh families, access-token authorization records, and ES256 signing keys. V3 adds normalized client-registration, redirect-URI, CIMD snapshot, admission, and rate-limit state. Private signing material is encrypted with the deployment keyring; wrapping keys are never stored in SQL. SQLite transactions and PostgreSQL locking implement the same one-time redemption, refresh rotation/replay, admission, revocation, and signing-key activation contract.

Never edit an applied migration. Schema corrections require a new forward-only migration in both backend directories; SQLx rejects changed checksums on reopen.

The MCP database and every wrapping-key version still referenced by an encrypted signing row are one backup and recovery unit. Cleanup runs every five minutes in batches of 256 and is advisory. Signing maintenance runs every 24 hours, atomically activates one candidate while retaining the prior public key for verification overlap, and rewraps retained private material with the active wrapping key.

## Startup Flow

1. Parse `WALTR_DATABASE_URL` and optional `WALTR_MCP_DATABASE_URL`, or resolve and derive the durable defaults.
2. Open the Waltr SQLx pool and run Waltr's standard embedded migrator.
3. Open the independent MCP SQLx pool and construct the hosted OAuth stores, which run MCP's standard embedded migrator.
4. Validate access-auth and hosted-OAuth environment configuration and, in OIDC mode, perform upstream discovery, load the wrapping-key ring, and decrypt or bootstrap the active local signer.
5. Build the router with initialized persistence, authentication, and hosted authorization.
6. Bind the listener and expose health and readiness.

Any failure before router construction prevents the server from binding or reporting readiness. Errors identify the failing subsystem without including credentials or database URLs.

The environment-owned `build_app()` is therefore async and fallible. Explicit builders are synchronous composition APIs: they require initialized `Persistence`, and authenticated variants require an initialized `AccessAuthenticator`. All builders return a server-owned application lifecycle whose hosted-OAuth maintenance is cancelled and joined with server shutdown, independent of any external `Persistence` clones. There is no MCP protocol-session store or cleanup loop.

## Boundaries

- `crates/waltr-server/src/persistence/` owns database selection, Waltr migrations, and startup composition.
- `crates/mcp/src/oauth/server/` owns generic hosted-OAuth state transitions and runtime persistence; Waltr selects its MCP SQL pool and deployment policy.
- `crates/waltr-server/src/session/` and `waltr-agent` continue to own agent session storage. `WALTR_SESSION_STORE` is not an alias for `WALTR_DATABASE_URL`.
- Domain features should add purpose-specific stores and migrations rather than a generic key/value table.
- `crates/waltr-server/src/access_auth/store.rs` owns the OIDC V2 domain store and admission rules; handlers do not issue SQL directly.
- `crates/waltr-server/src/persistence/outbound_oauth.rs` owns durable callbacks, unified outbound credentials, and refresh claims.

## Tests

Persistence tests cover SQLite and container-backed PostgreSQL migration startup, history isolation, SQLx checksum rejection, independent pools, SQLite target collision rejection, foreign-key behavior, and hosted-OAuth store contracts. Historical V1 rows are migration history only; no test reconstructs protocol sessions. See [Testing](../testing.md) for the server test commands.

## References

- [`crates/waltr-server/src/persistence/`](../../crates/waltr-server/src/persistence)
- [`crates/waltr-server/src/app.rs`](../../crates/waltr-server/src/app.rs)
- [MCP protocol](../mcp/protocol.md)
- [Access authentication](access-authentication.md)
- [Deployment](../deployment.md)
