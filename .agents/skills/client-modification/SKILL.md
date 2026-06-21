---
name: client-modification
description: Helps an agent modify the Waltr client when the user asks to change shared client state or server-facing client behavior.
---

# Client Modification

Treat the client as the shared state machine that owns server data state for UI clients.

## When To Use

Use this skill when changing `crates/waltr-client/`, shared client state, server-facing client behavior, client runtime orchestration, transport behavior, or view models consumed by web and TUI.

Do not use this skill for browser-only forms or terminal-only rendering unless the change reveals shared server-data behavior that belongs in `waltr-client`.

## Core Boundary

`waltr-client` is shell-neutral. It coordinates server data for UI clients and exposes commands, reducer state, effect intents, transport adapters, runtime orchestration, and stable snapshots. Web and TUI should consume client snapshots and dispatch client commands instead of duplicating server-data state machines.

## Modification Patterns

1. Decide whether the behavior is shared server-data coordination or platform UI behavior.
2. Put shared state in `ClientState` and expose shell-facing data through `ClientSnapshot` or view models.
3. Model facts that already happened as `ClientEvent` values.
4. Model work that must happen outside the reducer as `ClientEffect` values.
5. Keep reducers deterministic: mutate state and emit effects, but do not perform RPCs, sleeps, IO, or platform work in reducer code.
6. Keep RPC execution in the transport, executor, runtime, or stream orchestration layer.
7. Add command helpers on `Client` when web and TUI need the same server-data mutation path.
8. Build config patches from the latest loaded config and current ETag; treat stale ETag failures as conflicts rather than success.
9. Preserve watch maintenance rules: status readiness gates child watch opening, watch interest drives watch lifetime, child watch failures stay local, and failed status blocks child watch reconnects until status recovers.
10. Keep server-data collections authoritative from list/read/watch events: mutation completions should update keyed operation results, not directly mutate local collection state for config, sessions, or future server data.

## Test Selection

- Use `cargo test -p waltr-client --lib` for reducer helpers, state transitions, validation helpers, view models, and pure coordination logic.
- Use `cargo test -p waltr-client --tests` for UI-like shared client behavior over the server contract, including config load/patch/watch, stale ETags, status readiness, stream startup, retry, and snapshot behavior.
- Keep assertions at the client layer: verify commands, snapshots, state transitions, emitted effects, and client reaction to server contract behavior.

## Commands

Run client unit tests:

`cargo test -p waltr-client --lib`

Run client integration tests:

`cargo test -p waltr-client --tests`

## References

| Path | Info |
| --- | --- |
| [../../../docs/architecture/clients.md](../../../docs/architecture/clients.md) | Shared client architecture, reducer/effect model, runtime orchestration, and implementation contract. |
| [../../../docs/architecture/config.md](../../../docs/architecture/config.md) | Config patch, ETag, watch, merge, and reload behavior. |
| [../../../docs/testing.md](../../../docs/testing.md) | Canonical layered test structure and commands. |
| [../../../crates/waltr-client/AGENTS.md](../../../crates/waltr-client/AGENTS.md) | Client crate boundary and dependency contract. |
| [../../../crates/waltr-client/src/client.rs](../../../crates/waltr-client/src/client.rs) | Reducer entry point and command helpers. |
| [../../../crates/waltr-client/src/coordination/](../../../crates/waltr-client/src/coordination/) | Shared state, events, effects, reducers, runtime, and stream orchestration. |
| [../../../crates/waltr-client/src/transport.rs](../../../crates/waltr-client/src/transport.rs) | Runtime-neutral server transport boundary. |
| [../../../crates/waltr-client/src/view.rs](../../../crates/waltr-client/src/view.rs) | Stable shell-facing view models. |
| [../../../crates/waltr-client/tests/](../../../crates/waltr-client/tests/) | Client integration tests over the server contract. |
