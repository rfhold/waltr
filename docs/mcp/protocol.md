# MCP Protocol

The `mcp` package in [../../crates/mcp/](../../crates/mcp/) is Waltr's generic Model Context Protocol implementation. It owns protocol primitives, typed client/server APIs, transports, request/response parsing, capability helpers, and protocol-level tests.

Waltr-specific session behavior belongs outside this crate. Agent runtime adaptation is documented in [../agent/mcp.md](../agent/mcp.md).

## Boundaries

| Belongs in `mcp` | Belongs outside `mcp` |
| --- | --- |
| JSON-RPC message construction, parsing, request routing, and error conversion. | Waltr session runtime settings, model provider behavior, permissions, timeline events, and UI/server contracts. |
| Typed MCP client and server helpers. | Conversion of MCP tools/prompts/resources into Waltr agent tools or templates. |
| Stdio and Streamable HTTP transport behavior. | Waltr config schema, server-side config validation, and resolved session defaults. |
| Protocol-level trace propagation helpers. | Decisions about whether a Waltr session enables MCP trace propagation. |
| Spec and ecosystem tests for protocol compatibility. | Layer-specific agent, server, client, web, or TUI behavior. |

The crate must not depend on `waltr-agent`, `waltr-server`, `waltr-client`, `waltr-tui`, or `waltr-web`.

## Public Surface

| Module | Purpose |
| --- | --- |
| [`client`](../../crates/mcp/src/client/) | Typed MCP client, connection loop, handler callbacks, and transports. |
| [`server`](../../crates/mcp/src/server/mod.rs) | Typed MCP server handlers, server context, stdio serving, and Streamable HTTP router. |
| [`protocol`](../../crates/mcp/src/protocol.rs) | JSON-RPC helpers, protocol version, cancellation helpers, and parse/format utilities. |
| [`capabilities`](../../crates/mcp/src/capabilities.rs) | Capability parsing for initialized peers. |
| [`tool`](../../crates/mcp/src/tool.rs) | Tool definitions, calls, result formatting, and schemas. |
| [`prompt`](../../crates/mcp/src/prompt.rs) | Prompt definitions, arguments, and result formatting. |
| [`resource`](../../crates/mcp/src/resource.rs) | Resource definitions, templates, URI matching, and result formatting. |
| [`log`](../../crates/mcp/src/log.rs) | MCP log level types. |
| [`trace`](../../crates/mcp/src/trace.rs) | Request `_meta` trace propagation helpers. |

The crate exports these protocol types through [../../crates/mcp/src/lib.rs](../../crates/mcp/src/lib.rs) so consumers can use a typed API without reaching into implementation modules unnecessarily.

## Client Behavior

`McpClient::connect` starts a configured transport, sends `initialize` with protocol version `2025-06-18`, verifies the server protocol version, stores advertised capabilities and optional server `instructions`, marks the transport initialized, and sends `notifications/initialized`.

After initialization, typed helpers send protocol requests for tools, prompts, resources, completions, logging, ping, and listing operations. Requests are wrapped by `mcp.client.request` spans; trace propagation is optional and documented in [tracing.md](tracing.md).

The client handler callback surface currently covers roots, list-changed notifications, logging notifications, progress notifications, transport disconnection, and Streamable HTTP SSE event ids observed from POST responses.

## Server Behavior

The typed server API exposes `ServerHandler` methods for initialize, ping, tool listing/calls, prompt listing/get, resource listing/reading, resource-template listing, completions, logging-level updates, and notifications. Handler errors convert to JSON-RPC error responses.

Servers can provide initialize-time instructions with `ServerHandler::instructions`. Empty instructions are omitted from the initialize result. Clients can read non-empty initialize instructions with `McpClient::instructions`.

Server transports create `mcp.server.request` spans for inbound requests and notifications. When a request includes valid trace context in `params._meta`, the server span uses that context as its remote parent.

Server capabilities advertise only implemented behavior. Resource support advertises `subscribe: false` and `listChanged: false`; subscriptions are intentionally not presented as supported.

## Transports

| Transport | Current behavior |
| --- | --- |
| Stdio client | Starts a subprocess, writes line-delimited JSON-RPC to stdin, reads line-delimited JSON-RPC from stdout, and routes inbound responses, notifications, requests, and disconnects through the connection loop. |
| Streamable HTTP client | Sends JSON-RPC over HTTP POST, accepts JSON or `text/event-stream` POST responses, tracks `mcp-session-id`, sends `mcp-protocol-version` after initialization, and records SSE event ids observed in POST responses. |
| Stdio server | Reads line-delimited JSON-RPC from stdin and writes line-delimited JSON-RPC responses to stdout. |
| Streamable HTTP server | Serves POST requests at `/mcp`, manages initialize/session state, returns JSON responses or SSE POST responses when notifications are emitted with the response. |

The current Streamable HTTP implementation handles request/response traffic and SSE responses to POST requests. It does not yet open an optional long-lived GET SSE stream, resume with `Last-Event-ID`, or terminate sessions with DELETE.

## Unsupported Areas

Unsupported protocol areas should stay explicit in code, docs, and tests. Current intentionally unsupported or partial areas include:

- Resource subscriptions.
- Sampling.
- Elicitation.
- Streamable HTTP long-lived GET SSE streams.
- Streamable HTTP `Last-Event-ID` resume requests.
- Streamable HTTP DELETE session termination.

Do not add placeholder success behavior for unsupported features. Prefer explicit unsupported coverage in spec tests.

## Tests

MCP tests are split by purpose:

| Path | Covers |
| --- | --- |
| [../../crates/mcp/tests/spec.rs](../../crates/mcp/tests/spec.rs) and [../../crates/mcp/tests/spec/](../../crates/mcp/tests/spec/) | Revision-scoped protocol conformance tests. |
| [../../crates/mcp/tests/ecosystem.rs](../../crates/mcp/tests/ecosystem.rs) and [../../crates/mcp/tests/ecosystem/](../../crates/mcp/tests/ecosystem/) | Interoperability against ecosystem clients and servers such as `rmcp`. |

`rmcp` is a dev-dependency used for ecosystem compatibility tests. It should not be introduced into runtime/library code.

See [../testing.md](../testing.md) for canonical commands.
