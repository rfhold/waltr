# MCP Server Support

## Goal

Add opt-in MCP server support to `waltr-agent` as its own module. MCP servers are initialized at agent build time, their tools are fetched dynamically with `tools/list`, and each discovered MCP tool is exposed directly to the model as a normal provider-facing tool named `mcp_<server>_<tool>`.

## Scope

- Add a top-level `mcp` module to `waltr-agent`.
- Support MCP protocol version `2025-06-18`.
- Support stdio transport.
- Support current Streamable HTTP transport, including SSE responses from POST requests.
- Dynamically fetch tool definitions from each MCP server during startup.
- Expose MCP tools directly as provider tools.
- Integrate MCP servers through `AgentBuilder`.
- Keep MCP disabled unless explicitly configured.
- Add deterministic SDK-backed agent integration coverage for both transports.
- Add one ignored live-provider agent integration test covering both transports.

## Non-Goals

- Do not implement deprecated legacy HTTP+SSE transport.
- Do not implement MCP roots unless needed by a future feature.
- Do not require live model/provider credentials for the default test suite.
- Do not make MCP tools available by default.
- Do not expose MCP tools behind generic meta-tools such as `call_mcp_tool`.
- Do not require users to predeclare tools with a `.with_tool(...)` option.

## Public API Shape

Add `crates/waltr-agent/src/mcp/mod.rs` and export it from `src/lib.rs` with `pub mod mcp`.

Initial public types:

- `McpConfig`
- `McpServerConfig`
- `McpTransportConfig`

Builder shape:

```rust
let config = McpConfig::new()
    .with_server(McpServerConfig::stdio(
        "local",
        command,
        args,
    ))
    .with_server(McpServerConfig::streamable_http(
        "remote",
        endpoint,
    ));

let agent = Agent::builder(model)
    .with_mcp_servers(config)
    .await?
    .build();
```

`McpServerConfig::with_tool(...)` should be removed. Tool definitions come from MCP `tools/list`, not user configuration.

## Agent Integration

Add async MCP startup to `AgentBuilder`:

```rust
impl AgentBuilder {
    pub async fn with_mcp_servers(mut self, config: McpConfig) -> Result<Self> {
        self.tools.extend(config.into_agent_tools().await?);
        Ok(self)
    }
}
```

The method should:

- Initialize each configured MCP server.
- Fetch and cache each server's tool list.
- Build MCP-backed `AgentTool`s from fetched MCP tool definitions.
- Keep MCP opt-in.
- Surface MCP tools through the existing provider-facing `ToolDefinition` path.
- Use existing permission-policy behavior by preserving MCP tool names as host tool names.

Existing builder methods can remain synchronous. Only `with_mcp_servers` needs to be async because MCP tool definitions are remote/runtime data.

## Async Tool Execution

MCP transport calls are async, while existing tools were originally synchronous. Refactor tool handling minimally:

- Change `AgentTool` internals so handlers return boxed futures.
- Preserve existing sync constructor behavior:
  - `AgentTool::new(...)`
  - `AgentTool::with_context(...)`
- Add async constructors:
  - `AgentTool::new_async(...)`
  - `AgentTool::with_context_async(...)`
- Make `AgentTool::call` async.
- Make agent-loop `execute_tool` async and await it when handling `CompletionEvent::ToolCall`.

This avoids blocking on Tokio handles and keeps existing filesystem, skills, shell, and custom sync tools source-compatible.

## MCP Lifecycle

Each configured server initializes during `with_mcp_servers(...).await`:

1. Send `initialize` with:
   - `protocolVersion: "2025-06-18"`
   - client capabilities
   - client info
2. Validate the initialize response.
3. Send `notifications/initialized`.
4. Call paginated `tools/list` and cache the discovered tools.
5. Build direct `AgentTool`s from the cached discovered tools.

Request handling should include:

- Per-request timeout, defaulting to a conservative value such as 30 seconds.
- JSON-RPC id correlation.
- JSON-RPC error mapping to `LlmError::Protocol` or `LlmError::Api` as appropriate.
- `tools/call` errors represented by `isError: true` should return a normal tool result with an MCP error prefix, not fail the whole agent loop.

## Stdio Transport

Implement stdio transport using `tokio::process::Command`.

Behavior:

- Spawn the configured subprocess.
- Write newline-delimited JSON-RPC messages to stdin.
- Read newline-delimited JSON-RPC messages from stdout.
- Treat stdout as protocol-only.
- Ignore or capture stderr as logs only.
- Match request responses by JSON-RPC id.
- Handle notifications without blocking the current request.
- Serialize requests per stdio connection because stdin/stdout share one ordered channel.

## Streamable HTTP Transport

Implement current MCP Streamable HTTP, not deprecated legacy HTTP+SSE.

Behavior:

- POST each JSON-RPC request to the configured endpoint.
- Send `Accept: application/json, text/event-stream`.
- Send `Content-Type: application/json`.
- After initialization, send `MCP-Protocol-Version: 2025-06-18`.
- Store `Mcp-Session-Id` returned by initialize and include it on later requests.
- Parse `application/json` responses directly.
- Parse `text/event-stream` responses until the matching JSON-RPC response id is found.
- Ignore unrelated notifications except for stateful notifications such as `notifications/tools/list_changed`.
- On HTTP 404 with a stored session id, clear session state and reinitialize once.

The GET event stream path is not required for initial tool-call support, but the implementation should be structured so it can be added later.

## Tool Conversion

MCP `tools/list` entries map to provider-facing `ToolDefinition` values.

Direct naming:

- Namespace tools to avoid collisions.
- Format: `mcp_<server>_<tool>`.
- Sanitize server and tool name parts for provider tool-name compatibility.
- Preserve original `{ server, tool }` mapping internally.

Descriptions should include the MCP server name and the MCP tool description.

Input schemas:

- Map MCP `inputSchema` directly to `ToolDefinition.input_schema`.
- If missing or invalid, use an object schema with no required properties.

Tool results:

- Join text content into plain text.
- Serialize `structuredContent` compactly as JSON.
- Use concise placeholders for unsupported non-text content.
- If `isError: true`, return successful host tool output prefixed as an MCP tool error.

## Cache And Refresh

Cache discovered tools per server after startup initialization.

If `notifications/tools/list_changed` is observed:

- Invalidate that server's tool cache.
- Do not change the provider-facing tool array during an in-flight agent run.
- Re-fetch tools for future agent builds or a future explicit refresh API.

Dynamic mutation of the tool array during a run is out of scope for the first implementation.

## Permissions

Use existing permission integration:

- The permission tool name is the direct namespaced MCP tool name.
- Example: `mcp_stdio_test_echo_marker`.

Follow-up improvement:

- Add MCP-specific permission claims that include server and original MCP tool names so policies can target `server/tool` independently of the sanitized provider-facing name.

## Test Plan

Add `crates/waltr-agent/tests/agent/mcp.rs` and wire it from `crates/waltr-agent/tests/agent.rs`.

Use one deterministic SDK-backed test to prove the MCP client can discover and call direct tools without relying on provider behavior:

```rust
#[tokio::test]
async fn mcp_agent_loop_discovers_and_calls_rmcp_stdio_and_streamable_http_direct_tools() -> waltr_agent::Result<()> {
    // ...
}
```

The deterministic test should:

- Start an `rmcp`-backed stdio MCP test server.
- Start an in-process `rmcp` Streamable HTTP MCP test server.
- Configure both servers without `.with_tool(...)` declarations.
- Build the agent with `.with_mcp_servers(config).await?`.
- Capture the provider-facing tool definitions sent to the model.
- Assert startup-discovered direct tools are surfaced:
  - `mcp_stdio_test_echo_marker`
  - `mcp_http_test_echo_marker`
- Assert meta-tools are not surfaced:
  - `search_mcp_tools`
  - `get_mcp_tool_details`
  - `call_mcp_tool`
- Script the deterministic provider to call both direct tools.
- Assert transcript tool calls and results exist for both MCP tools.
- Assert all expected marker results appear:
  - `stdio-mcp-tool-called-707`
  - `http-mcp-tool-called-707`
  - final answer contains `mcp-flow-complete`

Use one ignored live-provider test, matching the style of the permissions and context agent tests:

```rust
#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_mcp_agent_loop_covers_stdio_and_streamable_http_direct_tools() -> waltr_agent::Result<()> {
    // ...
}
```

The live test should:

- Start the same `rmcp`-backed stdio MCP test server.
- Start the same in-process `rmcp` Streamable HTTP MCP test server.
- Configure both servers without `.with_tool(...)` declarations.
- Build the agent with `.with_mcp_servers(config).await?`.
- Capture the provider-facing tool definitions sent to the model and include them in failure diagnostics.
- Select the live provider with `live_provider::selected_from_env("WALTR_AGENT_LIVE_PROVIDER", "openai_responses_websocket")`.
- Prompt the live provider to call both startup-discovered direct tools:
  - `mcp_stdio_test_echo_marker`
  - `mcp_http_test_echo_marker`
- Assert the final transcript contains tool calls and tool results for both direct MCP tools.
- Assert all expected marker results appear:
  - `stdio-mcp-tool-called-707`
  - `http-mcp-tool-called-707`
  - final answer contains `mcp-flow-complete`

## Test MCP Server

The test MCP server should use `rmcp` so the client is tested against SDK-backed MCP server behavior instead of handwritten JSON-RPC assumptions.

The server should support both transports.

Stdio server:

- Spawn the local helper binary `waltr-agent-test-mcp-server`.
- Serve via `rmcp::transport::stdio()`.

Streamable HTTP server:

- Run in-process with `axum` bound to `127.0.0.1:0`.
- Serve `rmcp::transport::streamable_http_server::StreamableHttpService` at `/mcp`.
- Use SDK default Streamable HTTP behavior, including SSE responses.

Both transports implement:

- `initialize`
- `notifications/initialized`
- `tools/list`
- `tools/call`

Test tool:

- MCP tool name: `echo_marker`
- Input schema accepts `{ "value": "string" }`.
- Result text includes the passed marker value.

## Dependencies

Update `crates/waltr-agent/Cargo.toml`:

- Add Tokio features needed by MCP:
  - `process`
  - `io-util`
  - `time`
  - `net` for tests if the package test target needs it

Add dev-dependencies for the integration test:

- `axum`

Add `rmcp` with server, macros, stdio, and Streamable HTTP server features for the SDK-backed test servers.

Existing `reqwest` streaming support should be enough for Streamable HTTP client parsing.

## Verification

Run:

```sh
cargo fmt -p waltr-agent
cargo test -p waltr-agent --test agent mcp_agent_loop_discovers_and_calls_rmcp_stdio_and_streamable_http_direct_tools
cargo test -p waltr-agent --test agent live_mcp_agent_loop_covers_stdio_and_streamable_http_direct_tools
cargo test -p waltr-agent --test agent live_mcp_agent_loop_covers_stdio_and_streamable_http_direct_tools -- --ignored
cargo test -p waltr-agent
cargo clippy -p waltr-agent --all-targets
```
