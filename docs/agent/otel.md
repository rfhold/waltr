# Agent OpenTelemetry

Agent instrumentation uses `tracing` spans and fields that can be exported through OpenTelemetry when the host process installs an OTLP-compatible subscriber/exporter. The agent crate emits spans; process-level exporter setup belongs to the host binary.

## Turn Spans

Each agent turn runs inside an `agent.turn` span from [../../crates/waltr-agent/src/agent/loop.rs](../../crates/waltr-agent/src/agent/loop.rs).

| Field | Meaning |
| --- | --- |
| `session_id` | Durable session id. |
| `turn` | Turn index for the session run. |
| `provider` | Provider adapter name. |
| `model_provider` | Provider family from the selected model metadata. |
| `model_api` | Model API family from the selected model metadata. |
| `model` | Provider model id. |
| `terminal_state` | Recorded when the turn closes. |

The turn span logs `agent turn started` when created and `agent turn completed` with the terminal state when closed.

## Provider Request Spans

Each provider call created by the agent loop runs inside an `agent.provider_request` span.

| Field | Meaning |
| --- | --- |
| `session_id` | Durable session id. |
| `turn` | Turn index. |
| `provider` | Provider adapter name. |
| `model_provider` | Provider family. |
| `model_api` | Model API family. |
| `model` | Provider model id. |
| `tool_count` | Number of tools visible to the provider request. |
| `turn_count` | Number of provider-neutral turns in the request. |

Provider adapters add transport-level spans around the outbound call:

| Span | Used by |
| --- | --- |
| `http.client.request` | Anthropic Messages and OpenAI HTTP streaming providers. |
| `websocket.client.connect` | OpenAI Responses WebSocket connection setup. |
| `websocket.client.send` | OpenAI Responses WebSocket request send. |
| `provider.stream_body` | Provider response stream parsing. |

HTTP spans record fields such as `http_request_method`, `server_address`, `provider`, `model` when available, and `http_response_status_code` after the response is received.

## Tool Spans

Each tool future executed by the agent loop runs inside an `agent.tool_call` span.

| Field | Meaning |
| --- | --- |
| `session_id` | Durable session id. |
| `turn` | Turn index. |
| `tool_name` | Provider-facing tool name. |
| `tool_call_id` | Provider-neutral tool call id. |

Tool spans are children of the active `agent.turn` span when a turn span exists.

## MCP Integration Spans

Waltr MCP integration wraps operations that adapt MCP calls into agent tools and templates in `mcp.agent.operation` spans from [../../crates/waltr-agent/src/mcp/server.rs](../../crates/waltr-agent/src/mcp/server.rs).

| Operation | Fields |
| --- | --- |
| `tools/call` | `mcp.server`, `mcp.transport`, `rpc.method`, `mcp.tool.name`. |
| `prompts/get` | `mcp.server`, `mcp.transport`, `rpc.method`, `mcp.prompt.name`. |
| `resources/read` | `mcp.server`, `mcp.transport`, `rpc.method`, `mcp.resource.uri`. |
| `completion/complete` | `mcp.server`, `mcp.transport`, `rpc.method`, `mcp.resource_template.uri_template`. |

The generic MCP crate also emits `mcp.client.request` and `mcp.server.request` spans and can propagate W3C trace context through MCP request `_meta`. See [../mcp/tracing.md](../mcp/tracing.md).

## Boundaries

- The agent crate emits spans but does not own process-level OpenTelemetry exporter configuration.
- Provider request bodies are not recorded as span fields; durable request projections are persisted through session storage instead.
- Waltr's definition-owned MCP trace switch defaults to enabled. Explicit `tracePropagation: false` disables it. The generic MCP client default remains disabled.
