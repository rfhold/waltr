---
name: mcp-modification
description: Helps an agent modify Waltr's generic Model Context Protocol crate when the user asks to change MCP protocol types, JSON-RPC behavior, typed client/server APIs, transports, trace propagation, spec tests, or ecosystem interoperability.
---

# MCP Modification

Treat `crates/mcp/` as a generic protocol crate, not as a Waltr agent feature module.

## When To Use

Use this skill when changing `crates/mcp/`, MCP protocol types, JSON-RPC parsing or formatting, typed MCP client/server APIs, stdio or Streamable HTTP transport behavior, MCP trace propagation, spec conformance tests, or ecosystem interoperability tests.

Waltr host adaptation belongs to the server-modification skill, including permissions, templates, Skills, Apps, upstream credentials, live interactions, and runtime assembly.
Session settings, server config, and UI behavior remain outside this skill unless the generic MCP crate itself must change.

## Core Boundary

The `mcp` package owns reusable MCP protocol behavior. It should not depend on `waltr-agent`, `waltr-server`, `waltr-client`, `waltr-tui`, or `waltr-web`. Waltr layers may depend on `mcp` and adapt its typed APIs into session, server, client, or UI behavior.

## Modification Patterns

1. Decide whether the behavior is generic MCP protocol behavior or Waltr integration behavior.
2. Keep protocol types, JSON-RPC helpers, client/server APIs, transports, and trace propagation in `crates/mcp/`.
3. Keep Waltr tool/template conversion and runtime adaptation in `crates/waltr-server/src/agent_host/mcp/`.
4. Keep server-side config validation and resolved runtime defaults in `crates/waltr-server/`.
5. Keep `rmcp` as a dev-dependency used only by ecosystem tests.
6. Preserve typed APIs instead of leaking raw `serde_json::Value` when a stable MCP concept already exists.
7. Keep unsupported protocol areas explicit. Do not add placeholder success behavior beyond the implemented capabilities.
8. Leave trace propagation disabled by default in the generic client unless a caller explicitly enables it.
9. Preserve typed elicitation and exact MRTR continuations. Hosts own authority, live waiters, and single-use enforcement, not the generic protocol crate.

## Test Selection

- Use `cargo test -p mcp --test spec` for revision-scoped protocol conformance.
- Use `cargo test -p mcp --tests` for default-feature integration tests, including ecosystem interoperability.
- Use server host tests after generic behavior enters Waltr runtime adaptation.
- Keep assertions at the protocol layer for generic behavior: JSON-RPC shape, capabilities, protocol errors, transport behavior, trace metadata, and typed result parsing.

## Commands

Run these commands from the repository root. Feature-gated suites require their explicit feature selections; see the canonical test guide.

Build the MCP crate:

`cargo build -p mcp`

Run MCP spec tests:

`cargo test -p mcp --test spec`

Run default-feature MCP integration tests:

`cargo test -p mcp --tests`

## References

| Path | Info |
| --- | --- |
| [../../../docs/mcp/README.md](../../../docs/mcp/README.md) | Generic MCP docs index. |
| [../../../docs/mcp/protocol.md](../../../docs/mcp/protocol.md) | MCP crate boundaries, public surface, transports, unsupported areas, and tests. |
| [../../../docs/mcp/tracing.md](../../../docs/mcp/tracing.md) | MCP spans and trace propagation behavior. |
| [../../../docs/agent-host/mcp.md](../../../docs/agent-host/mcp.md) | Server host integration on top of the generic MCP crate. |
| [../../../crates/waltr-server/src/agent_host/mcp/](../../../crates/waltr-server/src/agent_host/mcp/) | Waltr-specific runtime adaptation. |
| [../../../docs/testing.md](../../../docs/testing.md) | Canonical layered test structure and commands. |
| [../../../crates/mcp/AGENTS.md](../../../crates/mcp/AGENTS.md) | Local MCP crate boundary guidance. |
| [../../../crates/mcp/src/lib.rs](../../../crates/mcp/src/lib.rs) | Public MCP exports. |
| [../../../crates/mcp/src/client/](../../../crates/mcp/src/client/) | Typed MCP client, connection loop, and transports. |
| [../../../crates/mcp/src/server/mod.rs](../../../crates/mcp/src/server/mod.rs) | Typed MCP server. |
| [../../../crates/mcp/src/protocol.rs](../../../crates/mcp/src/protocol.rs) | JSON-RPC and protocol helpers. |
| [../../../crates/mcp/src/trace.rs](../../../crates/mcp/src/trace.rs) | MCP trace context propagation helpers. |
| [../../../crates/mcp/tests/spec/README.md](../../../crates/mcp/tests/spec/README.md) | MCP spec test contract. |
