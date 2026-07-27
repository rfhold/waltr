# 06 Outbound MCP Client

Status: Complete

## Objective

Make Waltr's agent/server outbound MCP OAuth path a standards-based client of any protected MCP server, including a Waltr-hosted one, without per-key special cases or mandatory environment client IDs.

## Baseline Defects

The current outbound path:

- Requires `WALTR_MCP_<KEY>_OAUTH_CLIENT_ID` and redirect URI before discovery.
- Parses but ignores CIMD and DCR capability metadata.
- Copies target-level registration into every session runtime snapshot.
- Stores registration fields as untyped token metadata.
- Drops MCP metadata on the first token refresh.
- Can race concurrent refreshes and trigger refresh-family replay handling.
- Installs one immutable bearer token into the MCP transport.
- Uses manual pasted callback completion for both web and TUI.

Fix refresh correctness and concurrency before testing against the strict hosted authorization server.

## Durable Credential Model

Add a version-tolerant MCP-specific credential state in `waltr-agent`:

```text
McpOAuthState
├── resource
├── issuer
├── registration
│   ├── method: preregistered | client_metadata | dynamic
│   ├── client_id
│   ├── redirect_uri
│   ├── token_endpoint_auth_method
│   ├── created_at
│   └── expires_at
├── endpoints
│   ├── authorization
│   ├── token
│   └── revocation
└── token
    ├── access_token
    ├── refresh_token
    ├── token_type
    ├── granted_scopes
    ├── expires_at
    └── refresh_generation
```

Do not hide these fields in a string map. Preserve deserialization of current `StoredCredential::OAuth` MCP records and upgrade them on the next successful exchange or refresh. Provider OAuth credentials remain unaffected.

Registration and token state are target-scoped by stable MCP server identity, not copied into durable agent session settings. Existing complete session-snapshot registration pairs may remain a lowest-priority compatibility fallback while old snapshots exist; new snapshots do not require or persist registration.

The credential-store contract must provide an atomic cross-process refresh claim keyed by target and refresh generation. Waltr server injects a SQL-backed implementation for shared deployments; the file implementation provides the same contract for a single process. A claim has a bounded lease and explicit commit/abort outcomes. Only the claimant sends the refresh request. If the network outcome is indeterminate, the credential becomes reauthorization-required rather than retrying the same refresh token and risking family replay.

## Discovery And Registration Selection

Refactor `AuthService::begin_mcp_auth` into this order:

1. Validate the configured Streamable HTTP endpoint.
2. Reuse a structured bearer challenge already observed by the runtime when safe, or request a fresh challenge.
3. Discover and validate RFC 9728 protected-resource metadata.
4. Select and discover a compatible authorization server.
5. Load persisted registration for the exact issuer, resource, and redirect URI.
6. Resolve registration in MCP priority order:
   - Valid configured preregistration.
   - Client ID Metadata Document when advertised and Waltr has a usable HTTPS client identity.
   - Dynamic Client Registration when advertised.
   - Typed configuration-required failure.
7. Persist a dynamic registration before returning the authorization URL.
8. Start Authorization Code with PKCE and exact resource/scopes.

Never branch on the server key `self`. A configured endpoint pointing to Waltr's own `/mcp` follows exactly this path.

## Waltr Client Metadata Document

When `WALTR_PUBLIC_URL` is HTTPS, expose a Waltr outbound-client identity such as:

```text
GET /.well-known/oauth-client/waltr
```

The document declares:

- Its own exact URL as `client_id`.
- The server-owned outbound callback URI.
- Authorization Code and refresh grants.
- `code` response type.
- Token endpoint auth method `none`.
- A stable client name and project URI.

This is a client identity for Waltr calling external MCP servers. It is not `/mcp` resource metadata and does not create a `self` registration.

Local HTTP deployments cannot use CIMD unless an explicit standards-compatible development policy permits it. They use DCR or preregistration.

## Callback Completion

Add a server-owned callback route distinct from the hosted authorization server:

```text
GET /outbound-oauth/callback
```

The outbound authorization attempt persists state, PKCE verifier, target binding, exact redirect URI, expiry, and sanitized status in server-owned durable storage so any replica can complete it.

Web flow:

1. Begin authorization.
2. Open the authorization URL.
3. External server redirects to Waltr's callback.
4. Callback atomically exchanges and stores credentials.
5. Shared client polls or watches sanitized attempt status.

Retain manual completion for TUI and providers that cannot redirect to the server callback. Manual input remains state-bound and uses the same exchange service.

Additive protobuf changes may include completion mode and attempt-status polling. Preserve existing RPC field numbers and `CompleteAuth` for manual fallback.

## Refresh Correctness

Before strict refresh-family reuse detection is enabled:

- Preserve registration, endpoints, resource, scope, and token type across refresh.
- Keep actual expiry separate from the refresh-ahead threshold.
- Require a nonempty bearer access token and supported token type.
- Retain the previous refresh token only when the response omits a replacement.
- Claim refresh atomically by target and generation in shared durable storage before making a network request.
- Re-read credential state after acquiring the claim.
- Use compare-and-swap generation semantics so stale results cannot overwrite a newer token.
- Let nonclaiming replicas wait for and reuse the committed generation rather than sending the old refresh token.
- Treat an indeterminate claimant outcome as reauthorization-required; do not retry a possibly consumed refresh token.
- Persist advertised revocation endpoint and revoke best-effort on logout before local deletion.

## Dynamic Bearer Provider

Extend the generic MCP Streamable HTTP client with an optional asynchronous bearer provider while retaining static `Option<String>` configuration for compatibility.

The provider supplies a current credential for every POST, GET connection, reconnection, and DELETE. The Waltr implementation may refresh before returning a token and coordinates callers through the durable cross-process refresh claim.

Provider failures must surface as structured authorization or availability errors. The transport must not silently continue with an expired token or log it.

## Shared Client And UI

Keep discovery, registration, callback, exchange, and refresh in server/agent layers. Shared client state owns only:

- Target identity.
- Registration method as nonsecret status.
- Completion mode.
- Attempt expiry and lifecycle.
- Sanitized failure reason.

Web and TUI continue using the shared reducer path. Neither UI receives access tokens, refresh tokens, client registration credentials, state, or PKCE verifier.

## Tests

Cover:

- Refresh preserving all MCP metadata.
- Rotated and omitted refresh tokens.
- Concurrent refresh singleflight and stale-write prevention.
- Cross-replica refresh claiming, claimant failure, lease expiry, and indeterminate-outcome reauthorization.
- Dynamic bearer use by POST, GET/reconnect, and DELETE.
- Discovery occurring before registration selection.
- Reuse of persisted registration after restart.
- Preregistered, CIMD, and DCR paths.
- Invalid or confidential DCR response rejection.
- Exact resource preservation through authorize, token, refresh, and revoke.
- No static registration required for CIMD/DCR-capable servers.
- Typed failure when no mechanism is available.
- New session runtime resolution without OAuth environment variables.
- Legacy session fallback without making it primary.
- Server-owned callback success, denial, replay, expiry, and replica handoff.
- Manual completion fallback.
- Logout revocation and unconditional local deletion.
- Waltr-to-Waltr authorization with no `self` code or configuration.

Update browser integration fixtures so Waltr outbound MCP connects to the hosted Waltr authorization server through DCR or CIMD rather than a statically configured Dex client.

## Exit Criteria

- Waltr can connect to a new standards-compatible MCP server using discovery and DCR without environment registration.
- Waltr can use CIMD when its public client metadata is available.
- Active MCP transports survive access-token rotation without runtime reconstruction.
- Strict refresh replay detection does not punish legitimate concurrent Waltr sessions.
- Target registration and credentials survive restart without entering agent session snapshots.
- No production code or deployment configuration treats `self` specially.

## Dependencies

Depends on generic protocol and registration workstreams. Waltr-to-Waltr end-to-end coverage also depends on [Waltr Server Integration](05-waltr-server-integration.md).
