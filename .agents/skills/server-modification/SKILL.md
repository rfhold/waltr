---
name: server-modification
description: Helps an agent modify the Waltr server when the user asks to change coordination, state sharing, or server APIs.
---

# Server Modification

Treat the server as a coordination layer that keeps multiple clients aligned on state.

## When To Use

Use this skill when changing `crates/waltr-server/`, server APIs, gRPC service behavior, persistence, validation, status reporting, config watching, pubsub, health routes, or server-side shared-state coordination.

Do not use this skill for client reducer decisions or UI state unless the server contract itself must change.

## Core Boundary

`waltr-server` owns shared coordination and server-side contracts. It exposes RPC services, validates and persists shared state, protects concurrent writes, publishes watch updates, and reports readiness. It should not own browser or terminal UI state, and it should not encode client reducer policy beyond explicit server contract semantics.

## Modification Patterns

1. Start from the contract: identify whether the change affects proto messages, service behavior, persistence, watch events, status, or health.
2. Keep validation and persistence server-owned for shared mutable state.
3. Use explicit concurrency behavior for shared writes; config writes use ETags and stale edits map to `failed_precondition`.
4. Publish watch events for changes that other clients must observe instead of relying on polling or UI-local state.
5. Keep watch payloads contract-shaped: initial snapshots are full, later config changes may be sparse changed sections with ETag metadata.
6. Keep filesystem behavior behind config store, watcher, and pubsub abstractions rather than leaking file details into services.
7. Convert domain errors to precise gRPC statuses at the service boundary.
8. Keep router construction centralized in `app.rs` and service-specific behavior in the corresponding service module.

## Test Selection

- Use `cargo test -p waltr-server --lib` for store, watcher, pubsub, validation, hashing, status helpers, and other crate-local behavior.
- Use `cargo test -p waltr-server --tests` for gRPC contract behavior, config reads and patches, validation errors, stale ETags, watch streams, status watch, health, and process/system flows.
- Keep assertions at the server layer: verify RPC status codes, response payloads, persisted state, watch events, health responses, and coordination semantics.

## Commands

Run server unit tests:

`cargo test -p waltr-server --lib`

Run server integration tests:

`cargo test -p waltr-server --tests`

## References

| Path | Info |
| --- | --- |
| [../../../docs/architecture/config.md](../../../docs/architecture/config.md) | Config persistence, patch, ETag, and watch architecture. |
| [../../../docs/architecture/clients.md](../../../docs/architecture/clients.md) | Server/client boundary and shared client contract. |
| [../../../docs/testing.md](../../../docs/testing.md) | Canonical layered test structure and commands. |
| [../../../crates/waltr-server/AGENTS.md](../../../crates/waltr-server/AGENTS.md) | Server crate boundary and client dependency contract. |
| [../../../crates/waltr-server/src/app.rs](../../../crates/waltr-server/src/app.rs) | Axum router and gRPC route construction. |
| [../../../crates/waltr-server/src/config_service.rs](../../../crates/waltr-server/src/config_service.rs) | Config RPC service behavior. |
| [../../../crates/waltr-server/src/status_service.rs](../../../crates/waltr-server/src/status_service.rs) | Status RPC service behavior. |
| [../../../crates/waltr-server/src/config/](../../../crates/waltr-server/src/config/) | Config traits, filesystem store, watcher, pubsub, and errors. |
| [../../../crates/waltr-server/tests/](../../../crates/waltr-server/tests/) | Server contract and system tests. |
