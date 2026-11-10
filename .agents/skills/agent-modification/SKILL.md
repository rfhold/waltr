---
name: agent-modification
description: Use when changes affect Waltr's generic agent loop, providers, inference auth, neutral tools/context/hooks, durable sessions/history, cancellation, completion, host fences, or agent tests.
---

# Agent Modification

Treat the agent as an isolated hostable library for model interaction, neutral host contracts, and durable sessions.

## When To Use

Use this skill for `crates/waltr-agent/`, provider adapters, inference auth, neutral tool/context/hooks, prompt assembly, sessions, timeline events, or agent tests.

For concrete capabilities, definitions, templates, Skills, MCP, Apps, worker integration, permissions, or live interactions, use the server-modification skill.
Host guidance lives in [docs/agent-host/](../../../docs/agent-host/README.md).
Client reducers and UI behavior remain outside this library.

For generic MCP protocol, JSON-RPC, client/server APIs, transports, trace propagation, spec tests, or ecosystem interoperability, use the mcp-modification skill.

## Core Boundary

`waltr-agent` owns provider-neutral requests, events, tool contracts, context/hooks, inference auth, history, coordination, cancellation, completion, and durable host fences/receipts.
Definitions remain opaque JSON here. The crate has no built-in tools, permission engine, MCP dependency, shell feature, or worker implementation.
Do not add dependencies on `waltr-server`, `waltr-client`, `waltr-tui`, or `waltr-web`.

The server owns upstream credentials. Its `CredentialStore::provider_store()` adapter supplies provider-only credentials to the generic agent.
The server also owns concrete runtime assembly and private live interactions. Hosts own APIs, UI presentation, and process lifecycle.

## Modification Patterns

1. Decide whether the behavior belongs to the hostable agent library or to a host layer.
2. Keep public host contracts in `lib.rs`, `agent/`, `stream.rs`, `timeline.rs`, and provider-neutral types.
3. Route provider-specific protocol behavior and continuation metadata through `provider/` adapters. Preserve provider-neutral request and event semantics.
4. Keep the loop responsible for turn progression, provider streams, neutral tool callbacks, timeline events, and session journal persistence.
5. Keep neutral tools opt-in through `AgentRuntimeBuilder::with_tool`; a default runtime has no concrete capabilities.
6. Route concrete policy and claim checks through the server's shared `CapabilityDispatcher` before effects.
7. Preserve deterministic hook order; it affects prompt construction, tool visibility, and provider request shape.
8. Base durable session replay on persisted timeline and journal records, not provider-specific transient parser state.
9. Keep generic MCP in `crates/mcp/`; keep Waltr adaptation in `crates/waltr-server/src/agent_host/mcp/`.
10. Keep deterministic tests on scripted providers and shared support; reserve real provider and external service behavior for ignored live tests.
11. Persist the neutral host dispatch fence before the agent polls an effect. Never automatically replay an unknown dispatch.
12. Keep private approvals and pending interactions outside metadata, journal, timeline, and generic resume input.
13. Preserve generic interruption and tool-result continuation; these do not restore private approvals after restart.
14. Keep session/item metadata opaque; evaluate revision, context, incarnation and metadata predicates with dependent masks in one transaction.
15. Preserve absent versus null and canonical item annotations across supported copies/projections; never rewrite raw provider evidence.
16. Keep category schemas, checkpoints and background policy in Routine JavaScript. Keep helper/task classification in trusted server-local roles.
17. Keep public record revisions separate from incarnation-bound execution tokens; retain strict public CAS and context predicates.
18. Adopt coherent append receipts before publication; never guess public revisions or reload to reconstruct append results.
19. Preserve legacy JSON through read-only sequence seeding and the next authorized write; never invent incarnations or reset stores.

See [revision and compatibility contracts](../../../docs/agent/storage.md#revision-domains-and-append-receipts) for mutation classification and coordinated writer requirements.

## Test Selection

- Use `cargo test -p waltr-agent --lib` for builders, providers, neutral hooks, stream parsers, sessions, and host fences.
- Use `cargo test -p waltr-agent --test agent` for deterministic loop, session, completion, context, and storage behavior.
- Use `cargo test -p waltr-agent --test live -- --ignored` for real provider and external storage coverage.
- Use `cargo test -p waltr-server --lib host_tests` for concrete host capability tests.
- Keep agent assertions provider-neutral: requests, events, timeline output, session records, interruption, tool results, and public contracts.

## Commands

Run these commands from the repository root. Ignored live tests require their documented credentials and services.

Run agent unit tests:

`cargo test -p waltr-agent --lib`

Run deterministic agent integration tests:

`cargo test -p waltr-agent --test agent`

Run ignored live agent tests:

`cargo test -p waltr-agent --test live -- --ignored`

## References

| Path | Info |
| --- | --- |
| [../../../docs/architecture/agent.md](../../../docs/architecture/agent.md) | Canonical generic runner and server host boundary. |
| [../../../docs/agent-host/README.md](../../../docs/agent-host/README.md) | Concrete server capability guides. |
| [../../../docs/agent-host/mcp.md](../../../docs/agent-host/mcp.md) | Server-owned MCP integration and generic MCP handoff. |
| [../../../docs/mcp/protocol.md](../../../docs/mcp/protocol.md) | Generic MCP crate behavior and boundaries. |
| [../../../docs/testing.md](../../../docs/testing.md) | Canonical layered test structure, commands, live-test setup, and container-backed test notes. |
| [../../../crates/waltr-agent/AGENTS.md](../../../crates/waltr-agent/AGENTS.md) | Agent crate isolation boundary and dependency contract. |
| [../../../crates/waltr-agent/src/lib.rs](../../../crates/waltr-agent/src/lib.rs) | Public exports and module boundary. |
| [../../../crates/waltr-agent/src/agent/builder.rs](../../../crates/waltr-agent/src/agent/builder.rs) | Neutral runtime construction and tool/context injection. |
| [../../../crates/waltr-agent/src/agent/loop.rs](../../../crates/waltr-agent/src/agent/loop.rs) | Generic run loop, callbacks, host fences, timeline events, and persistence. |
| [../../../crates/waltr-agent/src/agent/session/](../../../crates/waltr-agent/src/agent/session/) | Durable sessions, snapshots, interruption, tool-result continuation, and storage backends. |
| [../../../crates/waltr-agent/src/stream.rs](../../../crates/waltr-agent/src/stream.rs) | Provider-neutral request, event, turn, tool, thinking, and metadata types. |
| [../../../crates/waltr-agent/src/provider/](../../../crates/waltr-agent/src/provider/) | Provider adapter implementations and provider-specific protocol handling. |
| [../../../crates/waltr-agent/src/middleware/mod.rs](../../../crates/waltr-agent/src/middleware/mod.rs) | Neutral request/context hooks. |
| [../../../crates/waltr-server/src/agent_host/](../../../crates/waltr-server/src/agent_host/) | Concrete capabilities, templates, middleware, MCP, and worker gateway. |
| [../../../crates/waltr-server/src/permission/live.rs](../../../crates/waltr-server/src/permission/live.rs) | Private policy receipts and live approval waits. |
| [../../../crates/waltr-server/src/auth/credentials.rs](../../../crates/waltr-server/src/auth/credentials.rs) | Server credential owner and provider-only adapter. |
| [../../../crates/mcp/](../../../crates/mcp/) | Independent generic MCP protocol crate. |
| [../../../crates/waltr-agent/tests/AGENTS.md](../../../crates/waltr-agent/tests/AGENTS.md) | Deterministic agent test boundaries and support structure. |
| [../../../crates/waltr-agent/tests/live/AGENTS.md](../../../crates/waltr-agent/tests/live/AGENTS.md) | Live provider and external service test boundaries. |
