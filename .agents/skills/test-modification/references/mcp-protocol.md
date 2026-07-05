# MCP Protocol Tests

Use MCP protocol tests when changing the generic `mcp` crate, protocol types, JSON-RPC behavior, typed client/server APIs, transports, trace propagation, or interoperability with other MCP implementations.

# Boundaries

| Path | Intent |
| --- | --- |
| `crates/mcp/tests/spec.rs` and `crates/mcp/tests/spec/` | Protocol conformance tests grouped by spec revision. |
| `crates/mcp/tests/ecosystem.rs` and `crates/mcp/tests/ecosystem/` | Interoperability tests against other MCP clients and servers such as `rmcp`. |
| `crates/waltr-agent/tests/agent/mcp.rs` | Waltr agent integration on top of the generic MCP crate. |

# Expectations

Start with spec tests for generic protocol behavior. Add ecosystem tests when the change affects compatibility with external MCP implementations. Add agent tests only when the generic behavior is adapted into Waltr session tools, prompt templates, resource templates, progress, roots, permissions, or runtime settings.

Keep unsupported protocol areas explicit. Current intentionally unsupported or partial areas include resource subscriptions, sampling, elicitation, Streamable HTTP long-lived GET SSE streams, `Last-Event-ID` resume requests, and DELETE session termination.

Do not use `rmcp` outside MCP ecosystem tests.

# Commands

Run MCP spec tests:

`cargo test -p mcp --test spec`

Run all MCP integration tests:

`cargo test -p mcp --tests`
