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
| [`oauth`](../../crates/mcp/src/oauth.rs) | Protected-resource and authorization-server metadata, bearer challenges, discovery/client helpers, and feature-gated hosted authorization. |
| [`progressive`](../../crates/mcp/src/progressive.rs) | Feature-gated helpers for explicit multi-tool progressive servers with tool-scoped actions and runbooks. |
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

Call `McpClient::shutdown().await` when the client is finished and handle its `Result`. For stdio it kills and waits for the subprocess. For Streamable HTTP it stops and joins GET and POST SSE readers, waits a bounded time for sends, and issues an authenticated `DELETE` for a server-issued session. A dedicated supervisor continuously joins completed finite readers even while the client is otherwise idle. Cancellation closes reader registration, explicitly aborts every remaining reader, joins every handle, and drives the tracked count to zero before supervisor exit; the bounded transport fallback uses the same abort registry rather than dropping handles. Handshake headers and bodies are time- and size-bounded; every failure after a session is observed performs one cleanup DELETE. Local cleanup still completes when DELETE fails, and concurrent shutdown calls share one DELETE outcome. Dropping a client handle is not an async shutdown contract and should not be relied on for transport cleanup.

The client handler callback surface currently covers roots, list-changed notifications, logging notifications, progress notifications, terminal transport disconnection, classified background transport failures, and Streamable HTTP SSE event ids observed from POST responses or GET delivery. `transport_failed` distinguishes network, HTTP-status, SSE-body, and SSE-decoding failures and reports whether automatic retry remains active.

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
| Streamable HTTP client | Sends JSON-RPC over HTTP POST, incrementally decodes JSON-RPC from JSON or `text/event-stream` responses, promotes a server-issued session only after validating the initialize result, sends its negotiated protocol version, advances resumable cursors atomically with inbound message and event-id delivery, reports and retries recoverable GET failures with bounded backoff, starts long-lived GET delivery after `notifications/initialized`, and can attach an optional bearer token to every HTTP request. Session `404` recovery serializes a new JSON, SSE, or stateless initialize handshake without replaying the failed operation; SSE reinitialization returns on the matching response without waiting for EOF, and replacement sessions use a new generation and empty cursor. |
| Stdio server | Reads line-delimited JSON-RPC from stdin and writes line-delimited JSON-RPC responses to stdout. |
| Streamable HTTP server | Serves POST, GET, and DELETE requests at `/mcp`, manages initialize/session state through an injected session store, returns JSON responses or non-resumable SSE POST responses when notifications are emitted with the response, queues notification-only SSE messages for resumable long-lived GET delivery, denies every present Origin unless canonically allowlisted, and can publish OAuth protected-resource metadata plus validate bearer tokens through caller-provided hooks. |

Streamable HTTP GET requires `Accept: text/event-stream` and an initialized `mcp-session-id`. Each session has one resumable GET outbox. GET applies a stream-scoped `Last-Event-ID` acknowledgement, emits `retry: 1000` plus a sequence-zero id-only priming frame for a new cursor, waits on process-local notifications with a one-second persistence poll, non-destructively leases one batch, and closes after delivery or 30 idle seconds while emitting 15-second keepalives. Events remain pending until a later GET acknowledges their IDs, providing acknowledgment-based at-least-once delivery across disconnects. Credential expiry is checked around store preparation and before every frame, with elapsed expiry winning over ready notifications or sends. Session deletion also ends active streams. DELETE requires `mcp-session-id` and terminates the stored HTTP session. Initialization must omit the session header; later requests with missing, unknown, owner-mismatched, or terminated session IDs are rejected rather than recreating the session.

Servers can replace the default `InMemoryStreamableHttpSessionStore` with any public, object-safe `StreamableHttpSessionStore` through `StreamableHttpOptions::with_session_store`. The contract reads and writes lifecycle, atomically compare-and-sets lifecycle transitions, atomically assigns ordered positive numeric IDs while appending events, transactionally acknowledges prior events and leases the next batch, and deletes sessions with their events. Store errors distinguish unavailable storage from invalid persisted data. Authorization is evaluated before any store operation.

Stores bind each session to `Anonymous` or a stable `McpPrincipalId`, enforce a fixed session TTL, global and per-principal session quotas, per-session event count/byte limits, event-size limits, bounded delivery batches, and initialization/delivery leases. `cleanup_expired` is bounded and advisory; expiry is enforced by every operation before cleanup. POST SSE responses are non-resumable and carry no event IDs; only queued GET outbox events participate in acknowledgments.

The optional `streamable-http-sqlite` and `streamable-http-postgres` features export `SqliteStreamableHttpSessionStore` and `PostgresStreamableHttpSessionStore`. Their constructors accept backend URLs or existing pools and run backend-specific migrations embedded with SQLx's standard `migrate!()` mechanism before returning. SQLite records standard history in `_sqlx_migrations` and therefore expects an MCP-owned database file or in-memory database. PostgreSQL creates the `mcp` schema, runs its standard migrator with that schema as the connection search path, and records history in `mcp._sqlx_migrations`; runtime queries remain schema-qualified. SQLx owns migration version and checksum validation on reopen.

Absent Origin headers are accepted. Every present Origin is denied by default and is validated before request extraction, authorization, or persistence. `StreamableHttpOptions::try_with_allowed_origins` installs a canonical scheme, host, and effective-port allowlist; malformed, duplicate, non-UTF-8, `null`, userinfo-bearing, path-bearing, query-bearing, and fragment-bearing Origins return HTTP `403 Forbidden`.

## Authorization

Authorization support is scoped to HTTP transports, matching MCP `2025-11-25`. Stdio remains unauthenticated at the protocol layer and should obtain credentials from the calling environment if a consumer needs that behavior.

The `oauth` module represents RFC 9728 protected resource metadata, RFC 8414 authorization server metadata, bearer challenges from `WWW-Authenticate`, and MCP discovery URL ordering. Unknown metadata fields are preserved for extension compatibility. Protected-resource metadata validation requires an exact resource match, non-empty `authorization_servers`, and no fragment in the resource identifier.

Streamable HTTP clients can be configured with an optional bearer token through `McpTransportConfig::StreamableHttp`. When present, the token is sent with `Authorization: Bearer <token>` on POST, GET, and DELETE requests. HTTP `401 Unauthorized` and `403 Forbidden` responses are returned as `Error::Authorization` with the parsed bearer challenge when available. Authorization failures observed by background GET delivery are surfaced through `McpClientHandler::authorization_failed` and stop delivery instead of retrying indefinitely. HTTP `503 Service Unavailable` remains a transport/server failure and uses bounded backoff rather than authorization handling.

Streamable HTTP servers can enable protected-resource behavior with `StreamableHttpOptions::with_authorization`. `StreamableHttpAuthorization::new` preserves the external-validator path: `McpTokenValidation::Authorized { principal_id, expires_at }` binds session access to a stable owner and optionally bounds stream lifetime. `StreamableHttpAuthorization::hosted` accepts `McpHostedTokenValidation` and can attach an `McpRevocationLease`. GET delivery races credential expiry, same-process revocation notifications, and authoritative store failure before every frame, including retry metadata and events prepared before the response stream starts. A revoked token or store failure closes delivery without releasing a frame waiting for transport capacity. The default hosted interval is five seconds, so PostgreSQL changes committed by another process terminate streams within five seconds plus scheduling and query latency; local changes made through `OAuthAuthorizationServer` notify immediately after commit. PostgreSQL propagation currently uses this authoritative polling path rather than `LISTEN/NOTIFY`.

The `oauth-authorization-server` feature exports `OAuthAuthorizationServer`, its validated configuration/resource types, object-safe resource-owner authentication and consent hooks, and memory-backed OAuth storage. It serves RFC 8414 metadata, JWKS, Authorization Code with mandatory PKCE S256, consent decisions, access/refresh token exchange, RFC 7009 revocation, RFC 7591 public-client registration, and Client ID Metadata Document resolution. Access tokens are store-authorized ES256 RFC 9068 JWTs; refresh tokens rotate, and replay revokes the complete family. SQLite and PostgreSQL persistence are selected with `oauth-authorization-server-sqlite` and `oauth-authorization-server-postgres`.

Issuer-relative default endpoints are `/.well-known/oauth-authorization-server<issuer-path>`, `<issuer>/authorize`, `<issuer>/authorize/decision`, `<issuer>/token`, `<issuer>/revoke`, `<issuer>/jwks.json`, and an optional configured registration endpoint. Metadata advertises only code response, authorization-code and refresh-token grants, public token authentication method `none`, PKCE `S256`, ES256 access-token signing, and the configured exact resources/scopes. The host supplies resource-owner authentication and consent responses; the generic crate never knows Authentik, cookies, or Waltr principals.

Authorization codes and refresh tokens are persisted only as digests. Code redemption atomically validates client, redirect, resource, PKCE, expiry, and one-time use while committing grant/token state. Access JWT validation also checks the durable access record, client, grant, and linked family, so a valid signature is not sufficient after revocation. Refresh rotates the opaque token and generation atomically; reuse of a consumed generation revokes the family and linked access tokens. Revocation is idempotent and local leases are notified only after commit. Signing verification overlap must cover the access-token lifetime plus accepted clock skew, and private-key rollback overlap cannot exceed verification overlap; invalid relationships are rejected when constructing the hosted server.

SQL OAuth stores persist normalized lifecycle rows rather than a singleton state blob and transactionally write only changed records. Cleanup deletes dependent expired authorization transactions before their retained CIMD snapshots in the same transaction. Access-token validation is an indexed join over token, grant, family, client, and revocation state; it does not acquire the mutation transaction lock. Active-signer readiness decrypts and parses PKCS#8 private material, derives its public key, and requires the exact stored JWK before issuance.

`OAuthAuthorizationServer::with_client_registration` installs generic registration policy without selecting deployment-specific trust or proxy behavior. Preregistered clients use `OAuthAuthorizationStore::register_client`; every registration path enforces the public redirect-URI profile. DCR registrations begin provisionally, are admitted under atomic global/source rate and capacity limits, and are promoted on their first authenticated authorization subject to per-principal and active-dynamic-client quotas. Cleanup reclaims expired provisional clients, expired rate buckets, and inactive promoted clients after every access token and refresh family is expired or revoked. Retained grant audit records do not prevent reclamation once token state is terminal. Dynamic capacity is separate from the preregistered/CIMD pool. Hosts may provide `OAuthRegistrationSourceResolver` only for source identities established by their own trusted-proxy boundary and `OAuthRegistrationObserver` for sanitized CIMD hostname/outcome and aggregate DCR outcome metrics; observers never receive client IDs, metadata, or credentials.

Default generic registration limits are a 64 KiB request, 2,048-byte field values, 1,000 provisional clients for ten minutes, 10,000 active dynamic clients, 20 promoted clients per principal, 120 global and 20 trusted-source registrations per 60 seconds, and 30-day inactive-client retention. Native loopback redirects are disabled unless the host explicitly enables them.

The hosted server resolves clients in this order: an enabled stored client, an unexpired validated CIMD snapshot, then a fresh CIMD fetch when configured. `HardenedOAuthClientMetadataFetcher::production` requires HTTPS, resolves DNS under a fixed timeout and limits the complete result set before connecting, rejects every non-global IPv4/IPv6 destination including Teredo and 6to4, pins the approved addresses, verifies the connected peer, refuses redirects, bounds connection/total time, headers, and body bytes, and requires a JSON media type. It sends no ambient cookies, authorization credentials, or inbound request headers. A host may install `TrustedPrivateOAuthCimdDestinationPolicy` with exact HTTPS origins to admit only RFC 1918 IPv4 and IPv6 unique-local destinations at those origins; loopback, link-local, metadata, and other prohibited ranges remain blocked. Loopback HTTP is available only through the explicit test policy. Validated documents are normalized and content-digested; authorization transactions and codes retain and use their exact immutable snapshot even after cache refresh or document mutation.

Authorization-server metadata validation accepts preregistered-only servers that advertise neither DCR nor CIMD; those fields describe available registration mechanisms rather than Authorization Code client support. Callers must still select only a mechanism actually advertised for clients that are not preregistered. The `oauth::client::register_public_client` helper sends public DCR metadata and rejects responses containing registration credentials or mismatched metadata. `McpPublicClientRegistrationRequest::validate_and_normalize`, `McpClientMetadataDocument::validate_public_client`, `McpClientRegistrationResponse::validate_public_response`, and `validate_public_redirect_uri` expose the corresponding protocol validation without imposing host persistence policy.

## Unsupported Areas

Unsupported protocol areas should stay explicit in code, docs, and tests. Current intentionally unsupported or partial areas include:

- Resource subscriptions.
- Sampling.
- Elicitation.
- Task runtime execution. Task capability shapes and tool `execution.taskSupport` metadata are represented, but server-side task execution is planned separately.

Do not add placeholder success behavior for unsupported features. Prefer explicit unsupported coverage in spec tests.

## Tests

MCP tests are split by purpose:

| Path | Covers |
| --- | --- |
| [../../crates/mcp/tests/spec.rs](../../crates/mcp/tests/spec.rs) and [../../crates/mcp/tests/spec/](../../crates/mcp/tests/spec/) | Protocol conformance tests for the single supported revision, `2025-11-25`. Older revision tests are used only as migration scaffolding and should not remain active. |
| [../../crates/mcp/tests/ecosystem.rs](../../crates/mcp/tests/ecosystem.rs) and [../../crates/mcp/tests/ecosystem/](../../crates/mcp/tests/ecosystem/) | Interoperability against ecosystem clients and servers such as `rmcp`. |
| [../../crates/mcp/tests/progressive.rs](../../crates/mcp/tests/progressive.rs) | Feature-gated coverage for progressive tool isolation, generated help, runbooks, resources, filters, and auth helpers. |
| [../../crates/mcp/tests/streamable_http_sqlite_store.rs](../../crates/mcp/tests/streamable_http_sqlite_store.rs) | Feature-gated SQLite store contract, migration history, persisted-data safeguards, and concurrency. |
| [../../crates/mcp/tests/streamable_http_postgres_store.rs](../../crates/mcp/tests/streamable_http_postgres_store.rs) | Feature-gated, ignored container-backed PostgreSQL parity, migrations, concurrency, and safeguards. |
| [../../crates/mcp/tests/oauth_authorization_server.rs](../../crates/mcp/tests/oauth_authorization_server.rs) | Hosted authorization metadata, JWKS, authorization code, PKCE, JWT, refresh rotation/replay, and revocation flow. |
| [../../crates/mcp/tests/oauth_store.rs](../../crates/mcp/tests/oauth_store.rs) | OAuth store atomic transition, revocation, quota, cleanup, and signing-key contract. |
| [../../crates/mcp/tests/oauth_sqlite_store.rs](../../crates/mcp/tests/oauth_sqlite_store.rs) | Feature-gated SQLite OAuth parity, reopen persistence, migration history, and secret-at-rest checks. |
| [../../crates/mcp/tests/oauth_postgres_store.rs](../../crates/mcp/tests/oauth_postgres_store.rs) | Feature-gated, ignored container-backed PostgreSQL OAuth parity and concurrent initialization. |

`rmcp` is a dev-dependency used for ecosystem compatibility tests. It should not be introduced into runtime/library code.

Run the SQL store suites explicitly because they are not enabled by the default feature set:

```text
cargo test -p mcp --features streamable-http-sqlite --test streamable_http_sqlite_store
cargo test -p mcp --features streamable-http-postgres --test streamable_http_postgres_store -- --ignored
cargo test -p mcp --features oauth-authorization-server-sqlite --test oauth_sqlite_store
cargo test -p mcp --features oauth-authorization-server-postgres --test oauth_postgres_store -- --ignored
```

The PostgreSQL command requires Docker. See [../testing.md](../testing.md) for the complete test matrix.
