# MCP Protocol

The `mcp` package in [../../crates/mcp/](../../crates/mcp/) is Waltr's generic Model Context Protocol implementation. It owns protocol primitives, typed client/server APIs, transports, request/response parsing, capability helpers, and protocol-level tests. The crate supports a single active protocol revision, `2025-11-25`.

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
| [`oauth`](../../crates/mcp/src/oauth.rs) | HTTP transport authorization metadata, bearer challenges, and discovery helpers. |
| [`progressive`](../../crates/mcp/src/progressive.rs) | Feature-gated helpers for the opinionated progressive-workflow `exec` tool server pattern. |
| [`protocol`](../../crates/mcp/src/protocol.rs) | JSON-RPC helpers, protocol version, cancellation helpers, and parse/format utilities. |
| [`capabilities`](../../crates/mcp/src/capabilities.rs) | Capability parsing for initialized peers. |
| [`tool`](../../crates/mcp/src/tool.rs) | Tool definitions, calls, result formatting, and schemas. |
| [`prompt`](../../crates/mcp/src/prompt.rs) | Prompt definitions, arguments, and result formatting. |
| [`resource`](../../crates/mcp/src/resource.rs) | Resource definitions, templates, URI matching, and result formatting. |
| [`log`](../../crates/mcp/src/log.rs) | MCP log level types. |
| [`trace`](../../crates/mcp/src/trace.rs) | Request `_meta` trace propagation helpers. |

The crate exports these protocol types through [../../crates/mcp/src/lib.rs](../../crates/mcp/src/lib.rs) so consumers can use a typed API without reaching into implementation modules unnecessarily.

## Client Behavior

`McpClient::connect` starts a configured transport, sends `initialize` with protocol version `2025-11-25`, verifies the server protocol version, stores advertised capabilities and optional server `instructions`, marks the transport initialized, and sends `notifications/initialized`.

After initialization, typed helpers send protocol requests for tools, prompts, resources, completions, logging, ping, and listing operations. Requests are wrapped by `mcp.client.request` spans; trace propagation is optional and documented in [tracing.md](tracing.md).

The client handler callback surface currently covers roots, list-changed notifications, logging notifications, progress notifications, transport disconnection, and Streamable HTTP SSE event ids observed from POST responses or GET polling.

## Server Behavior

The typed server API exposes `ServerHandler` methods for initialize, ping, tool listing/calls, prompt listing/get, resource listing/reading, resource-template listing, completions, logging-level updates, and notifications. Handler errors convert to JSON-RPC error responses.

Servers can provide initialize-time instructions with `ServerHandler::instructions`. Empty instructions are omitted from the initialize result. Clients can read non-empty initialize instructions with `McpClient::instructions`.

Server and feature metadata supports 2025-11-25 baseline fields used by existing APIs: implementation `description`, implementation `icons`, tool/prompt/resource/resource-template `icons`, and tool `execution.taskSupport` metadata. JSON schemas follow the 2025-11-25 default dialect, JSON Schema 2020-12; callers may include explicit schema keywords in raw schema values.

Server transports create `mcp.server.request` spans for inbound requests and notifications. When a request includes valid trace context in `params._meta`, the server span uses that context as its remote parent.

Server capabilities advertise only implemented behavior. Resource support advertises `subscribe: false` and `listChanged: false`; subscriptions are intentionally not presented as supported.

Roots, sampling, and logging still exist in `2025-11-25`, but the MCP deprecation roadmap targets them for a future revision. Keep compatibility coverage for existing support, but do not expand those surfaces unless a concrete consumer needs it.

## Transports

| Transport | Current behavior |
| --- | --- |
| Stdio client | Starts a subprocess, writes line-delimited JSON-RPC to stdin, reads line-delimited JSON-RPC from stdout, and routes inbound responses, notifications, requests, and disconnects through the connection loop. |
| Streamable HTTP client | Sends JSON-RPC over HTTP POST, accepts JSON or `text/event-stream` POST responses, tracks `mcp-session-id`, sends `mcp-protocol-version` after initialization, records SSE event ids, starts GET polling after `notifications/initialized`, and can attach an optional bearer token to every HTTP request. |
| Stdio server | Reads line-delimited JSON-RPC from stdin and writes line-delimited JSON-RPC responses to stdout. |
| Streamable HTTP server | Serves POST, GET, and DELETE requests at `/mcp`, manages initialize/session state, returns JSON responses or SSE POST responses when notifications are emitted with the response, queues notification-only SSE messages for GET polling, supports opt-in Origin validation, and can publish OAuth protected-resource metadata plus validate bearer tokens through caller-provided hooks. |

Streamable HTTP GET requires `Accept: text/event-stream` and an initialized `mcp-session-id`. GET drains queued SSE messages and accepts `Last-Event-ID` to skip already-observed queued events. DELETE requires `mcp-session-id` and terminates the in-memory HTTP session. Servers can install an Origin policy with `StreamableHttpOptions::with_origin_policy`; invalid origins return HTTP `403 Forbidden` before JSON-RPC handling.

## Authorization

Authorization support is scoped to HTTP transports, matching MCP `2025-11-25`. Stdio remains unauthenticated at the protocol layer and should obtain credentials from the calling environment if a consumer needs that behavior.

The `oauth` module represents RFC 9728 protected resource metadata, RFC 8414 authorization server metadata, bearer challenges from `WWW-Authenticate`, and MCP discovery URL ordering. Unknown metadata fields are preserved for extension compatibility. Protected-resource metadata validation requires an exact resource match, non-empty `authorization_servers`, and no fragment in the resource identifier.

Streamable HTTP clients can be configured with an optional bearer token through `McpTransportConfig::StreamableHttp`. When present, the token is sent with `Authorization: Bearer <token>` on POST and GET polling requests. HTTP `401 Unauthorized` and `403 Forbidden` responses are returned as `Error::Authorization` with the parsed bearer challenge when available. Authorization failures observed by background GET polling are surfaced through `McpClientHandler::authorization_failed` and stop polling instead of retrying indefinitely.

Streamable HTTP servers can enable protected-resource behavior with `StreamableHttpOptions::with_authorization`. The server publishes protected-resource metadata at RFC 9728 well-known paths, challenges unauthenticated requests with `WWW-Authenticate: Bearer`, and delegates token validation to the caller-provided validator. Invalid or expired tokens return `401`; insufficient scopes return `403` with `error="insufficient_scope"` and the required `scope` value.

The crate intentionally does not implement a complete OAuth authorization-code flow. Client registration, browser/device UX, redirect handling, PKCE verifier storage, token exchange, token refresh, secure token storage, and Waltr-specific config or UI policy belong in the integrating layers.

## Unsupported Areas

Unsupported protocol areas should stay explicit in code, docs, and tests. Current intentionally unsupported or partial areas include:

- Resource subscriptions.
- Sampling.
- Elicitation.
- Task runtime execution. Task capability shapes and tool `execution.taskSupport` metadata are represented, but server-side task execution is planned separately.
- OAuth authorization-code flows, automatic refresh, and token lifecycle management. HTTP protected-resource metadata validation, bearer challenges, bearer injection, authorization errors, discovery URL helpers, post-initialize authorization failure reporting, and server validation hooks are implemented.

Do not add placeholder success behavior for unsupported features. Prefer explicit unsupported coverage in spec tests.

## Tests

MCP tests are split by purpose:

| Path | Covers |
| --- | --- |
| [../../crates/mcp/tests/spec.rs](../../crates/mcp/tests/spec.rs) and [../../crates/mcp/tests/spec/](../../crates/mcp/tests/spec/) | Protocol conformance tests for the single supported revision, `2025-11-25`. Older revision tests are used only as migration scaffolding and should not remain active. |
| [../../crates/mcp/tests/ecosystem.rs](../../crates/mcp/tests/ecosystem.rs) and [../../crates/mcp/tests/ecosystem/](../../crates/mcp/tests/ecosystem/) | Interoperability against ecosystem clients and servers such as `rmcp`. |
| [../../crates/mcp/tests/progressive.rs](../../crates/mcp/tests/progressive.rs) | Feature-gated coverage for the progressive server macro, generated help, workflows, resources, filters, and auth helpers. |

`rmcp` is a dev-dependency used for ecosystem compatibility tests. It should not be introduced into runtime/library code.

See [../testing.md](../testing.md) for canonical commands.
