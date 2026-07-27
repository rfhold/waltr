# MCP Streamable HTTP Session Persistence Plan

## Status

Implemented by `cee28bb` and hardened by `6fed1ec`. This is the first historical plan in the persistence and authentication sequence. Use the [MCP protocol documentation](../mcp/protocol.md) and [Server Persistence](../architecture/persistence.md) for the current contract.

## Objective

Make MCP Streamable HTTP session state durable without introducing Waltr-specific behavior into the generic `mcp` crate.

The crate exposes an owner-bound, bounded session persistence contract, preserves an in-memory default, and provides optional SQLite and PostgreSQL implementations. Each SQL implementation runs migrations embedded with SQLx's standard `migrate!()` mechanism when it opens its MCP-owned store.

## Preimplementation Baseline

Before this plan was implemented, `crates/mcp/src/server/mod.rs` kept Streamable HTTP sessions in a process-local `HashMap`. Each session contained its initialization lifecycle and queued SSE events.

That implementation had the following consequences:

- Lifecycle and queued events are lost on restart.
- Multiple server instances cannot share sessions.
- GET delivery was process-local and could not resume from another instance.
- SSE IDs are process-local numeric strings.
- `Last-Event-ID` is compared as a string, so values such as `10` and `9` are ordered incorrectly.

## Scope

- Add a public, object-safe Streamable HTTP session store trait.
- Move the existing in-memory behavior behind the trait.
- Allow `StreamableHttpOptions` to accept a session store.
- Add optional SQLite and PostgreSQL stores.
- Add backend-specific raw SQL migrations.
- Run pending migrations when a SQL store is opened.
- Give MCP tables and standard SQLx migration history MCP-owned storage.
- Assign and compare SSE event IDs numerically.
- Preserve the existing public router constructors and in-memory default.

## Boundaries

The `mcp` crate owns protocol-level state only:

- MCP session identifiers.
- Streamable HTTP initialization lifecycle.
- Queued server-to-client SSE messages.
- Event ordering and deletion semantics.

The crate must not own Waltr users, OIDC login attempts, browser sessions, agent sessions, server configuration, or deployment policy. It accepts only an opaque stable `McpPrincipalId` from an integrating authorization validator and must not depend on any Waltr crate.

## Store Contract

Add a public trait under `mcp::server`, named `StreamableHttpSessionStore` unless implementation work reveals a clearer protocol-specific name. It should use the crate's existing boxed future pattern rather than adding `async-trait` solely for this contract.

The contract must support these operations:

| Operation | Required behavior |
| --- | --- |
| Read lifecycle | Return the lifecycle for a session or report that it does not exist. |
| Write lifecycle | Create or update lifecycle state without discarding queued events. |
| Append events | Atomically assign ordered numeric IDs and append messages to a session. |
| Prepare delivery | In one store operation, validate owner and lifecycle, apply a `Last-Event-ID` acknowledgement, and lease the next ordered batch without deleting it before delivery. |
| Delete session | Remove lifecycle and queued events and report whether the session existed. |

The public store error should distinguish unavailable storage and invalid persisted data. Router handlers should map store failures to a server error without leaking database details.

`StreamableHttpSessionStoreConfig` supplies the 24-hour TTL, 10,000 global and 32 per-principal session quotas, event count and byte quotas, bounded delivery and cleanup batch sizes, and delivery/initialization lease durations. Every operation enforces owner, expiry, lifecycle, protocol version, and relevant quotas before mutation; bounded cleanup is advisory.

The implementation must keep lifecycle validation and mutation safe under concurrent requests. If separate read and write methods cannot preserve a lifecycle transition atomically, the trait should expose a compare-and-set or higher-level transition operation rather than relying on handler-side sequencing.

## Router Integration

`StreamableHttpOptions` will carry an `Arc<dyn StreamableHttpSessionStore>`. Its default will construct the in-memory store, preserving the behavior and dependency footprint of existing router constructors.

The POST, GET, and DELETE handlers will use the store for all session state:

| Request | Store behavior |
| --- | --- |
| POST `initialize` | Validate the previous lifecycle and persist `InitializeResponded` after the response is produced. |
| POST `notifications/initialized` | Persist `Initialized` while preserving queued events. |
| Other POST | Read persisted lifecycle before applying protocol lifecycle rules. |
| Notification-only POST | Append emitted server notifications to the persisted event queue. |
| GET | Require an owner-matched initialized session, acknowledge the prior cursor, and non-destructively lease one resumable outbox batch. |
| DELETE | Delete the session and its queued events. |

Authorization remains ahead of storage access. An unauthorized request must not create, read, update, or delete a session.

Absent Origin is accepted. Every present Origin is denied unless the integrating caller installs a canonical allowlist. POST SSE responses are non-resumable and have no event IDs; the session's single GET outbox is the only resumable delivery channel.

## Event IDs

Stores will use an unsigned or positive 64-bit numeric event ID internally. SSE responses will continue to serialize the ID as a string as required by the transport format.

`Last-Event-ID` will be parsed as a numeric value. Invalid values will receive a clear client error rather than falling back to lexical comparison.

SQL stores should let the database assign IDs. The in-memory store should use a counter owned by that store so two routers sharing one store also share one event sequence.

## SQL Layout

PostgreSQL uses an `mcp` schema. SQLite uses an MCP-owned database with `mcp_` table prefixes. Exact timestamp types may differ by backend, but lifecycle values and event ordering have equivalent semantics.

| Logical table | Purpose |
| --- | --- |
| `streamable_http_sessions` | Session ID, lifecycle, creation time, and update time. |
| `streamable_http_events` | Numeric event ID, owning session ID, serialized JSON-RPC message, and creation time. |

The event table has a foreign key with cascading deletion and an index on `(session_id, event_id)`.

Migration history is SQLx's standard `_sqlx_migrations` table. SQLite therefore requires an MCP-owned file or in-memory database rather than sharing another owner's file. PostgreSQL runs the migrator with the `mcp` search path, placing history at `mcp._sqlx_migrations` even when Waltr and MCP use the same logical database.

## Migrations

Migration files are committed at these backend-specific paths:

- `crates/mcp/migrations/sqlite/`
- `crates/mcp/migrations/postgresql/`

Migration rules:

- Files contain raw SQL.
- Versions only move forward.
- SQLx records applied migrations and owns version/checksum validation.
- An applied migration is never edited; corrections use a new migration.
- Opening a SQL store runs all pending migrations before the store is returned.
- A migration or checksum failure prevents the store from being used.
- Migration coordination and history behavior use SQLx's standard migrator behavior.

## Cargo Features

Keep SQL dependencies optional so users of protocol primitives and in-memory transports do not acquire database drivers.

The implemented features are:

- `streamable-http-sqlite`
- `streamable-http-postgres`

The implementation uses SQLx for both backends, asynchronous request handling, transactions, embedded migrations, and standard migration histories.

## Implementation Steps

1. Define persisted lifecycle, event, error, and store types in a focused server session module.
2. Implement and test the in-memory store independently of Axum handlers.
3. Refactor the router to use the store while preserving current API defaults.
4. Replace string event comparisons with numeric parsing and ordering.
5. Add SQLite migrations, store implementation, and feature-gated tests.
6. Add PostgreSQL migrations, store implementation, and container-backed tests.
7. Update MCP protocol documentation for the new options and feature flags.

## Tests

Follow the MCP layering in [testing.md](../testing.md).

Store contract tests must cover:

- Lifecycle create, read, update, and invalid persisted values.
- Ordered event append, redelivery before acknowledgement, and pruning after acknowledgement.
- Numeric ordering across event IDs `9`, `10`, and `11`.
- `Last-Event-ID` pruning.
- Cascading event deletion.
- Idempotent deletion of an unknown session.
- Concurrent lifecycle and event operations.

Transport tests must cover:

- A session remaining valid when a second router uses the same store.
- Queued events surviving reconstruction of the router.
- Repeated initialization being rejected after reconstruction.
- GET requiring an initialized session.
- DELETE making later GET requests return unknown session.
- Unauthorized POST, GET, and DELETE leaving storage unchanged.

SQLite tests use temporary databases and run normally. The container-backed PostgreSQL target is ignored and runs explicitly with Docker:

`cargo test -p mcp --features streamable-http-postgres --test streamable_http_postgres_store -- --ignored`

## Acceptance Criteria

- Existing MCP tests pass with the default in-memory store.
- SQLite and PostgreSQL stores pass the same behavioral contract.
- Sessions are bound to anonymous or stable authenticated owners; wrong-owner operations are indistinguishable from unknown sessions.
- TTL, global/per-principal quotas, event limits, leases, and bounded cleanup are enforced consistently.
- GET delivery is acknowledgment-based and at least once; POST SSE remains non-resumable.
- Present Origins are denied by default, and MCP clients use fallible explicit shutdown for DELETE and local cleanup.
- SQL migrations run automatically and idempotently when stores open.
- Two routers sharing a SQL database observe the same lifecycle and queued events.
- Restarting a router does not lose persisted session state.
- `Last-Event-ID` uses numeric ordering.
- The default `mcp` feature set does not require a SQL backend.
- No Waltr-specific type or policy is added to the `mcp` crate.

## Phase-Local Non-Goals

These items were outside this generic MCP persistence plan, not necessarily outside later plans or future work.

- Persisting MCP client-side session IDs or access tokens.
- Multiple independent resumable GET outboxes per session; the contract intentionally has one ordered outbox.
- Garbage collection or expiry policy beyond recording timestamps needed for a later policy.
- Waltr OIDC, browser sessions, users, or agent session storage.
- A generic database framework for unrelated application data.

## Dependencies

This plan has no dependency on the other foundational plans. It enables [Waltr Server Persistence Foundation](waltr-server-persistence.md), which will select and inject the MCP store during server startup.

## References

- [MCP protocol documentation](../mcp/protocol.md)
- [`crates/mcp/src/server/mod.rs`](../../crates/mcp/src/server/mod.rs)
- [`crates/mcp/AGENTS.md`](../../crates/mcp/AGENTS.md)
- [Testing](../testing.md)
