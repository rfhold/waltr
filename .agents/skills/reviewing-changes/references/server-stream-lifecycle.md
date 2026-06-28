# Server Stream Lifecycle

## Invariant

Server streams provide contract-shaped initial state, ordered updates, precise errors, and cleanup when clients disconnect.

## Why It Matters

Streams are the server's shared-state contract; drift causes clients to render stale or partial state and can leak resources.

## Source Files To Inspect

- `crates/waltr-server/src/config_service.rs`
- `crates/waltr-server/src/status_service.rs`
- `crates/waltr-server/src/config/`
- `crates/waltr-server/tests/`
- `docs/architecture/config.md`
- `proto/`

## Common Drift Signs

- Initial stream event is sparse when the contract requires a full snapshot.
- Update events omit ETag or changed-section metadata.
- Cancellation leaves watcher, pubsub, or task resources alive.
- Domain errors map to generic gRPC status codes.

## Suggested Tests

Add server integration tests for initial snapshot, ordered updates, cancellation, pubsub cleanup, and status-code mapping.

## Finding Format

`[server-stream-lifecycle] <path>:<line> changes <stream> lifecycle by <missing snapshot/update/error/cleanup>.`
