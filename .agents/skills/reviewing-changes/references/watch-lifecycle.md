# Watch Lifecycle

## Invariant

Client watch streams open only when readiness and interest allow them, stay scoped to active interest, and reconnect according to status-gated rules.

## Why It Matters

Correct watch lifecycle prevents leaked streams, redundant server load, and stale UI after readiness changes.

## Source Files To Inspect

- `crates/waltr-client/src/coordination/stream.rs`
- `crates/waltr-client/src/coordination/runtime.rs`
- `crates/waltr-client/src/coordination/status.rs`
- `crates/waltr-client/src/transport.rs`
- `docs/architecture/clients.md`
- `docs/architecture/config.md`

## Common Drift Signs

- Child watches open before status readiness.
- Watch interest is ignored when routes or screens unmount.
- Child watch errors trigger global failure instead of local retry state.
- Failed status does not block child watch reconnects.

## Suggested Tests

Add client integration tests for readiness gating, interest start/stop, local child failure, status recovery, and reconnect sequencing.

## Finding Format

`[watch-lifecycle] <path>:<line> violates <readiness/interest/reconnect> rule for <watch name>.`
