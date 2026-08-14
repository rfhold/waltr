# MCP Tracing

The generic `mcp` crate emits spans and native metrics for MCP server dispatch, emits spans for MCP client requests, and can propagate W3C trace context through MCP request metadata. It does not install an OpenTelemetry SDK, exporter, meter provider, or tracing subscriber.

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
| `otel.kind` | Always `server`. |
| `rpc.system` | Always `jsonrpc`. |
| `rpc.jsonrpc.version` | Always `2.0`. |
| `rpc.method` | JSON-RPC method name. |
| `mcp.method` | MCP method name, currently the same value as `rpc.method`. |
| `mcp.request_id` | Request id for requests, empty for notifications. |
| `mcp.transport` | `stdio` or `streamable_http`. |
| `mcp.protocol_version` | Protocol version from the request body's required `_meta` object. |
| `mcp.outcome` | `success`, `error`, or `cancelled`. Notifications record `success` after their handler completes. |
| `otel.status_code` | `ERROR` for JSON-RPC errors and cancellation. |
| `rpc.jsonrpc.error_code` | JSON-RPC error code when the response contains one. |

Server spans begin only after transport parsing, JSON-RPC and required MCP metadata validation, and, for Streamable HTTP, origin, content negotiation, authorization, mirrored-header, method-availability, and tool-parameter-header checks succeed. Rejections before that accepted-dispatch boundary do not create server request spans or metrics. Stdio requests are dispatched in independent tasks; valid stdio notifications are handled inline. The stdio `notifications/cancelled` message is transport control handled before accepted handler dispatch, so it emits no `mcp.server.request` span or metrics. Metadata-valid HTTP notifications with that method can still dispatch through the generic handler and therefore retain the bounded metric-method allowlist entry. Streamable HTTP requests and notifications are dispatched in independent tasks.

A tool result containing `isError: true` remains a `success` outcome because the JSON-RPC response itself succeeded.

## Server Metrics

The server creates instruments once per Streamable HTTP router or stdio/stream serve invocation. Instrument creation therefore occurs after a host has an opportunity to install its global meter provider. With no provider installed, the OpenTelemetry API remains a valid no-op.

| Instrument | Type | Unit | Meaning |
| --- | --- | --- | --- |
| `mcp.server.request.count` | `u64` counter | `{request}` | Completed accepted requests and notifications. |
| `mcp.server.request.duration` | `f64` histogram | `s` | Dispatch duration in seconds. |
| `mcp.server.request.in_flight` | `i64` up/down counter | `{request}` | Accepted dispatches currently in flight. |

All three instruments use bounded `rpc.method`, `mcp.transport`, and `mcp.protocol_version` attributes. Completed count and duration points also use `mcp.outcome`; JSON-RPC errors may add the integer `rpc.jsonrpc.error_code`. Unknown or extension methods are normalized to `other` for metrics while span fields retain exact protocol values. Unsupported protocol versions are rejected before telemetry. Version normalization to `other` is defensive for any accepted noncanonical value; it does not mean rejected versions emit spans or metrics.

Metrics never include request IDs, tool names or arguments, URLs, credentials, user identity, arbitrary error text, or application-domain values. A drop-safe idempotent lifecycle guard balances in-flight and records count and duration exactly once for normal responses, notification completion, JSON-RPC errors, cancellation, authorization lease end, response-stream disconnect, and handler task abort.

## Waltr Agent Integration

The generic crate does not decide whether Waltr sessions propagate MCP trace context. Its default remains `McpTracePropagation::Disabled`. Waltr owns a separate definition switch at `FeatureSet.spec.mcp.tracePropagation`. Waltr defaults that switch to `true`; explicit `false` preserves the generic disabled mode.

Agent-specific MCP spans are documented in [../agent/otel.md](../agent/otel.md).
