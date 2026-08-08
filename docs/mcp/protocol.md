# MCP Protocol

The `mcp` crate implements one active Model Context Protocol revision: `2026-07-28`.
Clients and servers reject every other revision. There is no initialize-era fallback,
version negotiation shim, or protocol-session compatibility path.

## Ownership

The generic crate owns:

- JSON-RPC and MCP wire types
- typed client and server APIs
- stdio and Streamable HTTP transports
- request metadata, capabilities, discovery, results, MRTR, and subscriptions
- HTTP authorization and hosted OAuth primitives
- trace-context propagation
- revision-scoped conformance and ecosystem tests

Waltr-specific tool adaptation, runtime settings, identity policy, and UI behavior stay in
their owning Waltr crates.

## Per-Request Protocol

MCP `2026-07-28` is stateless. Every request carries these fields in `params._meta`:

- `io.modelcontextprotocol/protocolVersion`, exactly `2026-07-28`
- `io.modelcontextprotocol/clientCapabilities`, as an object
- `io.modelcontextprotocol/clientInfo`, emitted by the Waltr client

The optional `io.modelcontextprotocol/logLevel` enables logging only for that request.
Opaque caller metadata, including `progressToken` and trace context, is preserved when
the required metadata is added.

Servers implement `server/discover`. Discovery reports supported versions,
capabilities, instructions, cache hints, and server identity. Waltr clients call
discovery when connecting and validate all required fields.

Successful results use `resultType: "complete"` or `resultType: "input_required"`.
Clients reject unknown result types unless a future negotiated extension defines them.
As required by the active specification, clients treat an omitted `resultType` as
`"complete"`; this receiver tolerance does not enable any older lifecycle or transport.

Unsupported revisions use error `-32022` with `data.supported` and `data.requested`.
Mirrored-header mismatches use `-32020`. Resource-not-found and malformed method
parameters use `-32602`.

## Features

The typed surface supports tools, prompts, resources, resource templates, completion,
pagination, progress, cancellation, annotations, icons, opaque metadata, and extension
capability maps.

Roots are retained as a deprecated client feature. A server requests roots through an
`input_required` result; the client retries the originating `tools/call`, `prompts/get`,
or `resources/read` request with a new JSON-RPC ID, exact `requestState`, and keyed
`inputResponses`.

List-change notifications use a long-lived `subscriptions/listen` request. The first
message acknowledges only requested filters that the server actually advertises.
Every acknowledgment, notification, and graceful final result carries the originating
request ID as `io.modelcontextprotocol/subscriptionId`. Waltr reconnects advertised
list-change subscriptions after an unexpected closure.

Logging is deprecated but retained. Clients request a threshold through per-request
metadata. Servers emit `notifications/message` only on that request's response stream
and never on a subscription stream. `logging/setLevel` is not implemented.

## Transports

### stdio

stdio uses one UTF-8 JSON-RPC message per line. Requests and notifications are validated
for required per-request metadata. Parse errors (`-32700`) and invalid requests
(`-32600`) are returned without terminating the stream. `notifications/cancelled`
cancels the matching active request; servers never initiate JSON-RPC requests.

### Streamable HTTP

The MCP endpoint accepts one JSON-RPC request or notification per HTTP `POST`.
Requests return JSON or a request-scoped SSE stream; accepted notifications return an
empty `202`. `GET`, `DELETE`, protocol sessions, resumability, event IDs, and
`Last-Event-ID` are absent. Closing a response stream cancels its request.

HTTP requests mirror body metadata through `MCP-Protocol-Version`, `Mcp-Method`, and,
for named operations, `Mcp-Name`. Unsafe, non-ASCII, whitespace-padded, or sentinel-like
values use the specification's `=?base64?...?=` encoding. Valid `x-mcp-header`
annotations on statically reachable string, integer, or boolean tool parameters produce
`Mcp-Param-*` headers. Clients exclude malformed annotated tools; servers decode and
compare recognized headers against the request body before executing the tool.

Origin validation is default-deny when an `Origin` header is present. Bearer tokens are
resolved independently for every POST. Token expiry or revocation terminates the active
response stream.

## Authorization

Authorization is optional and applies only to HTTP. Protected-resource and issuer URLs
require HTTPS except for loopback development URLs. Discovery validates exact resource
and issuer identities. Dynamic client registration carries and validates
`application_type`; Waltr uses `web` for HTTPS callbacks and `native` for loopback/native
callbacks. RFC 9207 `iss` responses are validated before code redemption or error
display, and persisted registrations remain bound to their authorization-server issuer.

Hosted OAuth persistence remains in the MCP-owned database. Historical migration
`0001_streamable_http_sessions.sql` is immutable migration history, but its session table
is inert because the modern protocol has no session runtime.

## Unsupported Areas

The crate explicitly does not implement:

- resource URI subscriptions and `notifications/resources/updated`
- sampling input requests
- elicitation input requests
- task runtime or task extension methods
- non-roots MRTR input requests
- initialize-era lifecycle, ping, or session transport behavior

Unsupported areas are omitted from capabilities and fail explicitly rather than
returning placeholder success.

## Verification

Run the revision suite and all generic integration tests:

```bash
cargo test -p mcp --test spec
cargo test -p mcp --tests --all-features
```

The PostgreSQL OAuth store contract remains ignored by default because it requires a
container-backed database. `rmcp` is a dev-only ecosystem fixture and explicitly selects
the `2026-07-28` discovery lifecycle; revision-scoped tests remain the conformance
authority.
