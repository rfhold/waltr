---
name: agent-modification
description: Helps an agent modify the isolated Waltr agent library when the user asks to change agent loops, providers, tools, permissions, middleware, sessions, or agent tests.
---

# Agent Modification

Treat the agent as an isolated hostable library for model interaction, tools, permissions, middleware, and durable sessions.

## When To Use

Use this skill when changing `crates/waltr-agent/`, provider adapters, provider-neutral streaming types, agent loop behavior, middleware, prompt assembly, tools, permissions, MCP integration, durable sessions, timeline events, session storage, or agent library tests.

Do not use this skill for server coordination, shared client reducers, browser UI, or terminal UI unless the change exposes behavior that belongs in the isolated agent library.

## Core Boundary

`waltr-agent` is a normal Rust library boundary. It owns provider-neutral agent configuration, model request and stream types, tool contracts, permission evaluation, middleware hooks, timeline output, and durable session replay. It should not depend on `waltr-server`, `waltr-client`, `waltr-tui`, or `waltr-web`; host layers own server APIs, UI presentation, process lifecycle, and runtime wiring around the library.

## Modification Patterns

1. Decide whether the behavior belongs to the hostable agent library or to a host layer.
2. Keep public host-facing contracts in `lib.rs`, `agent/`, `stream.rs`, `timeline.rs`, `tools/`, `permission/`, and provider-neutral types.
3. Route provider-specific encoding, parsing, reasoning, thinking, and continuation metadata through `provider/` adapters while preserving provider-neutral request and event semantics.
4. Keep the agent loop responsible for turn progression, provider streaming, tool execution, permission checks, timeline events, and session journal persistence.
5. Keep tools opt-in through `AgentBuilder`; a default agent should not gain filesystem, shell, MCP, skills, task, or question tools implicitly.
6. Evaluate permission claims before executing host tools that read, write, delete, execute, or otherwise affect external resources.
7. Preserve deterministic middleware assembly order because it affects prompt construction, tool visibility, and provider request shape.
8. Base durable session replay on persisted timeline and journal records, not provider-specific transient parser state.
9. Keep deterministic tests on scripted providers and shared support; reserve real provider and external service behavior for ignored live tests.

## Test Selection

- Use `cargo test -p waltr-agent --features shell --lib` for crate-local builders, providers, middleware, tools, permissions, stream parsing, and pure session behavior.
- Use `cargo test -p waltr-agent --features shell --test agent` for deterministic agent integration behavior using scripted/shared providers, including loop, session, permissions, MCP, question, filesystem, and shell flows.
- Use `cargo test -p waltr-agent --features shell --test live -- --ignored` for real provider, external service, live tool, storage, MCP, shell, and filesystem coverage.
- Keep assertions at the agent library boundary: verify provider-neutral requests and events, timeline output, session records, suspensions, permission outcomes, tool results, and public API behavior.

## Commands

Run agent unit tests:

`cargo test -p waltr-agent --features shell --lib`

Run deterministic agent integration tests:

`cargo test -p waltr-agent --features shell --test agent`

Run ignored live agent tests:

`cargo test -p waltr-agent --features shell --test live -- --ignored`

## References

| Path | Info |
| --- | --- |
| [../../../docs/architecture/agent.md](../../../docs/architecture/agent.md) | Agent architecture, boundaries, runtime flow, providers, tools, permissions, middleware, sessions, and invariants. |
| [../../../docs/testing.md](../../../docs/testing.md) | Canonical layered test structure, commands, live-test setup, and container-backed test notes. |
| [../../../crates/waltr-agent/AGENTS.md](../../../crates/waltr-agent/AGENTS.md) | Agent crate isolation boundary and dependency contract. |
| [../../../crates/waltr-agent/src/lib.rs](../../../crates/waltr-agent/src/lib.rs) | Public exports and module boundary. |
| [../../../crates/waltr-agent/src/agent/builder.rs](../../../crates/waltr-agent/src/agent/builder.rs) | Agent construction, tools, permissions, prompt, MCP, and middleware assembly. |
| [../../../crates/waltr-agent/src/agent/loop.rs](../../../crates/waltr-agent/src/agent/loop.rs) | Agent run loop, provider streaming, tool execution, permissions, timeline events, and session persistence. |
| [../../../crates/waltr-agent/src/agent/session/](../../../crates/waltr-agent/src/agent/session/) | Durable sessions, snapshots, suspensions, resume inputs, and storage backends. |
| [../../../crates/waltr-agent/src/stream.rs](../../../crates/waltr-agent/src/stream.rs) | Provider-neutral request, event, turn, tool, thinking, and metadata types. |
| [../../../crates/waltr-agent/src/provider/](../../../crates/waltr-agent/src/provider/) | Provider adapter implementations and provider-specific protocol handling. |
| [../../../crates/waltr-agent/src/middleware/mod.rs](../../../crates/waltr-agent/src/middleware/mod.rs) | Middleware hook contract for prompt, tools, requests, results, and suspensions. |
| [../../../crates/waltr-agent/src/tools/mod.rs](../../../crates/waltr-agent/src/tools/mod.rs) | Built-in tool modules. |
| [../../../crates/waltr-agent/src/permission/mod.rs](../../../crates/waltr-agent/src/permission/mod.rs) | Permission policy, claims, matching, and effects. |
| [../../../crates/waltr-agent/tests/AGENTS.md](../../../crates/waltr-agent/tests/AGENTS.md) | Deterministic agent test boundaries and support structure. |
| [../../../crates/waltr-agent/tests/live/AGENTS.md](../../../crates/waltr-agent/tests/live/AGENTS.md) | Live provider and external service test boundaries. |
