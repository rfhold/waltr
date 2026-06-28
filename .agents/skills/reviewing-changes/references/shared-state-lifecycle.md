# Shared State Lifecycle

## Invariant

Shared server data flows through `waltr-client` state, events, effects, runtime orchestration, and snapshots; UI shells render snapshots and dispatch commands.

## Why It Matters

One authoritative state machine keeps web and TUI consistent and prevents mutation completions from racing watch/list/read updates.

## Source Files To Inspect

- `crates/waltr-client/src/client.rs`
- `crates/waltr-client/src/coordination/state.rs`
- `crates/waltr-client/src/coordination/event.rs`
- `crates/waltr-client/src/coordination/runtime.rs`
- `crates/waltr-client/src/view.rs`
- `docs/architecture/clients.md`

## Common Drift Signs

- UI-local stores mirror config, status, sessions, or other server data.
- Reducers perform RPCs, sleeps, IO, or platform work.
- Mutation completion directly rewrites authoritative collections instead of operation results.
- Snapshots omit state needed by both UI shells.

## Suggested Tests

Add reducer tests for pure transitions and client integration tests for command, effect, transport, watch, and snapshot behavior.

## Finding Format

`[shared-state-lifecycle] <path>:<line> duplicates or mutates shared server data outside waltr-client <state/effect/snapshot> flow.`
