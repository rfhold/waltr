# Request Identity

## Invariant

Requests that can overlap need stable operation identity, keyed pending/completion state, and stale-result guards.

## Why It Matters

Without request identity, late completions can overwrite newer user intent or make UI feedback attach to the wrong operation.

## Source Files To Inspect

- `crates/waltr-client/src/coordination/state.rs`
- `crates/waltr-client/src/coordination/event.rs`
- `crates/waltr-client/src/coordination/config.rs`
- `crates/waltr-client/src/coordination/session.rs`
- `crates/waltr-web/src/routes/`
- `crates/waltr-tui/src/`

## Common Drift Signs

- A single boolean tracks multiple concurrent operations.
- Completion events do not carry or check the originating operation ID.
- UI clears an error or pending state for a newer request when an older request completes.
- Retry paths reuse stale payloads or stale ETags without explicit conflict handling.

## Suggested Tests

Test overlapping requests, out-of-order completions, retry after failure, and stale ETag failure mapping.

## Finding Format

`[request-identity] <path>:<line> lets <late/foreign> completion affect <operation> because <missing identity check>.`
