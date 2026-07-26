# 08 MCP Session Store Contract

Status: Complete

Replace the destructive, unbounded, ownerless durable session contract with an owner-bound, bounded, acknowledgment-based one. Migrations are undeployed: rewrite `0001` in place.

## Findings Resolved

- `take_events` deletes queued events before HTTP delivery; disconnect after commit loses messages despite documented resumability (memory/SQLite/PostgreSQL stores + `server/mod.rs` GET).
- Sessions/events have no owner, TTL, quotas, or cleanup; any bearer can drive another client's session; DB grows forever.
- Idle GET polling produces 10 Hz `BEGIN IMMEDIATE` + DELETE churn.
- Initialized lifecycle commits separately from emitted events; crash between them strands the session.
- Negotiated protocol version is neither validated nor persisted.

## Public API (crates/mcp)

### oauth.rs

- `McpPrincipalId(String)`: nonempty UTF-8, <=256 bytes, stable across token renewal, never a token.
- `McpTokenValidation::Authorized { principal_id: McpPrincipalId, expires_at: Option<SystemTime> }` (expiry consumed by WS9 SSE bounding).

### server/session.rs

- `StreamableHttpSessionOwner::{Anonymous, Authenticated(McpPrincipalId)}`; wrong owner is indistinguishable from unknown session (`NotFound` -> HTTP 404). Anonymous sessions count against the global cap only.
- `StreamableHttpSessionStoreConfig` defaults: TTL 24h; 10_000 global; 32 per principal; 1_024 events/session; 8 MiB pending bytes/session; 1 MiB per event; delivery batch 128; cleanup batch 128; delivery lease 30s; initialization claim lease 60s.
- Key/request/ID types: `StreamableHttpSessionKey { session_id, owner }`; requests carry `presented_protocol_version: Option<String>` and `now: SystemTime`; `StreamableHttpEventId { stream_id, sequence }` with strict `Display`/`FromStr` (<=160 bytes, single separator, sequence within `0..=i64::MAX`).
- Trait operations replacing `read_lifecycle`/`write_lifecycle`/`compare_and_set_lifecycle`/`take_events`:
  - `create_session`, `inspect_session`, `claim_initialization`, `complete_initialization`, `abandon_initialization`, `append_events`, `prepare_delivery`, `delete_session`, `cleanup_expired(now, limit)`.
- Outcomes (not store errors): `NotFound`, `ProtocolVersionMismatch`, `LifecycleMismatch`, `InitializationInProgress { retry_after }`, `AlreadyCompleted`, `CapacityExceeded`, `Idle`, `DeliveryBusy { retry_after }`, `Events(...)`, `Deleted`. Store errors stay `Unavailable | InvalidData`.

## Schema (rewrite both `0001_streamable_http_sessions.sql` in place)

Sessions: `session_id` PK; `owner_key`; `protocol_version`; `lifecycle` (`initialize_responded|initializing|initialized`); `initialization_operation_id`; nullable claim id/expiry pair; `completed_initialization_claim_id`; `event_stream_id`; `next_event_sequence` (default 1); `delivered_event_sequence` (default 0); `acknowledged_event_sequence` (default 0); `pending_event_count`; `pending_event_bytes`; nullable delivery id/from/through/lease quad; `created_at_ms`/`updated_at_ms`/`expires_at_ms`. Epoch-millisecond integers in both backends.

Constraints: timestamp ordering; `0 <= acked <= delivered < next`; nonnegative counters; claim pair all-or-none; `initializing` requires claim; delivery quad all-or-none with ordered range `<= delivered`. Indexes: `(expires_at_ms, session_id)`, `(owner_key, expires_at_ms)`.

Events: `(session_id, event_sequence)` PK, `message`, `message_bytes` (checked against actual byte length per backend), `created_at_ms`, FK cascade, `event_sequence > 0`, `message_bytes > 0`. No AUTOINCREMENT/global identity.

## Semantics

- Create: bounded expired cleanup, cap counts, insert (`initialize_responded`, negotiated version, random stream id, stable init operation id, fixed absolute expiry) in one transaction; ID collision -> `AlreadyExists`, router retries bounded times.
- Append: serialize/validate sizes before the transaction; lock session; validate owner/version/lifecycle/expiry/quotas; reserve contiguous sequence range; insert; update counters; all-or-nothing. Empty append validates read-only.
- `prepare_delivery` (core): load by `(session_id, owner_key)`; expired/wrong-owner fold into `NotFound`; validate presented version; require `initialized`; validate cursor (same stream id, `<= delivered`, in range) — invalid cursors mutate nothing; advance `acknowledged = max(acked, presented)`; delete only `<= acknowledged`; clear the lease when fully acked or expired; unexpired partial lease -> `DeliveryBusy { retry_after }`; claim next batch (fresh delivery id + lease, advance `delivered`) returning events WITHOUT deleting; empty -> `Idle`. Initial idle check with no new acknowledgment must be a plain read (no write transaction, no timestamp touch) — this removes idle SQLite churn.
- Initialization: `claim_initialization` transitions `initialize_responded -> initializing` with claim id + lease; `complete_initialization` verifies the claim, appends emitted events, sets `initialized`, records completed claim id atomically; duplicate completion with the same claim is idempotent `AlreadyCompleted`; expired claims reclaimable; `abandon_initialization` releases on handler failure.
- Delivery is at-least-once; redelivery reuses identical event IDs. Expired sessions never authenticate or deliver even before physical deletion. `cleanup_expired` is bounded, ordered, cascade-deletes events, and reports counts.
- SQLite: mutations use `BEGIN IMMEDIATE`; no transaction held across HTTP. PostgreSQL: `SELECT ... FOR UPDATE` per session; advisory lock only for global admission; cleanup uses `FOR UPDATE SKIP LOCKED`.

## Implementation Order

1. Types + trait + outcomes in `session.rs`; update mock/test validators and macro fixtures for principal-bearing authorization.
2. Rewrite the in-memory store; build the shared behavioral contract suite in `tests/streamable_http_store_support/mod.rs` against it.
3. Rewrite SQLite migrations + store; pass the contract suite; add idle-read (no `updated_at_ms`/lock churn), constraint, and checksum tests.
4. Rewrite PostgreSQL migrations + store; pass the ignored container-backed contract; add admission/lock-serialization coverage.

## Contract Suite Musts

Owner persistence across reopen; wrong-owner `NotFound` on inspect/append/deliver/delete without mutation; same-principal token renewal retains access; version persistence/mismatch; sequences across 9->10->11; first delivery does not delete; unacked replay after lease expiry; partial ack deletes prefix only; full ack clears lease/counters; malformed/future/wrong-stream cursors mutate nothing; concurrent GET claims yield each message once; concurrent append preserves order; append-vs-delete cannot resurrect; init lifecycle+events commit together; ambiguous completion idempotent; expired claim recoverable with the same operation id; quotas atomic under concurrency; expired sessions inert pre-cleanup; cleanup bounded + cascading.

## Exit Criteria

- All three stores pass one shared contract; no destructive pre-delivery deletes remain; owners isolate sessions; growth is bounded; initialization cannot strand sessions.

## Verification

```
cargo test -p mcp --test spec
cargo test -p mcp --tests
cargo test -p mcp --features streamable-http-sqlite --test streamable_http_sqlite_store
cargo test -p mcp --features streamable-http-postgres --test streamable_http_postgres_store -- --ignored
```
