# Audience

Tests here serve generic MCP protocol consumers and ecosystem compatibility, not Waltr agent/session behavior.

# Boundaries

| Path | Info |
| --- | --- |
| [spec/](spec/) | Revision-scoped MCP protocol conformance tests. |
| [ecosystem/](ecosystem/) | Interoperability tests against other MCP implementations. |

# Contracts

- Keep spec tests grouped under the single supported protocol revision, currently `spec/v2025_11_25/`.
- Keep shared spec helpers minimal and protocol-focused.
- Use ecosystem tests for compatibility with crates such as `rmcp`; do not use ecosystem dependencies in runtime code.
- Preserve explicit coverage for unsupported areas such as subscriptions, sampling, and elicitation.

# Hints

Run focused spec coverage with `cargo test -p mcp --test spec`.
Run all MCP integration tests with `cargo test -p mcp --tests`.
