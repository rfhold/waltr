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

Waltr-specific tool adaptation, inference settings, identity policy, and UI behavior stay in
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

The official `io.modelcontextprotocol/skills` extension adds typed `skills/list` and `skills/get` methods.
Clients opt in with `McpClientOptions::with_skills`; the default remains off.
Servers publish exact file manifests and serve their bytes through `resources/read`.
See [Skills](skills.md) for catalog registration, validation, and host authority boundaries.

The `io.modelcontextprotocol/ui` extension adds typed MCP Apps metadata, bounded resource decode, and iframe lifecycle payloads.
Clients opt in with `McpClientOptions::with_apps`; the default remains off.
Apps revision `2026-01-26` does not change core revision `2026-07-28` or its per-request discovery contract.
The generic crate supplies no browser sandbox or host authorization policy.
See [MCP Apps](apps.md) for the separate extension boundary.

Tool results retain their raw wire value and also expose ordered borrowed text, image, and
unknown extension blocks. Parsing rejects malformed known blocks while leaving image decoding,
MIME policy, and payload validation to the consuming application.
Under MCP `2026-07-28`, `structuredContent` may be any JSON value. The protocol recommends also
providing serialized structured JSON in a text content block for compatibility; progressive
servers apply the synchronization policy documented in [progressive.md](progressive.md).

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
Client reads use the default bounded message size unless the caller selects explicit limits through
the with-limits connection APIs described below.

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

Clients bound JSON responses, SSE lines and events, and stdio messages. Default connections accept
at most 64 KiB in one SSE line and 1 MiB in one complete message, SSE event, JSON response body, or
stdio line. Applications expecting larger bounded content such as Base64 image blocks opt in with
`McpClient::connect_with_transport_limits` or
`McpClient::connect_with_bearer_provider_and_transport_limits`; the original connection and public
transport entry points retain the conservative defaults.

`McpClient::connect_with_transport_options` accepts `McpTransportOptions` for callers that need to
combine explicit receive limits, a dynamic bearer provider, or a preconfigured `reqwest::Client`.
Low-level transport callers can pass the same options to `McpTransport::start_with_options`.
The supplied client is used only by Streamable HTTP. When no client is supplied, Streamable HTTP
continues to use `reqwest::Client::new()` behavior; stdio ignores the HTTP client option.

## Authorization

Authorization is optional and applies only to HTTP. Protected-resource and issuer URLs
require HTTPS except for loopback development URLs. Discovery validates exact resource
and issuer identities. Dynamic client registration carries and validates
`application_type`; Waltr uses `web` for HTTPS callbacks and `native` for loopback/native
callbacks. RFC 9207 `iss` responses are validated before code redemption or error
display, and persisted registrations remain bound to their authorization-server issuer.

Hosted public-client metadata need not declare the nonstandard
`code_challenge_methods` extension. Authorization still requires PKCE S256, and code
redemption verifies the verifier. This applies to preregistered, CIMD, and DCR clients.

The redirect-admission policy must explicitly allow native HTTP loopback callbacks.
After admission, their port can differ at authorization; the original scheme, host,
path, query, and all other components must remain identical. `localhost` and
`127.0.0.1` are not interchangeable. HTTPS and non-loopback redirects remain exact
matches. The transaction retains the actual authorization redirect, and token
exchange must repeat that exact URI, including its port. Memory, SQLite, and
PostgreSQL stores use the same matching rules and immutable CIMD snapshots.

Hosted OAuth persistence remains in the MCP-owned database. Historical migration
`0001_streamable_http_sessions.sql` is immutable migration history, but its session table
is inert because the modern protocol has no session runtime.

### Resource-Owner OIDC

The optional `oauth-resource-owner-oidc` feature provides a provider-neutral, one-shot
implementation of `OAuthResourceOwnerAuthenticator`. It authenticates exactly one hosted
authorization transaction and creates no reusable application session. The SQL variants
are `oauth-resource-owner-oidc-sqlite` and `oauth-resource-owner-oidc-postgres`; each also
enables the corresponding hosted authorization-store backend.

`OidcResourceOwnerAuthenticator::discover` validates exact issuer and local endpoint
configuration, discovers a redirect-free Authorization Code provider, loads bounded
JWKS, and selects `client_secret_basic`, `client_secret_post`, or `none` according to the
registered client and advertised metadata. HTTPS is required by default. The explicit
`OidcEndpointPolicy::LoopbackTest` exception permits HTTP and non-Secure cookies only on
loopback hosts. Every discovery, JWKS, and token request has a validated total timeout;
the default is ten seconds and callers may select a nonzero override no greater than one
minute. The authenticator applies that bound itself rather than trusting the supplied
Reqwest builder.

The hosted server generates its normal browser correlation and sets it through the
authenticator's additive cookie directive. OIDC uses one transaction-specific host-only,
HTTP-only, Lax, root-path cookie, so concurrent authorization tabs cannot overwrite one
another. Only SHA-256 state and correlation digests enter persistence. Login sends the
upstream provider state, nonce, and S256 challenge; callback atomically claims the attempt
before exchanging the code with the exact callback and verifier. ID-token mapping occurs
only after asymmetric signature, `kid`, issuer, audience/`azp`, time, nonce, subject, and
optional `at_hash` verification.

`OidcPrincipalMapper` receives only the verified provider-neutral identity and claims.
Its principal, denial, or unavailability result maps directly into hosted authenticator
semantics. A successful callback reconstructs the configured local continuation from the
parent transaction ID. Resume reads the completed OIDC principal without consuming it and
returns the cookie correlation. The hosted authorization store then atomically validates
and consumes that correlation while attaching the principal or promoting the dynamic
client. False outcomes never proceed to consent and leave no partially committed hosted
authentication. Only after that transaction commits does the server invoke an additive
best-effort authenticator finalization hook and clear the cookie. OIDC finalization marks
its completed attempt consumed; if it fails, the already-consumed hosted correlation
still makes the flow one-shot and normal cleanup removes the residual OIDC row.

`OidcResourceOwnerStore` is a separate object-safe persistence seam with a process-local
reference store. `SqliteOidcResourceOwnerStore` and `PostgresOidcResourceOwnerStore` accept
the caller's existing MCP pool and run the same embedded MCP migrator. Migration `0004`
adds the digest-only attempt table with a cascading parent transaction foreign key;
expired attempts support bounded cleanup.

## Unsupported Areas

The crate explicitly does not implement:

- resource URI subscriptions and `notifications/resources/updated`
- sampling input requests
- elicitation input requests
- task runtime or task extension methods
- `resources/directory/read` and the Skills `directoryRead` capability
- non-roots MRTR input requests
- initialize-era lifecycle, ping, or session transport behavior

Unsupported areas are omitted from capabilities and fail explicitly rather than
returning placeholder success.

## Verification

Run the revision suite and all generic integration tests:

```bash
cargo test -p mcp --test spec
cargo test -p mcp --test oauth_resource_owner_oidc --all-features
cargo test -p mcp --tests --all-features
```

The PostgreSQL OAuth store contract remains ignored by default because it requires a
container-backed database. `rmcp` is a dev-only ecosystem fixture and explicitly selects
the `2026-07-28` discovery lifecycle; revision-scoped tests remain the conformance
authority.
