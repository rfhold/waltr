---
name: mcp-modification
description: Helps an agent modify Waltr's generic Model Context Protocol crate when the user asks to change MCP protocol types, JSON-RPC behavior, typed client/server APIs, transports, trace propagation, spec tests, or ecosystem interoperability.
---

# MCP Modification

Treat `crates/mcp/` as a generic protocol crate, not as a Waltr agent feature module.

## When To Use

Use this skill when changing `crates/mcp/`, MCP protocol types, JSON-RPC parsing or formatting, typed MCP client/server APIs, stdio or Streamable HTTP transport behavior, MCP trace propagation, spec conformance tests, or ecosystem interoperability tests.

Do not use this skill for Waltr-side agent runtime adaptation, session settings, permission behavior, prompt/resource template middleware, server config validation, browser UI, or terminal UI unless the generic MCP crate itself must change.

## Core Boundary

The `mcp` package owns reusable MCP protocol behavior. It should not depend on `waltr-agent`, `waltr-server`, `waltr-client`, `waltr-tui`, or `waltr-web`. Waltr layers may depend on `mcp` and adapt its typed APIs into session, server, client, or UI behavior.

## Modification Patterns

1. Decide whether the behavior is generic MCP protocol behavior or Waltr integration behavior.
2. Keep protocol types, JSON-RPC helpers, client/server APIs, transports, and trace propagation in `crates/mcp/`.
3. Keep Waltr agent tool/template conversion in `crates/waltr-agent/src/mcp/`.
4. Keep server-side config validation and resolved runtime defaults in `crates/waltr-server/`.
5. Keep `rmcp` as a dev-dependency used only by ecosystem tests.
6. Preserve typed APIs instead of leaking raw `serde_json::Value` when a stable MCP concept already exists.
7. Keep unsupported protocol areas explicit. Do not add placeholder success behavior for subscriptions, sampling, elicitation, or incomplete Streamable HTTP behavior.
8. Leave trace propagation disabled by default in the generic client unless a caller explicitly enables it.

## Test Selection

- Use `cargo test -p mcp --test spec` for revision-scoped protocol conformance.
- Use `cargo test -p mcp --tests` for all MCP integration tests, including ecosystem interoperability.
- Use agent tests only after generic behavior is adapted into Waltr agent runtime behavior.
- Keep assertions at the protocol layer for generic behavior: JSON-RPC shape, capabilities, protocol errors, transport behavior, trace metadata, and typed result parsing.

## Commands

Build the MCP crate:

`cargo build -p mcp`

Run MCP spec tests:

`cargo test -p mcp --test spec`

Run all MCP tests:

`cargo test -p mcp --tests`

## References

| Path | Info |
| --- | --- |
| [../../../docs/mcp/README.md](../../../docs/mcp/README.md) | Generic MCP docs index. |
| [../../../docs/mcp/protocol.md](../../../docs/mcp/protocol.md) | MCP crate boundaries, public surface, transports, unsupported areas, and tests. |
| [../../../docs/mcp/tracing.md](../../../docs/mcp/tracing.md) | MCP spans and trace propagation behavior. |
| [../../../docs/agent/mcp.md](../../../docs/agent/mcp.md) | Waltr agent integration on top of the generic MCP crate. |
| [../../../docs/testing.md](../../../docs/testing.md) | Canonical layered test structure and commands. |
| [../../../crates/mcp/AGENTS.md](../../../crates/mcp/AGENTS.md) | Local MCP crate boundary guidance. |
| [../../../crates/mcp/src/lib.rs](../../../crates/mcp/src/lib.rs) | Public MCP exports. |
| [../../../crates/mcp/src/client/](../../../crates/mcp/src/client/) | Typed MCP client, connection loop, and transports. |
| [../../../crates/mcp/src/server/mod.rs](../../../crates/mcp/src/server/mod.rs) | Typed MCP server. |
| [../../../crates/mcp/src/protocol.rs](../../../crates/mcp/src/protocol.rs) | JSON-RPC and protocol helpers. |
| [../../../crates/mcp/src/trace.rs](../../../crates/mcp/src/trace.rs) | MCP trace context propagation helpers. |
| [../../../crates/mcp/tests/spec/README.md](../../../crates/mcp/tests/spec/README.md) | MCP spec test contract. |
