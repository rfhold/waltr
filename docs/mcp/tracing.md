# MCP Tracing

The generic `mcp` crate emits spans for MCP client and server requests and can propagate W3C trace context through MCP request metadata.

## Propagation

Trace propagation is controlled by [`McpTracePropagation`](../../crates/mcp/src/trace.rs):

| Mode | Behavior |
| --- | --- |
| `Disabled` | Do not mutate MCP request params. This is the generic client default. |
| `TraceContext` | Inject the current span context into `params._meta.traceparent` and, when present, `params._meta.tracestate`. |

Injection preserves existing `_meta` values such as MCP progress tokens. If `_meta.traceparent` already exists, the helper does not overwrite it. `ping` requests do not receive injected trace context.

On the server side, inbound requests and notifications read `params._meta.traceparent` and `params._meta.tracestate`. Valid context becomes the remote parent for the server request span. Missing or invalid context is ignored.

## Client Span

Each typed client request runs inside an `mcp.client.request` span from [../../crates/mcp/src/client/connection.rs](../../crates/mcp/src/client/connection.rs).

| Field | Meaning |
| --- | --- |
| `rpc.system` | Always `jsonrpc`. |
| `rpc.jsonrpc.version` | Always `2.0`. |
| `rpc.method` | JSON-RPC method name. |
| `mcp.method` | MCP method name, currently the same value as `rpc.method`. |
| `mcp.request_id` | Generated JSON-RPC id after the request is created. |
| `mcp.transport` | `stdio` or `streamable_http`. |
| `mcp.trace_propagation` | `disabled` or `tracecontext`. |
| `mcp.outcome` | `success`, `error`, `cancelled`, or `timeout`. |
| `rpc.jsonrpc.error_code` | JSON-RPC error code when the response contains one. |

Stdio cancellation and timeout paths send `notifications/cancelled`. Streamable HTTP cancellation closes only the affected request response stream.

## Server Span

Each inbound server request or notification runs inside an `mcp.server.request` span from [../../crates/mcp/src/server/mod.rs](../../crates/mcp/src/server/mod.rs).

| Field | Meaning |
| --- | --- |
| `rpc.system` | Always `jsonrpc`. |
| `rpc.jsonrpc.version` | Always `2.0`. |
| `rpc.method` | JSON-RPC method name. |
| `mcp.method` | MCP method name, currently the same value as `rpc.method`. |
| `mcp.request_id` | Request id for requests, empty for notifications. |
| `mcp.transport` | `stdio` or `streamable_http`. |
| `mcp.protocol_version` | Protocol version from the request body's required `_meta` object. |
| `mcp.outcome` | `success` or `error` when the handler completes. |
| `rpc.jsonrpc.error_code` | JSON-RPC error code when the response contains one. |

Stdio request spans are spawned per request. Streamable HTTP request spans cover stateless metadata and mirrored-header validation, handler execution, response formatting, and any request-scoped SSE response body creation.

## Waltr Agent Integration

The generic crate does not decide whether Waltr sessions propagate MCP trace context. The agent integration maps `SessionRuntimeSettings::mcp_trace_propagation` to `McpTracePropagation::TraceContext`; server-resolved session runtime configs enable that setting for managed sessions.

Agent-specific MCP spans are documented in [../agent/otel.md](../agent/otel.md).
