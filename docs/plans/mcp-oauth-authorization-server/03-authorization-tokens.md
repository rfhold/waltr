# 03 Authorization, Tokens, And Revocation

Status: Complete

The authorization, token, refresh, revocation, and lease implementation is complete. Construction enforces signing overlap against the configured access-token lifetime and clock skew, Waltr derives that overlap from its accepted token lifetime, and rotation tests preserve one-hour unexpired tokens.

## Objective

Build the generic MCP OAuth authorization server over the protocol and persistence contracts, including Authorization Code with PKCE, consent hooks, access and refresh token issuance, revocation, signing-key rotation, and active Streamable HTTP authorization leases.

## Server API

Add public composition types under `mcp::oauth::server`:

```rust
pub struct OAuthAuthorizationServer;
pub struct OAuthAuthorizationServerConfig;
pub struct OAuthResource;
pub trait OAuthResourceOwnerAuthenticator;
pub trait OAuthConsentHandler;
```

The configuration declares:

- Exact issuer.
- Authorization, token, revocation, registration, and JWKS endpoints.
- Supported resources and scopes.
- Authorization transaction and code lifetimes.
- Access-token and refresh-family lifetimes.
- Signing rotation and clock-skew policy.
- Client, transaction, grant, and token quotas.

Construction validates all URLs and invariants before returning a router or service handle.

## Routes

For an issuer `https://waltr.example/oauth`, serve:

```text
GET  /.well-known/oauth-authorization-server/oauth
GET  /oauth/jwks.json
GET  /oauth/authorize
POST /oauth/authorize/decision
POST /oauth/token
POST /oauth/revoke
```

Registration is added by the following workstream. Do not publish OpenID Provider metadata because the hosted service delegates identity authentication and is not itself an OpenID Provider.

Every OAuth response uses explicit cache controls. Token and error responses use `Cache-Control: no-store` and `Pragma: no-cache`. Authorization errors redirect only after the client and redirect URI have been validated; otherwise the server returns a local sanitized error.

## Resource-Owner Authentication Hook

The generic server must not know cookies or Authentik. It asks an injected adapter to resolve an HTTP request into one of these outcomes:

- Authenticated stable `McpPrincipalId`.
- Login required with a safe integrating-server continuation.
- Authentication denied.
- Authentication infrastructure unavailable.

The generic service persists an opaque transaction ID and browser-correlation digest before requesting login. The integrating server places the random correlation value in a secure, HTTP-only, same-site cookie and resumes the transaction only when both values match. Resumption consumes the correlation once, rejects fixation and replay, and applies a no-referrer policy. Raw authorization parameters are not round-tripped through an untrusted `return_to` query.

## Consent Hook

The generic service supplies a sanitized consent model containing:

- Client identifier and untrusted display name.
- Exact resource.
- Requested scopes.
- Already granted scopes.
- Opaque transaction identifier.

The integrating handler renders and submits the decision. The generic service validates CSRF evidence through an adapter contract, records approval or denial, and controls the OAuth redirect.

Consent grants are exact to principal, client, resource, and canonical scope set. Existing consent skips presentation only for the same scope set. A narrowed, expanded, or otherwise different set requires a new decision.

## Authorization Code And PKCE

Requirements:

- `response_type=code` only.
- `code_challenge_method=S256` required for every client.
- Exact registered redirect URI.
- Exact configured MCP resource through RFC 8707 `resource`.
- Requested scopes limited to the resource's supported scopes.
- Random, short-lived, opaque authorization codes.
- Code digest stored instead of plaintext.
- Code bound to client, redirect, principal, resource, scopes, and PKCE challenge.
- Atomic one-time redemption.
- Constant-time PKCE comparison.

Authorization codes default to five minutes. A consumed, expired, wrong-client, wrong-redirect, wrong-resource, or wrong-verifier code produces the same standards-appropriate invalid-grant response without revealing which check failed.

The token endpoint may inspect an unconsumed code to prepare signing, but the store performs all final checks in one conditional redeem-and-issue transition. It atomically consumes the code and persists the grant, `jti`, refresh family, and refresh-token digest. Signing failure leaves the code unconsumed; transaction failure discards the prepared JWT and returns no token.

## Access Tokens

Issue ES256 JWT access tokens following RFC 9068:

| Claim | Value |
| --- | --- |
| `iss` | Exact Waltr OAuth issuer. |
| `sub` | Stable opaque `McpPrincipalId`. |
| `aud` | Exact MCP resource URL. |
| `client_id` | Authorized downstream client. |
| `scope` | Canonical granted scopes. |
| `iat` | Issue time. |
| `nbf` | Not-before time under configured skew policy. |
| `exp` | Short access-token expiry. |
| `jti` | Random unique identifier persisted for revocation. |

The JOSE header requires `alg=ES256`, a known `kid`, and `typ=at+jwt`. The default access-token lifetime is five minutes.

JWT validation includes cryptographic checks and an authorization-store check for token, grant, client, and the token's explicitly linked refresh-family state. Signed tokens are not treated as irrevocable stateless credentials.

## Refresh Tokens

Refresh tokens are random opaque values with at least 256 bits of entropy. Persist only their digests.

Every successful refresh:

1. Validates client, family, grant, resource, scopes, expiry, and presented generation.
2. Marks the presented generation consumed.
3. Issues a new access token.
4. Issues a replacement refresh token.
5. Commits all changes atomically.

Scopes may remain equal or narrow and must never expand during refresh. Refresh families have both per-token expiry and configurable absolute lifetime.

Presenting a consumed refresh token is replay. The replay transaction revokes the entire family, its active access tokens, and its authorization leases. A concurrent legitimate refresh therefore succeeds once and causes every stale concurrent use to fail closed.

## Revocation

Implement RFC 7009 semantics:

- Accept access or refresh token hints.
- Return success for unknown tokens.
- Bind public-client revocation to the recorded client identifier.
- Revoke a refresh token as a family operation.
- Revoke associated access-token records.
- Publish revocation only after the database transaction commits.
- Never return whether a token existed.

Administrative service methods should also support revoking a grant or client without exposing bearer token values.

## Streamable HTTP Authorization Lease

Preserve the current external-validator constructor. Add an authorization path that can return:

```rust
pub struct McpTokenAuthorization {
    pub principal_id: McpPrincipalId,
    pub expires_at: Option<SystemTime>,
    pub revocation: Option<McpRevocationLease>,
}
```

`McpRevocationLease` resolves when the access token, grant, family, or client becomes invalid.

Stream behavior:

- POST and DELETE check current authorization before touching session persistence.
- GET delivery races source frames against token expiry and lease revocation.
- Same-process revocation signals immediately after commit.
- PostgreSQL instances propagate committed revocation through `LISTEN/NOTIFY` or an equivalent store event channel.
- A bounded periodic authoritative store check covers missed notifications and reconnects.
- Store failure while revalidating terminates delivery as unavailable rather than continuing with stale authorization.

Document the maximum cross-instance termination bound and test it. Reuse the existing guarded-stream patterns rather than spawning one detached task per response.

## Background Lifecycle

The authorization server exposes bounded maintenance operations for the integrating host to schedule:

- Expired transaction and code cleanup.
- Expired token and revoked-family cleanup.
- Signing-key candidate generation and activation.
- Retired-key cleanup after verification overlap.
- Wrapping-key re-encryption.
- Revocation event listener health.

Correctness cannot depend on cleanup running on time. Every read validates expiry and state.

## Tests

Add full HTTP-flow tests covering:

- Authorization metadata and JWKS.
- Login-required and authenticated hooks.
- Consent approval, denial, and remembered grants.
- Invalid client and redirect behavior without attacker redirects.
- PKCE omission, `plain`, mismatch, and valid S256.
- Exact resource and scope binding.
- Code expiry, replay, and concurrent redemption.
- JWT claims, header policy, wrong audience, wrong key, and retired-key overlap.
- Refresh rotation, narrowing, expiry, and replay-family revocation.
- RFC 7009 idempotence.
- Token, grant, client, and family revocation.
- Immediate same-process stream termination.
- Bounded cross-process stream termination.
- Store failure terminating streams as unavailable.
- Existing external-validator resource-server behavior remaining compatible.

## Exit Criteria

- A preregistered public client can complete Authorization Code with PKCE and call a protected MCP router using the issued token.
- Refresh rotation and replay behavior are atomic across every store backend.
- Revocation ends active GET delivery within the documented bound.
- Signing-key rotation preserves validation of unexpired tokens.
- The generic server needs only abstract principal, consent, store, clock, entropy, and key-encryption dependencies.

## Dependencies

Depends on [OAuth Protocol And Discovery](01-protocol-discovery.md) and [OAuth Persistence And Signing Keys](02-persistence-signing.md). Enables registration and Waltr integration.
