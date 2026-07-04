# MCP

Model Context Protocol support lets a session expose tools and prompt templates from configured MCP servers. The implementation lives in `crates/waltr-agent/` and is owned by the durable session coordinator so MCP servers can remain initialized alongside a session.

## Entry Points

| API | Purpose |
| --- | --- |
| `McpConfig` | Collection of MCP servers plus request timeout. |
| `McpServerConfig` | One named server and its transport configuration. |
| `McpTransportConfig` | Agent-supported MCP transport configuration. |
| `SessionRuntimeSettings::mcp_servers` | Per-session MCP server snapshots used by `SessionCoordinator`. |
| `SessionCoordinator` | Starts and caches a session-scoped MCP runtime when resolving a session runtime. |

MCP is not attached directly to `AgentRuntimeBuilder`. Runtime construction receives tools and prompt-template middleware from the coordinator-owned MCP runtime for that session.

## Transports

| Transport | Behavior |
| --- | --- |
| Stdio | Starts a persistent subprocess with a command and args, keeps stdin/stdout open, and routes line-delimited JSON-RPC messages through a connection loop. |
| Streamable HTTP | Sends JSON-RPC over HTTP POST, accepts JSON or `text/event-stream`, tracks `mcp-session-id`, sends `mcp-protocol-version` after initialization, and records SSE event IDs seen in POST responses. |

The current Streamable HTTP client handles request responses and SSE responses to POST requests. It does not yet open the optional long-lived GET SSE stream or resume with `Last-Event-ID`.

## Tool Discovery

For each configured server, the session MCP runtime:

1. Opens the transport.
2. Sends `initialize` with protocol version `2025-06-18`.
3. Sends `notifications/initialized`.
4. Stores server capabilities from the initialize response.
5. Lists tools when the server advertises tool support, following cursors until all pages are loaded.
6. Stores tool metadata, including `inputSchema`, `outputSchema`, and annotations.
7. Converts the current tool registry into `AgentTool`s when an agent runtime is resolved.

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
- The MCP request includes a progress token.
- If the Waltr tool call is canceled or times out, the MCP runtime sends `notifications/cancelled` for the in-flight JSON-RPC request.
- The response is converted into provider-neutral `ToolResult` text.

MCP result formatting includes text content, unsupported-content markers, `structuredContent`, and MCP `isError` status.

`outputSchema` is preserved in the tool registry for future validation and presentation work. Waltr does not yet validate `structuredContent` against `outputSchema`.

## Prompt Discovery

For each configured server, the session MCP runtime also lists prompts when the server advertises prompt support. Prompt definitions are stored with their names, titles, descriptions, and argument metadata.

MCP prompts are surfaced through the normal session template search target for prompts. Templates use the server key and prompt name:

```text
/foo:bar
```

If the prompt declares required arguments, the inserted template includes JSON placeholders:

```text
/foo:bar({"name":""})
```

Prompt arguments follow the MCP `prompts/get` schema and must be a JSON object with string values. Invalid template arguments render a template error instead of sending a protocol-invalid MCP request.

Prompt names and server keys are inserted only when they can be represented by Waltr's slash-template syntax: ASCII alphanumeric characters, `_`, and `-`. Other spec-valid MCP prompt names remain registered but are not currently offered as slash-template search results.

When a user prompt starts with an MCP prompt template, the template middleware sends `prompts/get` to that server and inserts the returned prompt content before the provider request. Prompt content can contain nested template references, which are rendered by the normal template pipeline.

## Notifications and Server Requests

The MCP connection loop continuously routes inbound messages while a server is running.

| MCP message | Current behavior |
| --- | --- |
| `notifications/tools/list_changed` | Refreshes that server's tool registry in the background. Newly built agent runtimes see the refreshed tools. |
| `notifications/prompts/list_changed` | Refreshes that server's prompt registry in the background. Newly built agent runtimes and template searches see the refreshed prompts. |
| `notifications/message` | Publishes an internal MCP log event and writes a tracing event. |
| `notifications/progress` | Publishes an internal MCP progress event. |
| `roots/list` | Responds with the coordinator runtime root as a single `file://` root. |

The internal MCP event stream is not yet exposed as a public client or UI contract.

## Boundaries

- MCP support is opt-in through session runtime settings.
- MCP resources, subscriptions, sampling, elicitation, completion, and extension negotiation are not implemented.
- Generic MCP meta-tools such as `search_mcp_tools` or `call_mcp_tool` are not exposed; discovered MCP tools are exposed directly.
- Stdio env/cwd configuration and HTTP auth/header configuration are not implemented in the agent MCP config.
- The shared server config names the HTTP MCP server type `SSE`; session runtime settings map that to the agent's Streamable HTTP transport.
- HTTP GET SSE streams, `Last-Event-ID` resume requests, and DELETE session termination are not implemented yet.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/tests/agent/mcp.rs](../../crates/waltr-agent/tests/agent/mcp.rs) | Deterministic stdio and streamable HTTP MCP tool flows plus stdio MCP prompt template discovery and rendering. |
| [../../crates/waltr-agent/tests/live/agent/mcp.rs](../../crates/waltr-agent/tests/live/agent/mcp.rs) | Ignored live-provider MCP coverage. |
| [../../crates/waltr-agent/src/bin/waltr-agent-test-mcp-server.rs](../../crates/waltr-agent/src/bin/waltr-agent-test-mcp-server.rs) | Test MCP server binary. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/mcp/config.rs](../../crates/waltr-agent/src/mcp/config.rs) | MCP configuration types. |
| [../../crates/waltr-agent/src/mcp/runtime.rs](../../crates/waltr-agent/src/mcp/runtime.rs) | Session MCP runtime, `AgentTool` conversion, and prompt access. |
| [../../crates/waltr-agent/src/mcp/server.rs](../../crates/waltr-agent/src/mcp/server.rs) | MCP initialize, capabilities, tool and prompt registries, notifications, and request behavior. |
| [../../crates/waltr-agent/src/mcp/connection.rs](../../crates/waltr-agent/src/mcp/connection.rs) | JSON-RPC request routing, cancellation notifications, server requests, and notification dispatch. |
| [../../crates/waltr-agent/src/mcp/name.rs](../../crates/waltr-agent/src/mcp/name.rs) | Generated tool-name sanitization. |
| [../../crates/waltr-agent/src/mcp/tool.rs](../../crates/waltr-agent/src/mcp/tool.rs) | MCP response formatting. |
| [../../crates/waltr-agent/src/mcp/prompt.rs](../../crates/waltr-agent/src/mcp/prompt.rs) | MCP prompt definition parsing and prompt response formatting. |
| [../../crates/waltr-agent/src/mcp/transport/stdio.rs](../../crates/waltr-agent/src/mcp/transport/stdio.rs) | Stdio transport. |
| [../../crates/waltr-agent/src/mcp/transport/http.rs](../../crates/waltr-agent/src/mcp/transport/http.rs) | Streamable HTTP transport. |
