# Server Persistence

## Purpose

Waltr server persistence initializes separately owned Waltr and MCP database targets for server-owned durable state, outbound provider and MCP OAuth credentials, and the hosted MCP authorization server. Startup completes both standard SQLx migrators before the router, health endpoint, or gRPC readiness service becomes available.

This database is separate from agent session storage selected by `WALTR_SESSION_STORE`.

Agent session JSON retains a public complete-record `revision` and a separate durable `execution_sequence`. Legacy JSON with an incarnation seeds an absent sequence from the stored public revision; reads never rewrite retained records. The next authorized write persists the field. Records without a durable incarnation remain readable/listable, but execution-token creation fails with `ExecutionCompatibility`. No SQL migration, historical migration edit, store scan or automatic reset accompanies this change. Each session store requires one coordinated writer version; mixed old/new complete-record writers are unsupported. See [session storage compatibility](../agent/storage.md#stored-record-compatibility).

Embedded Tauri composition does not read these environment defaults. The host supplies explicit absolute app-private paths for every durable store.

## Configuration

`WALTR_DATABASE_URL` selects the Waltr target. Optional `WALTR_MCP_DATABASE_URL` overrides the MCP target. Each accepts:

- `sqlite:` URLs use SQLite.
- `postgres://` and `postgresql://` URLs use PostgreSQL.
- An explicit PostgreSQL `sslmode` is preserved. PostgreSQL defaults to `sslmode=require` when the URL omits it.

When `WALTR_DATABASE_URL` is absent, Waltr uses `waltr/waltr.sqlite3` under `XDG_DATA_HOME`, or `$HOME/.local/share/waltr/waltr.sqlite3` when `XDG_DATA_HOME` is absent. The default MCP target is the sibling `mcp.sqlite3`. Startup fails if neither location can be resolved.

Without `WALTR_MCP_DATABASE_URL`, or when it is empty, a file-backed SQLite Waltr target derives `mcp.sqlite3` in the same directory while preserving safe parsed SQLite options. A PostgreSQL target derives the same logical database, opened through a separate pool. An explicit MCP URL can select another supported target. Configuration rejects physical aliases for the same file-backed SQLite target and equal named shared in-memory targets; separate private `sqlite::memory:` targets remain valid.

Tests inject independent in-memory databases or temporary file-backed SQLite databases instead of reading the process environment. The static Compose `dev` profile keeps `/data/waltr.sqlite3` and `/data/mcp.sqlite3` on the local `waltr-dev-data` volume. The `test` and `test-auth` Waltr services mount `/data` as tmpfs so integration runs cannot retain or reuse persistence.

## Embedded Tauri Layout

`crates/waltr-app/src/server_controller.rs` derives this layout from Tauri's app data and app config directories:

| State | Relative path |
| --- | --- |
| Waltr SQL | App data `server/waltr.sqlite3` |
| MCP SQL | App data `server/mcp.sqlite3` |
| Filesystem sessions | App data `server/sessions/` |
| Routine payload objects | App data `server/routine-objects/` |
| Agent workspace | App data `workspace/` |
| models.dev cache | App data `server/models/api.json` |
| Resource overrides | App config `server/.waltr/resources/` |

The embedded builder requires absolute paths. It never derives these stores from the current directory, `HOME`, XDG variables, or server persistence variables.

The app uses separate Waltr and MCP SQLite files plus filesystem sessions. Filesystem mutations use advisory shared/exclusive locks through an Android-capable Unix implementation, preserving the global wipe barrier and per-session mutation serialization on the app-private store. Provider and upstream-MCP OAuth credentials remain in Waltr SQL.

The process-generation local bearer stays in private Rust memory. The loopback endpoint is also retained in private runtime configuration, but it is not memory-only: before catalog load, startup atomically writes it into app config `server/.waltr/resources/mcp-servers/waltr.yaml`. Each generation overwrites the durable file with its current endpoint.

The persisted self-MCP resource has ordinary filesystem origin and catalog behavior. Neither the endpoint nor bearer enters published WebView snapshots, and the bearer never enters durable storage.

Embedded outbound OAuth replaces its callback endpoint with an opaque per-generation marker before it persists attempts or MCP credentials.

Only the matching active server generation rehydrates its marker into the exact callback endpoint. That generation can reuse a still-valid dynamic registration during reauthorization. A replacement generation rejects stale attempts, excludes stale marked credentials, and returns not found for status or cancel against an old attempt.

Every embedded server restart therefore requires outbound MCP reauthorization. A generation-specific DCR registration cannot seed registration reuse in the replacement generation, so DCR runs again when it is the selected mechanism.

Android backup rules exclude all private files, databases, shared preferences, and root data. Cloud backup and device transfer therefore exclude embedded state.

After process death, startup reopens durable stores and runs normal migrations. Routine startup reconciliation marks unfinished invocations with the existing restart outcome.

SQLite settings must use a direct `sqlite:` path. Nested `file:` URI filenames are rejected for both Waltr and MCP targets because they can obscure target identity and collision checks.

## Ownership

Waltr and MCP use separate pools and retain separate schema and migration ownership.

| Backend | Waltr objects | MCP objects |
| --- | --- | --- |
| SQLite | `waltr.sqlite3`: `_sqlx_migrations`, OIDC state, outbound OAuth, durable Routine invocation history, and external capability dispatch receipts | `mcp.sqlite3`: `_sqlx_migrations`, hosted OAuth tables, and inert historical Streamable HTTP session/event tables |
| PostgreSQL | `waltr` schema and `waltr._sqlx_migrations`, including outbound OAuth, Routine history, and external capability dispatch receipts | `mcp` schema and `mcp._sqlx_migrations`, including hosted OAuth |

Each owning crate embeds its forward-only migration files with `sqlx::migrate!()`. SQLx owns the standard `_sqlx_migrations` history format, version/checksum validation, and migration coordination; Waltr adds no migration-history layer. PostgreSQL migration connections set the owning `waltr` or `mcp` search path so the standard history and owned objects are created in that schema. Runtime queries remain owner-qualified where required.

Waltr migrations are immutable and normalized identically across SQLite and PostgreSQL:

| Version | Owner | Purpose |
| --- | --- | --- |
| `0001_schema_ownership.sql` | Waltr | Establish Waltr schema ownership. |
| `0002_oidc_access_auth.sql` | Waltr | Persist OIDC login attempts and browser sessions. |
| `0003_mcp_oauth_login_continuations.sql` | Waltr | Persist browser-bound hosted-MCP authorization continuations. |
| `0004_outbound_oauth.sql` | Waltr | Persist outbound OAuth attempts and the original MCP credential/refresh contract. |
| `0005_unified_oauth_credentials.sql` | Waltr | Hard-cut over provider and MCP credentials to one typed payload and refresh contract. |
| `0006_routine_invocations.sql` | Waltr | Add durable Routine invocation aggregates and the append-only lifecycle journal. |
| `0007_routine_interaction_trigger.sql` | Waltr | Extend legacy trigger history. |
| `0008_routine_result_budget.sql` | Waltr | Extend legacy result retention. |
| `0009_script_invocations.sql` | Waltr | Add JS revisions, invocations, and operation intent/completion journals. |
| `0010_script_text_results.sql` | Waltr | Add bounded durable UTF-8 result ownership and accounting. |
| `0011_script_observation.sql` | Waltr | Add durable publication order and opaque observation cursors. |
| `0013_code_dispatch.sql` | Waltr | Associate neutral agent dispatch fences with admitted code invocations. |
| `0014_retire_routine_dsl.sql` | Waltr | Drop only the three legacy task-history tables; preserve current script and unrelated stores. |
| `0015_external_capability_dispatch.sql` | Waltr | Add non-replayable sessionless capability associations and exact completion receipts. |
| `0016_routine_object_references.sql` | Waltr | Require empty incompatible Routine state; replace active program/text body columns with immutable-object references. |

The V2 access-authentication domain contains:

| Data | Durable contract |
| --- | --- |
| Login attempts | State and browser-correlation digests, nonce, PKCE verifier, local return path, creation/expiry, and one-time consumption. |
| Browser sessions | Opaque token digest, issuer, subject, creation/expiry/last-seen, and revocation. |

Admission is database-coordinated across server instances. The store permits at most 1,024 active login attempts, 1,024 active browser sessions globally, and 32 active browser sessions per issuer/subject. SQLite transactions and PostgreSQL advisory transaction locks make capacity checks and insertion atomic. Cleanup is bounded and opportunistic; expired or revoked records never authenticate even before cleanup.

Waltr migration V3 owns the hosted server's browser-correlation continuation records. V4 introduced outbound OAuth state. `outbound_oauth_attempts` persists state-bound callback attempts, registration/completion mode, expiry, sanitized lifecycle outcome, and an expiring identity-bound completion claim. Expired attempt-completion claims are reclaimable after a process crash, and stale claim identities cannot finish or commit.

V5 is a hard cutover for `outbound_oauth_credentials`: applying it removes existing credential rows, so every previously authorized provider and MCP target must authorize again. The recreated table stores a versioned Waltr-owned credential payload containing a shared OAuth token set and typed provider or MCP context. Access and refresh token values use `oauth2` secret-bearing primitives, while Waltr owns absolute expiry, context, and serialization. Credential revision, claim identity/deadline, reauthorization state, and update time remain envelope columns. Both provider and MCP refreshes use the same database lease and compare-and-swap contract. Callback completion and credential installation commit together for durable MCP callback attempts; provider authorization attempts remain process-local. An indeterminate refresh or expired refresh lease marks the credential for reauthorization rather than risking reuse.

V6 adds `routine_invocations`, normalized `routine_invocation_metadata`, and `routine_invocation_events`. The aggregate table stores current lifecycle state and admission order; metadata is stored separately for exact-match filtering.

The event table stores the append-only journal. Waltr stores ordered task descriptors in admission data and creates no task table.

Event sequences are opaque and potentially gappy durable cursors. PostgreSQL event transactions take one database-wide advisory transaction lock before allocating identities, which orders allocation with commit and prevents lower sequences from becoming durable after higher sequences. Rollbacks and metadata filtering can still produce gaps. SQL replay remains authoritative; process-local watch wakeups only prompt another query.

The preceding V6 event contract is historical migration provenance, not production v2 execution. Migration 0014 drops its three task-history tables without `CASCADE`. The current composition uses `script_invocations`, pinned `script_program_revisions`, and `script_operations`; durable publications and observations are separate from admission order. Code dispatch associations prevent uncertain outer calls from replaying effects. Startup reconciles unfinished script work without running old source. New JavaScript records and unrelated sessions, context masks, resource overrides, credentials and MCP state remain intact. The unshipped migration 0012 is removed without renumbering; retained databases that applied it need separate migration-history handling. See [Routines](routines.md) for current retention and observation contracts.

MCP migrations are likewise immutable. V1 created Streamable HTTP session/event tables for a removed protocol runtime; those tables remain inert so existing SQLx history and checksums stay valid. V2 adds the hosted authorization server's transactions, consents, digest-only codes and refresh tokens, grants, refresh families, access-token authorization records, and ES256 signing keys. V3 adds normalized client-registration, redirect-URI, CIMD snapshot, admission, and rate-limit state. Private signing material is encrypted with the deployment keyring; wrapping keys are never stored in SQL. SQLite transactions and PostgreSQL locking implement the same one-time redemption, refresh rotation/replay, admission, revocation, and signing-key activation contract.

MCP V4 adds OIDC resource-owner provenance. It does not move Waltr browser-session or Routine ownership into the MCP database.

Never edit an applied migration. Schema corrections require a new forward-only migration in both backend directories; SQLx rejects changed checksums on reopen.

The MCP database and every wrapping-key version still referenced by an encrypted signing row are one backup and recovery unit. Cleanup runs every five minutes in batches of 256 and is advisory. Signing maintenance runs every 24 hours, atomically activates one candidate while retaining the prior public key for verification overlap, and rewraps retained private material with the active wrapping key.

## Routine Payload Objects

`routine_objects::ImmutableObjects` owns immutable program and text payloads independently of session storage. SQL retains control records, descriptors, ownership, quota charges and journals. Ordinary bounded results and unrelated SQL JSON remain SQL data.

Standalone selects filesystem storage by default through `WALTR_ROUTINE_OBJECT_STORE`; `s3` selects the explicit S3 adapter. See [deployment configuration](../deployment.md#routine-payload-storage). Embedded composition supplies the database parent's `routine-objects` directory explicitly and rejects workspace overlap. It does not read standalone object-store variables.

Filesystem objects use `<root>/program/<sha256>` and `<root>/text/<sha256>`. New directories use mode 0700 and files use 0600 on Unix. Durable writes sync bytes and directory entries, publish without replacement, then verify readback. S3 uses `<prefix>/routine-objects/program/<sha256>` or `text/<sha256>`, conditional `If-None-Match: *` creation and verified readback. Session-store write modes do not apply.

Payload persistence and integrity verification precede usable SQL references. The publication transaction rechecks invocation state and atomically commits owner, quota and reference. Cancellation, quota conflict or SQL failure can leave an unreferenced object, never a false successful result. A lost SQL acknowledgment remains uncertain and does not authorize effect replay or automatic republication.

Authorized reads verify digest and size. Missing or corrupt objects fail explicitly; catalog bytes and old SQL bodies supply no fallback. Text limits remain 1 MiB per result, 32 results and 8 MiB aggregate charge, with reads through 64 KiB chunks.

Unreferenced objects remain retained. No automatic collector or object deletion API exists. Backups must pair SQL references with the matching object namespace. Namespace-specific retention or reset requires exact inventory, stopped writers and separate approval. Paths and object keys grant no client authority.

Startup preflight and paired migration 0016 guards refuse populated incompatible program, invocation or text state before body-column removal. There is no SQL-to-object migration, converter, legacy category reader or automatic wipe. Historical migration checksums remain immutable. Retained deployments require a separately approved archive/reset procedure for exact affected Routine state; see the [cutover runbook](../runbooks/server-capability-cutover.md). Preserve credentials, access state, hosted MCP state, session namespaces, workspace files, current object namespaces and unrelated dispatch receipts.

Preflight also refuses populated legacy `routine_invocations` before migration 0014 can drop its tables. The migration 0016 SQL guards check `script_program_revisions`, `script_invocations` and `script_text_results` for empty state.

## Sessionless External Dispatch

Shared standalone and embedded composition installs the Waltr SQL `ExternalDispatchStore` before exposing external tools. A missing journal fails closed. Before entering the dispatcher, each call must acknowledge a new `external_capability_dispatch` row in `uncertain` state. Its serialized association contains the authenticated principal and scopes (not bearer credentials), server-generated operation ID, exact target identity, declaration revision and schema, and original call arguments. It creates no agent session.

A known handler result permits one exact compare-and-swap from that association's `uncertain` state to `complete`, storing ordered text/image content and `isError`. A known tool error result can therefore complete; dispatch failure, settlement failure, or crash leaves the caller with uncertainty. Neither state is a restart continuation or replay instruction. The server does not reconstruct or rerun these effects from SQL.

Arguments and result bodies can contain sensitive user data, secrets supplied as tool input, or encoded images. This store serializes them as JSON text; it adds no payload encryption, automatic garbage collection, or expiry. Apply database and backup access/retention controls accordingly. Session deletion or namespace reset does not remove these SQL rows; any database reset or retention deletion needs separate explicit authorization. Migration 0015 is additive and requires no history or credential reset.

See [`external_journal.rs`](../../crates/waltr-server/src/agent_host/external_journal.rs), [`external.rs`](../../crates/waltr-server/src/agent_host/external.rs) (`dispatch_durable`), and the [SQL store](../../crates/waltr-server/src/persistence/external_dispatch.rs).

## Coordinated Capability Upgrade

Server-owned capabilities, MCP, permissions, and interactions require coordinated binary/client/resource/history upgrades. Pending interactions and private grants have no SQL or session persistence and disappear at restart. Removed session variants reject without dual decoders or automatic wipe. The [upgrade runbook](../runbooks/server-capability-cutover.md) separates incompatible Routine state from session and credential domains. No whole-database or auth reset follows from the ownership change.

## Startup Flow

1. Parse `WALTR_DATABASE_URL` and optional `WALTR_MCP_DATABASE_URL`, or resolve and derive the durable defaults.
2. Select the explicit Routine object backend, reject incompatible populated Routine state, and run Waltr's standard embedded migrator.
3. Open the independent MCP SQLx pool and construct the hosted OAuth stores, which run MCP's standard embedded migrator.
4. Validate access-auth and hosted-OAuth environment configuration and, in OIDC mode, perform upstream discovery, load the wrapping-key ring, and decrypt or bootstrap the active local signer.
5. Construct the shared JavaScript invocation service, reconcile unfinished script work, enforce new-format Routine configuration, and register the v2 router. The legacy engine is not started.
6. Bind the listener and expose health and readiness.

Any failure before router construction prevents the server from binding or reporting readiness. Errors identify the failing subsystem without including credentials or database URLs.

The environment-owned `build_app(BuildAppOptions)` is therefore async and fallible. Explicit builders require initialized `Persistence`, and authenticated variants require an initialized `AccessAuthenticator`; composition also awaits Routine preflight and reconciliation. All builders return a server-owned application lifecycle whose hosted-OAuth maintenance is cancelled and joined with server shutdown, independent of any external `Persistence` clones. There is no MCP protocol-session store or cleanup loop.

## Boundaries

- `crates/waltr-server/src/persistence/` owns database selection, Waltr migrations, and startup composition.
- `crates/mcp/src/oauth/server/` owns generic hosted-OAuth state transitions and runtime persistence; Waltr selects its MCP SQL pool and deployment policy.
- `crates/waltr-server/src/session/` and `waltr-agent` continue to own agent session storage. `WALTR_SESSION_STORE` is not an alias for `WALTR_DATABASE_URL`.
- Domain features should add purpose-specific stores and migrations rather than a generic key/value table.
- `crates/waltr-server/src/access_auth/store.rs` owns the OIDC V2 domain store and admission rules; handlers do not issue SQL directly.
- `crates/waltr-server/src/persistence/outbound_oauth.rs` owns durable callbacks, unified outbound credentials, and refresh claims.
- `crates/waltr-server/src/persistence/script_invocation/`, `script_text_result/`, and `script_code_dispatch.rs` own current Routine journals, observations, text, and code associations. The old task-history decoder/store is removed.
- `crates/waltr-server/src/persistence/external_dispatch.rs` owns non-replayable sessionless capability associations and completion receipts in Waltr SQL, separate from session history and live interaction waiters.

## Tests

Persistence tests cover SQLite and container-backed PostgreSQL migration startup, history isolation, SQLx checksum rejection, independent pools, SQLite target collision rejection, foreign-key behavior, and hosted-OAuth store contracts. Historical V1 rows are migration history only; no test reconstructs protocol sessions. See [Testing](../testing.md) for the server test commands.

## References

- [`crates/waltr-server/src/persistence/`](../../crates/waltr-server/src/persistence)
- [`crates/waltr-server/src/app.rs`](../../crates/waltr-server/src/app.rs)
- [MCP protocol](../mcp/protocol.md)
- [Access authentication](access-authentication.md)
- [Routines](routines.md)
- [Deployment](../deployment.md)
