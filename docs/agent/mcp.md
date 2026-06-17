# MCP

Model Context Protocol support lets an agent expose tools from configured MCP servers through the normal agent tool path. The implementation lives in `crates/waltr-agent/` and is configured directly through the agent builder.

## Entry Points

| API | Purpose |
| --- | --- |
| `McpConfig` | Collection of MCP servers plus request timeout. |
| `McpServerConfig` | One named server and its transport configuration. |
| `McpTransportConfig` | Agent-supported MCP transport configuration. |
| `AgentBuilder::with_mcp_servers` | Initializes configured servers, discovers tools, and adds them to the agent. |

`with_mcp_servers` is async because it connects to each server and lists tools during agent construction.

## Transports

| Transport | Behavior |
| --- | --- |
| Stdio | Starts a persistent subprocess with a command and args, then sends line-delimited JSON-RPC. |
| Streamable HTTP | Sends JSON-RPC over HTTP POST, accepts JSON or `text/event-stream`, and tracks `mcp-session-id`. |

Agent-side MCP config currently supports these transport shapes directly. The shared workspace config schema has its own MCP server representation; this document only describes the agent library boundary.

## Tool Discovery

For each configured server, the agent runtime:

1. Opens the transport.
2. Sends `initialize` with protocol version `2025-06-18`.
3. Sends `notifications/initialized`.
4. Lists tools, following cursors until all pages are loaded.
5. Converts each MCP tool into an `AgentTool`.

MCP tools are exposed directly. Tool names are generated as:

```text
mcp_<server>_<tool>
```

Non-alphanumeric characters are sanitized so the name is provider-safe.

## Tool Calls

When a provider calls an MCP-backed tool, the agent uses the normal tool execution path:

- The tool call and result are recorded as generic timeline events and journal-backed session projection data.
- Permission behavior can target the generated tool name through normal tool selectors.
- The MCP runtime sends `tools/call` to the server.
- The response is converted into provider-neutral `ToolResult` text.

MCP result formatting includes text content, unsupported-content markers, `structuredContent`, and MCP `isError` status.

## Boundaries

- MCP support is opt-in through `AgentBuilder::with_mcp_servers`.
- MCP resources, prompts, subscriptions, and sampling are not implemented.
- Generic MCP meta-tools such as `search_mcp_tools` or `call_mcp_tool` are not exposed; discovered MCP tools are exposed directly.
- Stdio env/cwd configuration and HTTP auth/header configuration are not implemented in the agent MCP config.
- I did not find code that converts shared server config MCP entries into `McpConfig`; current agent MCP usage is direct library configuration.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/tests/agent/mcp.rs](../../crates/waltr-agent/tests/agent/mcp.rs) | Deterministic stdio and streamable HTTP MCP tool flows. |
| [../../crates/waltr-agent/tests/live/agent/mcp.rs](../../crates/waltr-agent/tests/live/agent/mcp.rs) | Ignored live-provider MCP coverage. |
| [../../crates/waltr-agent/src/bin/waltr-agent-test-mcp-server.rs](../../crates/waltr-agent/src/bin/waltr-agent-test-mcp-server.rs) | Test MCP server binary. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/mcp/config.rs](../../crates/waltr-agent/src/mcp/config.rs) | MCP configuration types. |
| [../../crates/waltr-agent/src/mcp/runtime.rs](../../crates/waltr-agent/src/mcp/runtime.rs) | Tool discovery and `AgentTool` conversion. |
| [../../crates/waltr-agent/src/mcp/server.rs](../../crates/waltr-agent/src/mcp/server.rs) | MCP initialize, list-tools, and call-tool behavior. |
| [../../crates/waltr-agent/src/mcp/name.rs](../../crates/waltr-agent/src/mcp/name.rs) | Generated tool-name sanitization. |
| [../../crates/waltr-agent/src/mcp/tool.rs](../../crates/waltr-agent/src/mcp/tool.rs) | MCP response formatting. |
| [../../crates/waltr-agent/src/mcp/transport/stdio.rs](../../crates/waltr-agent/src/mcp/transport/stdio.rs) | Stdio transport. |
| [../../crates/waltr-agent/src/mcp/transport/http.rs](../../crates/waltr-agent/src/mcp/transport/http.rs) | Streamable HTTP transport. |
