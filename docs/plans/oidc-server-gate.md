# OIDC Server Gate Plan

## Status

Implemented by `71633dd`. This is the third historical plan in the persistence and authentication sequence. Use [Access Authentication](../architecture/access-authentication.md) for the current contract.

At this plan's completion, `/mcp` directly validated bearer tokens issued by the deployment's external OIDC provider. The later [MCP OAuth Authorization Server](mcp-oauth-authorization-server/README.md) work replaced that MCP-specific flow with Waltr-hosted authorization; references below to the configured OIDC issuer describe the original implementation, not current behavior.

## Objective

Gate Waltr's web application, gRPC APIs, and MCP server behind OIDC-backed authentication while treating every authenticated identity as the same effective Waltr principal.

This phase establishes authentication, not multi-user authorization. It validates identity, persists browser login sessions, and enforces access at server boundaries without partitioning Waltr data by user.

## Preimplementation Baseline

Before this plan was implemented, Waltr exposed health, web assets, gRPC and gRPC-web services, and `/mcp` without user authentication.

The existing `crates/waltr-server/src/auth/` code serves a different purpose: it acquires and stores credentials that Waltr uses with model providers and upstream MCP servers. Its `AuthTarget` values and OAuth attempts do not represent a user logging into Waltr.

The generic `mcp` crate already supports protected-resource metadata, bearer challenges, and an injected bearer-token validator. Waltr should configure those primitives rather than adding Waltr identity policy to the generic crate.

## Scope

- Add OIDC deployment configuration and discovery.
- Implement Authorization Code flow with state, nonce, and PKCE for browser login.
- Persist one-time login attempts and browser sessions in the Waltr database.
- Validate OIDC ID tokens received during browser callback.
- Validate bearer access tokens for native gRPC and MCP access.
- Add login, callback, logout, and session-status HTTP routes.
- Add a single server-owned authenticated-principal abstraction.
- Gate web, gRPC, gRPC-web, and MCP at router or service boundaries.
- Keep health and required authentication discovery routes public.
- Configure MCP OAuth protected-resource metadata and bearer challenges.
- Treat all valid authenticated principals identically.

## Authentication Versus Credential Authorization

Use distinct names and state for the two existing concerns:

| Concern | Meaning | Owner |
| --- | --- | --- |
| Waltr access authentication | A person or client is allowed to access this Waltr server. | New OIDC access module in `waltr-server`. |
| Provider/MCP credential authorization | Waltr needs credentials to call a configured model provider or upstream MCP server. | Existing `AuthService` and `AuthTarget` flow. |

An unauthenticated Waltr caller must not create an `AuthTarget`, open the provider OAuth dialog, or enter the existing provider/MCP authorization reducer path.

## Crate Boundary

Start with one server-owned access-authentication module rather than a new workspace crate. It should centralize discovery, token verification, session management, middleware, and the adapter used by Waltr's MCP router.

Creating an auth crate is warranted later only if the same implementation, rather than merely the same OIDC concepts, must be shared by independent crates. The server's confidential-client flow, browser cookie middleware, generic MCP validator adapter, and a native client's public-client flow have different runtime and security constraints; putting them in one crate prematurely would not remove meaningful duplication.

The existing provider/MCP `AuthService` can retain its public contract during this work. New module and type names must make the distinction explicit.

## Configuration

Add an explicit authentication mode so development and tests do not infer security policy from partially present variables.

| Setting | Purpose |
| --- | --- |
| `WALTR_AUTH_MODE` | `disabled` or `oidc`. |
| `WALTR_OIDC_ISSUER` | Expected issuer and discovery base URL. |
| `WALTR_OIDC_CLIENT_ID` | Browser authorization-code client ID. |
| `WALTR_OIDC_CLIENT_SECRET` | Confidential client secret when required by the provider. |
| `WALTR_OIDC_REDIRECT_URI` | Exact Waltr callback URL registered with the provider. |
| `WALTR_OIDC_AUDIENCE` | Required expected audience for access tokens; it is not inferred from the client ID. |
| `WALTR_OIDC_SCOPES` | Requested scopes, including `openid`. |
| `WALTR_AUTH_SESSION_TTL` | Maximum Waltr browser session lifetime. |

`disabled` should preserve current local behavior during rollout. `oidc` must fail startup when required configuration, discovery, or cryptographic validation setup is invalid. There must be no fallback from a failed `oidc` configuration to unauthenticated access.

Secrets must not appear in logs or status responses. Issuer, client ID, redirect URI, scopes, and audience should be visible in redacted startup diagnostics.

## OIDC Discovery And Validation

On startup, discover the provider metadata from the configured issuer and verify that required endpoints and signing algorithms are available. Cache JWKS with bounded refresh behavior so key rotation does not require a restart.

Browser callback validation must include:

- Exact state match against a persisted, unconsumed login attempt.
- Login attempt expiry and one-time consumption.
- Authorization response error handling.
- Token endpoint exchange using the persisted PKCE verifier.
- ID token signature, issuer, audience, expiry, and nonce validation.

Bearer validation must include:

- Signature and allowed algorithm.
- Issuer.
- Audience.
- Expiry and not-before constraints.
- Required scopes where a route declares them.

The initial bearer contract may require JWT access tokens verifiable through the issuer JWKS. Opaque-token introspection is a later extension and must not be emulated by treating an unverified token as authenticated.

## Persisted Data

OIDC data belongs to the Waltr schema introduced by [Waltr Server Persistence Foundation](waltr-server-persistence.md).

### Login Attempts

Persist enough state to complete and audit a browser authorization flow:

| Field | Purpose |
| --- | --- |
| State digest | CSRF correlation without storing the bearer state value directly. |
| Nonce | ID token replay protection. |
| PKCE verifier | Authorization-code exchange. |
| Return path | Safe same-origin destination after login. |
| Created and expiry times | Bounded attempt lifetime. |
| Consumed time | One-time-use enforcement. |

### Browser Sessions

Persist browser sessions using a random token whose digest, not plaintext value, is stored:

| Field | Purpose |
| --- | --- |
| Session token digest | Lookup and revocation. |
| Issuer and subject | Authentication evidence and diagnostics. |
| Created, expiry, and last-seen times | Session lifecycle. |
| Revoked time | Logout and administrative invalidation. |

No users table, ownership foreign key, role, or permission table is needed in this phase. Issuer and subject are evidence about the authenticated session, not Waltr data-partition keys.

Expired login attempts and sessions need a bounded cleanup path. Cleanup may run opportunistically during writes or as a small server task, but it must not be required for correctness.

## Browser Session Cookie

The browser callback will set a random opaque session cookie with these defaults:

- `HttpOnly`.
- `Secure` outside an explicit local-development mode.
- `SameSite=Lax` so the top-level OIDC callback can carry the cookie safely.
- `Path=/`.
- A bounded `Max-Age` no longer than the persisted session expiry.

Logout revokes the persisted session before clearing the cookie. A missing, expired, revoked, or unknown session is unauthenticated.

Return paths supplied to login must be restricted to local absolute paths. External return URLs and protocol-relative URLs must be rejected.

## Public And Protected Routes

Router construction in `crates/waltr-server/src/app.rs` will make the boundary visible.

Public routes:

- `/health`
- `/oidc/login`
- `/oidc/callback`
- `/oidc/logout`
- `/oidc/session`
- OIDC or OAuth protected-resource discovery routes required by MCP clients

Protected surfaces:

- Embedded web application assets and SPA routes
- gRPC and gRPC-web services
- `/mcp` POST, GET, and DELETE

Browser navigation to a protected web route may redirect to login. API, gRPC, and MCP requests must return protocol-appropriate unauthenticated responses rather than HTML redirects.

Enforcement belongs at the router or wrapped-service boundary. Individual config, catalog, session, status, and auth RPC methods should not each duplicate cookie or token validation.

## Authenticated Principal

Middleware will attach a server-owned authenticated principal to protected requests. The principal may retain issuer, subject, authentication method, and token scopes for diagnostics and future evolution.

Every valid principal has identical authorization in this phase. Services must not start filtering configuration, credentials, agent sessions, or MCP state by subject. This preserves the explicit single-effective-user behavior until a later authorization design introduces ownership.

## gRPC And gRPC-Web

Native gRPC requests authenticate with an `Authorization: Bearer` access token. Browser gRPC-web requests authenticate with the same-origin Waltr session cookie.

Unauthenticated gRPC requests return `Unauthenticated` without encoded provider/MCP `AuthTarget` details. This distinction allows the shared client to route the error to Waltr login rather than provider credential authorization.

CORS and credential settings must remain restrictive. Cross-origin browser deployment requires an explicit origin allowlist and credential configuration; it must not use wildcard origins with cookies.

## MCP

Waltr's MCP endpoint will use the generic `StreamableHttpAuthorization` contract.

The protected-resource metadata will identify the Waltr MCP resource, advertise the configured OIDC issuer as an authorization server, and list required scopes. Missing and invalid access tokens will return standards-compatible bearer challenges. Insufficient scopes will return the existing MCP `insufficient_scope` response.

MCP POST, GET, and DELETE require bearer access tokens. Browser session cookies are not an MCP authentication mechanism. Authentication must occur before the MCP session store is accessed, so unauthorized callers cannot probe, create, acknowledge, deliver, or delete persisted MCP sessions.

## Implementation Steps

1. Add and validate explicit auth-mode and OIDC configuration.
2. Implement provider discovery, JWKS caching, and token verifier tests.
3. Add backend-specific Waltr migrations for login attempts and browser sessions.
4. Implement domain-specific login-attempt and browser-session stores.
5. Implement login, callback, logout, and session-status handlers.
6. Add principal extraction and route/service authentication layers.
7. Partition public and protected routes in `app.rs`.
8. Adapt the verifier to MCP's bearer validator and configure protected-resource metadata.
9. Add server contract and security regression tests.
10. Document deployment settings and login behavior.

## Tests

Follow the server and MCP layering in [testing.md](../testing.md).

OIDC unit tests must cover:

- Discovery validation.
- JWKS cache refresh and key rotation.
- Valid and invalid issuer, audience, signature, expiry, nonce, and scope.
- State mismatch, expiry, replay, and concurrent callback attempts.
- PKCE verifier use during token exchange.
- Return-path validation.
- Cookie construction and clearing.

Persistence tests must cover:

- Login attempts surviving app reconstruction.
- Exactly one callback consuming an attempt.
- Session lookup by token digest.
- Expired and revoked sessions failing authentication.
- Cleanup not removing active sessions.
- Equivalent SQLite and PostgreSQL behavior.

Router tests must cover:

- Health and required discovery endpoints remaining public.
- Protected web navigation entering login behavior.
- Unauthenticated gRPC and gRPC-web returning `Unauthenticated` without `AuthTarget` details.
- Valid cookie authentication for browser gRPC-web.
- Valid bearer authentication for native gRPC.
- Invalid credentials never reaching protected service handlers.

MCP tests must cover:

- Protected-resource metadata contents.
- Missing, invalid, expired, and insufficient-scope tokens.
- Valid bearer access to Waltr MCP methods.
- Unauthorized POST, GET, and DELETE not touching session persistence.
- Authenticated MCP sessions surviving server restart.

## Acceptance Criteria

- `WALTR_AUTH_MODE=oidc` fails closed when configuration or discovery is invalid.
- Browser login uses state, nonce, PKCE, validated ID tokens, and a persisted opaque session cookie.
- Native gRPC and MCP accept only validated bearer access tokens.
- All protected server surfaces consistently reject unauthenticated access.
- Health and required login/discovery routes remain usable before authentication.
- Unauthenticated Waltr access cannot trigger provider/MCP credential OAuth UI.
- Every authenticated identity receives the same Waltr access, with no user partitioning.
- OIDC login attempts and browser sessions work with SQLite and PostgreSQL.

## Phase-Local Non-Goals

These items were outside the server-gate plan. Native/TUI acquisition was implemented by the following plan; the other items remain separate design work.

- Multiple Waltr users, authorization roles, ownership, or tenant isolation.
- Per-user provider credentials or agent sessions.
- Opaque access-token introspection in the initial implementation.
- Acting as an OAuth authorization server.
- Dynamic client registration for MCP clients.
- Native/TUI interaction design and token acquisition, which belong to the next plan.
- Refactoring all provider and upstream MCP credential code into a new auth crate.

## Dependencies

This plan depends on [Waltr Server Persistence Foundation](waltr-server-persistence.md) and uses the store from [MCP Streamable HTTP Session Persistence](mcp-session-persistence.md). It provides the server contract consumed by [OIDC Client And UI Flows](oidc-client-ui-flows.md).

## References

- [`crates/waltr-server/src/app.rs`](../../crates/waltr-server/src/app.rs)
- [`crates/waltr-server/src/auth/`](../../crates/waltr-server/src/auth)
- [`crates/waltr-server/src/mcp_service.rs`](../../crates/waltr-server/src/mcp_service.rs)
- [`crates/mcp/src/oauth.rs`](../../crates/mcp/src/oauth.rs)
- [`crates/mcp/src/server/mod.rs`](../../crates/mcp/src/server/mod.rs)
- [Testing](../testing.md)
