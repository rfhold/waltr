# Experimental MCP Events

The `mcp` crate supports the experimental `io.modelcontextprotocol/events` extension (draft SEP-3415).
The draft can change without compatibility guarantees.
Core MCP remains `2026-07-28` only; this extension adds no initialize handshake or protocol sessions.

## Wire contract

Both peers opt in through `capabilities.extensions["io.modelcontextprotocol/events"] = {}`.
Clients repeat their capabilities in every request's core metadata.

| Method | Parameters or result |
| --- | --- |
| `events/list` | Optional string `cursor`; result contains `events` and optional `nextCursor`. |
| `events/stream` | Required `name`; optional object `arguments`, string/null `cursor`, and unsigned integer `maxAgeMs`. |

Definitions contain `name` and optional `description`, `inputSchema`, `payloadSchema`, and string-array `delivery`.
The HTTP client mirrors the stream's `name` through `Mcp-Name`.
The HTTP server validates that header before it invokes the handler.

Every event notification carries the originating JSON-RPC request ID in `params._meta["io.modelcontextprotocol/subscriptionId"]`.
IDs retain their string or numeric type.

| Notification suffix under `notifications/events/` | Fields |
| --- | --- |
| `active` | Optional nullable `cursor`, optional `truncated` (default false). |
| `event` | `eventId`, `name`, `timestamp`, arbitrary JSON `data`, optional nullable `cursor`. |
| `heartbeat` | Optional nullable `cursor`. |
| `error` | Recoverable `error: {code, message, data?}`; the stream stays open. |
| `terminated` | Terminal `error: {code, message, data?}`; the client cancels the request. |

A final JSON-RPC success response ends the stream.
The application result is empty; modern server responses also contain standard `resultType` and server-info metadata.
The client accepts an empty `{}` final result from other implementations.
Cancellation emits `notifications/cancelled` with the original `requestId`.
HTTP cancellation also closes the request's response body.

The compatibility reference is OpenCode PR [63254](https://github.com/anomalyco/opencode/pull/63254),
commit `6b1db2759a1afac862c9c751ef82d7d7159f36cd`, particularly `packages/core/src/mcp/client.ts` and `packages/core/test/mcp-events.test.ts`.
That implementation also accepts `payloadSchema`; the typed definition retains it.
Its tests include legacy initialize fixtures; those fixtures do not define this crate's core protocol.

## Server API

Imports come from `mcp::{McpEventList, McpEventStreamRequest, McpEventMessage}` and `mcp::server`.
Servers explicitly advertise `ServerCapabilities::new().events()`.
Resources and tools remain independent capabilities, so resource-centric servers can also call `.resources().tools()`.

```rust,ignore
fn list_events(
    &self,
    cursor: Option<String>,
    context: ServerContext,
) -> BoxFuture<ServerResult<McpEventList>>;

fn stream_events(
    &self,
    request: McpEventStreamRequest,
    context: ServerContext,
) -> BoxFuture<ServerResult<()>>;
```

The producer emits typed notifications through `context.event(message).await`.
It emits `McpEventMessage::Active` first.
It emits heartbeats at least every thirty seconds; fifteen seconds provides margin for OpenCode's sixty-second idle timeout.
It returns `Ok(())` for graceful completion or `ServerError` for a JSON-RPC failure.
The transport drops the handler future when the request disconnects or loses authorization.
Any host work outside that future needs its own explicit cancellation and join contract.

The host owns filter validation, replay storage, cursor interpretation, replay gaps, permissions, and resource policy.
For example, a `mail.received` event can carry a resource URI instead of message content.
The generic crate does not fetch that resource or persist an event cursor.

## Client API

```rust,ignore
use mcp::{McpEventMessage, McpEventStreamRequest};
use mcp::client::McpClientOptions;

let options = McpClientOptions::new("consumer", "1", unary_timeout).with_events();
// Pass options to McpClient::connect, then:
let page = client.list_events(None, cancellation.clone()).await?;
let mut stream = client.stream_events(McpEventStreamRequest {
    name: "mail.received".into(),
    arguments: None,
    cursor: saved_cursor,
    max_age_ms: None,
}, cancellation).await?;
while let Some(message) = stream.recv().await {
    match message? {
        McpEventMessage::Event { data, cursor, .. } => consume(data, cursor),
        McpEventMessage::Terminated { error } => { report(error); break; }
        _ => {}
    }
}
stream.shutdown().await;
client.shutdown().await?;
```

The API exposes one discovery page per call; callers own pagination limits.
Streams disable only their unary request timeout, not other request timeouts.
Each stream has a 64-message consumer queue.
Overflow cancels that stream with a protocol error; it does not block other request dispatch.
Malformed notifications fail that stream with a protocol error.
`Terminated` returns one typed terminal message; the next `recv` returns `None`.
Sixty seconds of wire silence produces a protocol error and cancels the request, even without a consumer poll.
Each notification restarts the wire idle deadline.
`recv` also bounds each receive wait to sixty seconds.
Dropping a handle requests cancellation; `shutdown().await` explicitly joins its request task.
The shared transport retains its current receive limits and internal dispatch channel.

Server support works on stdio and real incremental HTTP response streams.
The typed client shares the same request-ID router across both transports.
Tests cover HTTP concurrency, incremental delivery beyond the unary timeout, final responses, validation, bounded-consumer cleanup, and stdio cancellation.
