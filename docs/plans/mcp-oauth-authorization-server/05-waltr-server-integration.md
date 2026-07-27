# 05 Waltr Server Integration

Status: Complete

## Objective

Compose the generic MCP OAuth authorization server into `waltr-server`, use the existing Authentik-backed browser session as the resource-owner authentication mechanism, present Waltr-owned consent, and replace direct Authentik MCP bearer validation with Waltr-issued token validation.

## Target Boundary

`waltr-server` supplies these adapters and policies:

- Current authenticated browser principal.
- Login continuation through `/oidc/login`.
- Consent page rendering and CSRF validation.
- Public issuer and resource URLs.
- Supported MCP scopes and lifetimes.
- MCP OAuth store and wrapping-key ring selection.
- Startup readiness and maintenance-task lifecycle.

The generic `mcp` service remains responsible for parsing OAuth requests, client resolution, transaction state, consent grants, authorization codes, token issuance, refresh, revocation, signing keys, and authorization leases.

## Configuration

Separate upstream access authentication from the local MCP issuer. Replace direct-MCP OIDC settings with local authorization-server settings.

Recommended environment contract:

| Variable | Contract |
| --- | --- |
| `WALTR_MCP_OAUTH_ISSUER` | Exact local issuer; defaults to `WALTR_PUBLIC_URL` with `/oauth` appended. |
| `WALTR_MCP_REQUIRED_SCOPES` | Space-separated scopes supported and required by `/mcp`. |
| `WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE` | Read-only file containing the versioned private-key wrapping-key ring. |
| `WALTR_MCP_OAUTH_ACCESS_TOKEN_TTL` | Short access-token lifetime; default five minutes. |
| `WALTR_MCP_OAUTH_REFRESH_TOKEN_TTL` | Per-generation refresh lifetime. |
| `WALTR_MCP_OAUTH_REFRESH_FAMILY_TTL` | Absolute refresh-family lifetime. |
| `WALTR_MCP_OAUTH_CODE_TTL` | Authorization-code lifetime; default five minutes. |
| `WALTR_MCP_OAUTH_ALLOW_DCR` | Enables public dynamic registration. |
| `WALTR_MCP_OAUTH_ALLOW_CIMD` | Enables Client ID Metadata Documents. |

Remove the direct resource-verifier settings:

```text
WALTR_MCP_OIDC_ISSUER
WALTR_MCP_OIDC_AUDIENCE
```

Do not add legacy equivalents. Browser and native API Authentik settings remain independent.

Validate at startup:

- OIDC access mode is fully initialized before enabling the local MCP OAuth server.
- Issuer and all advertised endpoints are under the normalized public deployment URL.
- Resource URL exactly matches the public `/mcp` URL.
- Wrapping-key ring has one active version and no duplicate IDs.
- Lifetimes, limits, and maintenance intervals are bounded.
- DCR/CIMD policy is explicit.
- SQL OAuth store and active signing key are usable before readiness succeeds.

When access auth is disabled, preserve the current unauthenticated MCP development behavior and do not expose a partially functional OAuth authorization server.

## Principal Adapter

Reuse `AuthenticatedPrincipal::mcp_principal_id()` so the stable authorization identity remains the domain-separated digest of exact upstream Authentik issuer and subject.

The adapter must:

- Accept only a valid Waltr browser-session cookie.
- Revalidate session expiry and durable revocation.
- Return only the stable `McpPrincipalId` and safe display attributes required by consent.
- Never pass Authentik access or ID tokens into the MCP OAuth store.
- Never derive identity from email, username, display name, or client-controlled claims.

Native API bearer tokens and MCP access tokens cannot satisfy browser authentication at `/oauth/authorize`.

## Authorization Continuation

Use an opaque stored transaction rather than round-tripping the original authorization query through Authentik:

1. `/oauth/authorize` validates client, redirect, resource, scopes, state, and PKCE.
2. The generic service persists the pending transaction.
3. If no browser session exists, the Waltr adapter sets a transaction-specific secure correlation cookie and redirects to `/oidc/login` with a local continuation containing only the opaque transaction ID.
4. Existing OIDC login and callback establish the Waltr browser session.
5. The continuation resumes the stored authorization transaction only after the correlation cookie matches its persisted digest, then consumes that correlation once.
6. The principal is attached and the consent decision is requested.

The transaction identifier is nonsecret but high entropy. It is insufficient without the browser-bound correlation value, is single-use at resumption, and is protected by `Referrer-Policy: no-referrer`. Transaction state remains server-side and expires independently of the browser login attempt.

## Consent UI

Add a minimal Waltr-owned, server-rendered consent page so any MCP client's browser can complete authorization without loading the authenticated application SPA.

The page displays:

- Waltr deployment identity.
- Untrusted client name rendered as text.
- Exact client ID in a secondary detail.
- Exact MCP resource.
- Requested scopes with deployment-owned descriptions.
- Approve and deny actions.

The decision form includes a transaction-bound CSRF token, accepts only POST, and returns no-store responses. The generic authorization service controls the final success or error redirect after the Waltr handler submits the decision.

Do not render remote logos or HTML from client metadata. Do not expose the upstream Authentik subject.

## Router Composition

Update `crates/waltr-server/src/app.rs` with explicit route ownership:

| Route | Owner |
| --- | --- |
| `/.well-known/oauth-protected-resource` | Waltr native API metadata. |
| `/.well-known/oauth-protected-resource/mcp` | Generic MCP protected-resource router. |
| `/.well-known/oauth-authorization-server/oauth` | Generic MCP OAuth authorization server. |
| `/oauth/*` | Generic OAuth routes plus Waltr principal and consent adapters. |
| `/oidc/*` | Existing Waltr-to-Authentik access login. |
| `/mcp` | Generic Streamable HTTP MCP server. |

Do not rely on middleware interception to resolve overlapping well-known routes. Build routers so each exact route has one owner.

Path-specific MCP metadata advertises only the local Waltr issuer. API-root metadata continues advertising the native API Authentik issuer.

## Token Validation

Replace the validator closure that calls `AccessAuthenticator::validate_mcp_access_token()` with the hosted authorization server's validator and revocation lease.

Remove from `AccessAuthenticator` after the cutover:

- Dedicated MCP verifier state.
- MCP issuer/audience getters.
- Direct Authentik MCP bearer validation.

Keep browser and native API validation unchanged.

There is no dual-validation period. Authentik-issued access tokens must fail against `/mcp` immediately after the local issuer is advertised.

## Persistence And Lifecycle

Initialize the OAuth store from the existing MCP persistence target, alongside Streamable HTTP session persistence. Both use the MCP SQLite file or PostgreSQL schema and complete shared migrator.

Extend `ServerApplication` lifecycle ownership to include:

- MCP session cleanup.
- OAuth state cleanup.
- Signing-key rotation and retirement.
- Wrapping-key re-encryption.
- Cross-replica revocation listener/revalidation.

All tasks share explicit cancellation and are joined on graceful shutdown, bind failure, startup rollback, and test teardown. Do not detach maintenance tasks from the application lifecycle.

## Error And Availability Policy

- Authentik outage prevents new browser login but does not invalidate existing Waltr browser sessions or already issued MCP tokens.
- OAuth store failure blocks authorization, token, refresh, revocation, and protected-resource validation with sanitized unavailable responses.
- Signing failure does not consume an authorization code or refresh token unless issuance can commit atomically.
- Consent and authorization pages disclose no storage, issuer-internal, key, or token details.
- Health reports process liveness; readiness requires usable OAuth persistence and signing state.

## Tests

Add or update server tests for:

- Independent API and MCP protected-resource metadata.
- Local authorization-server metadata and JWKS.
- Browser-session login continuation and transaction resumption.
- Expired, revoked, wrong-issuer, and wrong-client browser sessions.
- Consent approval, denial, CSRF rejection, continuation fixation/replay, and remembered exact scope sets.
- Direct Authentik MCP token rejection.
- Waltr token acceptance and exact scope/resource enforcement.
- Principal continuity across access/refresh rotation.
- Different Authentik subjects receiving MCP session `404` isolation.
- Immediate token revocation ending GET delivery.
- Restart preserving clients, consents, grants, refresh families, and signing overlap.
- Authentik outage after startup preserving local token validation.
- OAuth store or wrapping-key failure blocking readiness.
- Graceful shutdown joining every maintenance task.

Prefer a dedicated integration target such as `crates/waltr-server/tests/mcp_oauth_server.rs` instead of expanding unrelated access-auth cases indefinitely.

## Exit Criteria

- The browser/API access paths remain unchanged.
- `/mcp` advertises and accepts only Waltr-issued credentials.
- Authentik participates only through the existing Waltr browser session.
- Consent is Waltr-owned and exact to client, principal, resource, and scopes.
- Direct MCP OIDC verifier configuration and code are removed.
- Disabled-auth local development remains functional.
- Startup, readiness, background tasks, and shutdown are deterministic.

## Dependencies

Depends on [Authorization, Tokens, And Revocation](03-authorization-tokens.md) and [Client Registration](04-client-registration.md).
